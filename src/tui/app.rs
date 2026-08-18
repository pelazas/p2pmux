//! The three blocking entry points: a purely local terminal, the legacy
//! single-pane host, and a guest attached to someone else's pane.

use std::{
    error::Error,
    io,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, SetTitle, enable_raw_mode},
};
use portable_pty::PtySize;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};

use ratatui::style::Color;

use crate::{
    config::UiTheme,
    kitty_keyboard::KittyKeyboardTracker,
    lease::{IDLE_AFTER, LeaseDecision},
    pty_host::PtyHost,
    screen::{GuestScreen, OuterResetRecognizer, SyncGate},
    session::{GuestEvent, GuestPane, HostControlEvent},
    tui::{
        HostPaneRuntime,
        input::{
            events::{
                MAX_EVENTS_PER_CYCLE, begin_synchronized_output, collect_pending_events,
                end_synchronized_output, event_poll_timeout, frame_due,
            },
            keys::{encode_key, encode_paste, is_quit},
        },
        pane::remote::{
            RemoteInput, lease_allows_held_input, reconcile_remote_control_attempt,
            remote_input_decision,
        },
        render::vt::{VtScreen, render_guest_screen, render_host_screen},
        terminal::{TerminalGuard, clear_before_first_frame, enable_keyboard_enhancement},
    },
};

/// The legacy fixed-grid host/guest footer, which has no chords, agents, or share modal.
const CONTROL_HELP: &str = "Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS";
/// Run one local shell in a PTY whose dimensions never change after startup.
pub fn run_local() -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut host = PtyHost::spawn_default_shell(size)?;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let mut kitty_keyboard = KittyKeyboardTracker::default();
    let mut outer_reset = OuterResetRecognizer::default();

    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    execute!(io::stdout(), SetTitle("p2pmux"))?;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;

    let backend = CrosstermBackend::new(io::stdout());
    let fixed_area = Rect::new(0, 0, cols, rows);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(fixed_area),
        },
    )?;
    clear_before_first_frame(&mut terminal, fixed_area)?;
    let mut dirty = true;
    let mut reset_outer_pending = false;
    let mut last_draw: Option<Instant> = None;
    let mut sync_gate = SyncGate::default();

    loop {
        let drain_started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        let ready = if pending.is_empty() {
            sync_gate.flush_stale(Instant::now())
        } else {
            sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            reset_outer_pending |= outer_reset.feed(&ready);
            kitty_keyboard.observe(&ready);
            parser.process(&ready);
            if let Some(reply) = kitty_keyboard.take_query_reply() {
                host.write_input(&reply)?;
            }
            dirty = true;
        }
        if host.output_closed() {
            break;
        }

        if dirty && frame_due(last_draw) {
            if reset_outer_pending {
                clear_before_first_frame(&mut terminal, fixed_area)?;
                reset_outer_pending = false;
            }
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                let screen = parser.screen();
                let area = frame.area();
                frame.render_widget(VtScreen::new(screen), area);
                let (row, col) = screen.cursor_position();
                if !screen.hide_cursor() && row < area.height && col < area.width {
                    frame.set_cursor_position((area.x + col, area.y + row));
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }

        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if is_quit(key) {
                        quit = true;
                        break;
                    }
                    if let Some(bytes) = encode_key(key, parser.screen(), kitty_keyboard.active()) {
                        host.write_input(&bytes)?;
                    }
                }
                Event::Paste(text) => {
                    let bytes = encode_paste(&text, parser.screen().bracketed_paste());
                    host.write_input(&bytes)?;
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }

    Ok(())
}
/// Run the one fixed-grid host PTY and keep all peer work outside its drain loop.
pub fn run_host(mut runtime: HostPaneRuntime) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    clear_before_first_frame(&mut terminal, Rect::new(0, 0, cols, rows))?;
    let footer = CONTROL_HELP.to_owned();
    let mut dirty = true;
    let mut reset_outer_pending = false;
    let mut last_draw: Option<Instant> = None;
    let mut sync_gate = SyncGate::default();
    loop {
        if let Some(state) = runtime.lease.clear_if_idle(Instant::now())? {
            runtime.lease_tx.send_replace(state);
        }
        while let Ok(event) = runtime.control_rx.try_recv() {
            match event {
                HostControlEvent::Input { peer_id, input } => match runtime.lease.input(
                    &peer_id,
                    input.lease_epoch,
                    input.data,
                    Instant::now(),
                ) {
                    LeaseDecision::AcceptInput(bytes) => {
                        runtime.host.write_input(&bytes)?;
                        runtime.lease_tx.send_replace(runtime.lease.state().clone());
                    }
                    LeaseDecision::Publish(_)
                    | LeaseDecision::RejectStaleInput
                    | LeaseDecision::RejectStaleRequest
                    | LeaseDecision::RejectActiveController => {}
                },
                HostControlEvent::TakeControl { peer_id, request } => {
                    let decision = runtime.lease.take_control(
                        peer_id,
                        request.known_lease_epoch,
                        Instant::now(),
                    )?;
                    match decision {
                        LeaseDecision::Publish(state) => {
                            runtime.lease_tx.send_replace(state);
                        }
                        LeaseDecision::RejectActiveController => {
                            runtime.lease_tx.send_replace(runtime.lease.state().clone());
                        }
                        LeaseDecision::AcceptInput(_)
                        | LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest => {}
                    }
                }
                HostControlEvent::ReleaseControl { peer_id } => {
                    if runtime.lease.state().controller_peer_id == peer_id
                        && let Some(state) = runtime.lease.clear_controller(Instant::now())?
                    {
                        runtime.lease_tx.send_replace(state);
                    }
                }
            }
        }
        let drain_started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = runtime.host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        let ready = if pending.is_empty() {
            sync_gate.flush_stale(Instant::now())
        } else {
            sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            if let Ok(frame) = runtime.screen.process_pty(&ready) {
                reset_outer_pending |= frame.reset_outer;
                if let Some(reply) = runtime.screen.take_kitty_keyboard_query_reply() {
                    runtime.host.write_input(&reply)?;
                }
                runtime.screen_tx.send_replace(frame);
            }
            dirty = true;
        }
        if runtime.host.output_closed() {
            break;
        }
        if dirty && frame_due(last_draw) {
            if reset_outer_pending {
                clear_before_first_frame(&mut terminal, Rect::new(0, 0, cols, rows))?;
                reset_outer_pending = false;
            }
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                let screen = runtime.screen.screen();
                render_host_screen(frame, screen, &footer);
                let (row, col) = screen.cursor_position();
                let screen_height = screen.size().0.min(frame.area().height.saturating_sub(1));
                if !screen.hide_cursor() && row < screen_height && col < frame.area().width {
                    frame.set_cursor_position((frame.area().x + col, frame.area().y + row));
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }
        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if is_quit(key) {
                        quit = true;
                        break;
                    }
                    if let Some(bytes) = encode_key(
                        key,
                        runtime.screen.screen(),
                        runtime.screen.kitty_keyboard_active(),
                    ) {
                        let now = Instant::now();
                        let epoch = runtime.lease.state().epoch;
                        let decision =
                            runtime
                                .lease
                                .input(&runtime.host_peer_id, epoch, bytes, now);
                        match decision {
                            LeaseDecision::AcceptInput(bytes) => {
                                runtime.host.write_input(&bytes)?;
                                runtime.lease_tx.send_replace(runtime.lease.state().clone());
                            }
                            LeaseDecision::Publish(_) => {}
                            LeaseDecision::RejectStaleInput
                            | LeaseDecision::RejectStaleRequest
                            | LeaseDecision::RejectActiveController => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    let bytes = encode_paste(&text, runtime.screen.screen().bracketed_paste());
                    let now = Instant::now();
                    let epoch = runtime.lease.state().epoch;
                    let decision = runtime
                        .lease
                        .input(&runtime.host_peer_id, epoch, bytes, now);
                    match decision {
                        LeaseDecision::AcceptInput(bytes) => {
                            runtime.host.write_input(&bytes)?;
                            runtime.lease_tx.send_replace(runtime.lease.state().clone());
                        }
                        LeaseDecision::Publish(_) => {}
                        LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest
                        | LeaseDecision::RejectActiveController => {}
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }
    Ok(())
}
/// Render one remote, immutable terminal grid. Input forwarding arrives in milestone 12.
pub fn run_guest(mut pane: GuestPane) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    clear_before_first_frame(&mut terminal, Rect::new(0, 0, cols, rows))?;
    let mut remote = GuestScreen::new();
    let mut footer = String::from("controller: waiting spectator");
    let mut lease = None;
    let mut last_lease = Instant::now();
    let mut pending_control = false;
    let mut held_input = Vec::new();
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;

    loop {
        let mut received_lease = false;
        loop {
            match pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    if remote
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok()
                    {
                        remote.set_kitty_keyboard_active(snapshot.kitty_keyboard_active);
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    if remote
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok()
                    {
                        remote.set_kitty_keyboard_active(delta.kitty_keyboard_active);
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenGap { .. }) => {}
                Ok(GuestEvent::Lease(state)) => {
                    received_lease = true;
                    footer = format!(
                        "controller: {} typing",
                        short_peer(&state.controller_peer_id)
                    );
                    last_lease = Instant::now();
                    pending_control = false;
                    lease = Some(state);
                    dirty = true;
                }
                Ok(GuestEvent::Disconnected)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "host session disconnected",
                    )
                    .into());
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            }
        }

        if received_lease && let Some(state) = lease.as_ref() {
            reconcile_remote_control_attempt(
                &mut pending_control,
                &mut held_input,
                &state.controller_peer_id,
                pane.controls.peer_id(),
            );
        }

        if !pending_control
            && !held_input.is_empty()
            && let Some(state) = lease.as_ref()
            && lease_allows_held_input(&state.controller_peer_id, pane.controls.peer_id())
        {
            let bytes = std::mem::take(&mut held_input);
            if pane
                .controls
                .try_input(state.lease_epoch, bytes.clone())
                .is_err()
            {
                held_input = bytes;
            }
        }

        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                if let Some(screen) = remote.screen() {
                    render_guest_screen(frame, screen, &footer);
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }

        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && is_quit(key) =>
                {
                    quit = true;
                    break;
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen())
                        && let Some(bytes) = encode_key(key, screen, remote.kitty_keyboard_active())
                    {
                        let claiming_free_pane = state.controller_peer_id.is_empty();
                        match remote_input_decision(
                            &state.controller_peer_id,
                            pane.controls.peer_id(),
                            pending_control,
                            held_input.is_empty(),
                            last_lease.elapsed() >= IDLE_AFTER,
                        ) {
                            RemoteInput::Send => {
                                if pane.controls.try_input(state.lease_epoch, bytes).is_ok()
                                    && claiming_free_pane
                                {
                                    pending_control = true;
                                }
                            }
                            RemoteInput::Hold => held_input.extend_from_slice(&bytes),
                            RemoteInput::Request => {
                                held_input.extend_from_slice(&bytes);
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                            RemoteInput::Ignore => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen()) {
                        let bytes = encode_paste(&text, screen.bracketed_paste());
                        let claiming_free_pane = state.controller_peer_id.is_empty();
                        match remote_input_decision(
                            &state.controller_peer_id,
                            pane.controls.peer_id(),
                            pending_control,
                            held_input.is_empty(),
                            last_lease.elapsed() >= IDLE_AFTER,
                        ) {
                            RemoteInput::Send => {
                                if pane.controls.try_input(state.lease_epoch, bytes).is_ok()
                                    && claiming_free_pane
                                {
                                    pending_control = true;
                                }
                            }
                            RemoteInput::Hold => held_input.extend_from_slice(&bytes),
                            RemoteInput::Request => {
                                held_input.extend_from_slice(&bytes);
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                            RemoteInput::Ignore => {}
                        }
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }
    Ok(())
}
pub(in crate::tui) fn short_peer(peer_id: &[u8]) -> String {
    peer_id
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub(in crate::tui) fn member_label(peer_id: &[u8], members: &[crate::layout::Member]) -> String {
    let Some(member) = members.iter().find(|member| member.peer_id == peer_id) else {
        return short_peer(peer_id);
    };
    if member.display_name.is_empty() {
        return short_peer(peer_id);
    }
    // A peer id is per *process*, so one machine that has run p2pmux twice is
    // two members — and disambiguating on the display name alone printed it as
    // two machines, `droplet · b3cf62f0` and `droplet · 8fe47d3f`, one of them
    // usually a node whose process is already gone. Aiming a remote terminal at
    // the wrong one of those is silence, not an error.
    //
    // So the suffix is for telling two *machines* apart, and the proved machine
    // id is what says whether there are two. Members that could not prove one
    // fall back to their peer id, which makes each of them its own machine —
    // the honest answer when nothing links them.
    let identity = |member: &crate::layout::Member| {
        if member.machine_id.is_empty() {
            member.peer_id.clone()
        } else {
            member.machine_id.clone()
        }
    };
    let mine = identity(member);
    let other_machines = members
        .iter()
        .filter(|candidate| candidate.display_name == member.display_name)
        .any(|candidate| identity(candidate) != mine);
    if other_machines {
        format!("{} · {}", member.display_name, short_peer(peer_id))
    } else {
        member.display_name.clone()
    }
}

/// The color identifying a member everywhere presence is drawn.
///
/// The slot is the member's position in the authoritative member list, which every
/// client receives at the same revision, so all of them agree on who is which color
/// without a wire field to carry it. The cost is that slots shift when a member
/// leaves; the initial from [`member_initial`] rides alongside every color so an
/// identity is only ever recolored, never lost.
pub fn member_color(
    peer_id: &[u8],
    members: &[crate::layout::Member],
    theme: &UiTheme,
) -> Option<Color> {
    let slot = members
        .iter()
        .position(|member| member.peer_id == peer_id)?;
    Some(theme.member_colors[slot % theme.member_colors.len()])
}

/// The one-character stand-in for a member, for terminals and eyes that cannot separate
/// eight hues. Falls back to the peer id when a display name is missing or unprintable.
pub fn member_initial(peer_id: &[u8], members: &[crate::layout::Member]) -> char {
    members
        .iter()
        .find(|member| member.peer_id == peer_id)
        .and_then(|member| {
            member
                .display_name
                .chars()
                .find(|character| character.is_alphanumeric())
        })
        .or_else(|| short_peer(peer_id).chars().next())
        .map(|character| character.to_ascii_uppercase())
        .unwrap_or('?')
}

#[cfg(test)]
mod tests {
    use crate::{config::UiTheme, tui::test_support::presence_members};

    use super::{member_color, member_initial, member_label};

    #[test]
    fn member_labels_disambiguate_duplicate_display_names() {
        let members = vec![
            crate::layout::Member {
                peer_id: vec![0xaa, 0xbb, 0xcc, 0xdd],
                endpoint_addr: vec![1],
                display_name: "sam".into(),
                kind: Default::default(),
                machine_proof: Default::default(),
                machine_id: Default::default(),
            },
            crate::layout::Member {
                peer_id: vec![0x11, 0x22, 0x33, 0x44],
                endpoint_addr: vec![2],
                display_name: "sam".into(),
                kind: Default::default(),
                machine_proof: Default::default(),
                machine_id: Default::default(),
            },
            crate::layout::Member {
                peer_id: vec![0x55, 0x66, 0x77, 0x88],
                endpoint_addr: vec![3],
                display_name: "pat".into(),
                kind: Default::default(),
                machine_proof: Default::default(),
                machine_id: Default::default(),
            },
        ];

        assert_eq!(
            member_label(&members[0].peer_id, &members),
            "sam · aabbccdd"
        );
        assert_eq!(member_label(&members[2].peer_id, &members), "pat");
    }

    /// One machine that has run p2pmux twice is two members, and it used to be
    /// drawn as two machines — usually with one of them a node whose process
    /// had already gone. Aiming a remote terminal at that one is silence.
    #[test]
    fn two_processes_on_one_machine_are_one_machine() {
        let machine = vec![0xde, 0xad, 0xbe, 0xef];
        let member = |peer: Vec<u8>, machine_id: Vec<u8>, name: &str| crate::layout::Member {
            peer_id: peer,
            endpoint_addr: vec![1],
            display_name: name.into(),
            kind: Default::default(),
            machine_proof: Default::default(),
            machine_id,
        };
        let members = vec![
            member(vec![0xaa; 4], machine.clone(), "droplet"),
            member(vec![0xbb; 4], machine.clone(), "droplet"),
        ];

        assert_eq!(member_label(&members[0].peer_id, &members), "droplet");
        assert_eq!(
            member_label(&members[1].peer_id, &members),
            "droplet",
            "the same box under both of its peer ids"
        );

        // Two boxes that chose the same name are still two, and still have to
        // be told apart.
        let two_boxes = vec![
            member(vec![0xaa; 4], machine, "droplet"),
            member(vec![0xbb; 4], vec![0x01, 0x02, 0x03, 0x04], "droplet"),
        ];
        assert_eq!(
            member_label(&two_boxes[0].peer_id, &two_boxes),
            "droplet · aaaaaaaa"
        );
    }

    #[test]
    fn member_colors_are_distinct_per_slot_and_agree_across_clients() {
        let theme = UiTheme::default();
        let members = presence_members(crate::config::MEMBER_COLOR_SLOTS);

        let colors = members
            .iter()
            .map(|member| member_color(&member.peer_id, &members, &theme))
            .collect::<Option<Vec<_>>>()
            .expect("every member has a color");
        for (slot, color) in colors.iter().enumerate() {
            assert!(
                !colors[..slot].contains(color),
                "a full session must never show two members the same color"
            );
        }

        // A second client holding the same authoritative member list derives the same
        // colors: that agreement is what lets presence ship without a wire color field.
        let mirrored = members.clone();
        for member in &members {
            assert_eq!(
                member_color(&member.peer_id, &members, &theme),
                member_color(&member.peer_id, &mirrored, &theme)
            );
        }
    }
    #[test]
    fn member_colors_avoid_the_reserved_control_and_alert_colors() {
        let theme = UiTheme::default();
        for color in theme.member_colors {
            // A held pane borrows its controller's color, so this one is the *fallback*
            // for a controller with no slot left -- someone who dropped out mid-hold. It
            // has to stay outside the palette, or that pane would claim a live member's
            // identity.
            assert_ne!(
                color, theme.pane_border_remote_control,
                "the departed-controller border must not read as a member's own color"
            );
            assert_ne!(color, theme.tab_active_background);
            assert_ne!(color, theme.footer_accent);
            // Slot one is warm by design, so this one is worth stating outright: a
            // chord-armed border must never read as the member holding that pane.
            assert_ne!(color, theme.pane_border_chord_focused);
        }
    }
    #[test]
    fn member_color_is_none_for_a_departed_peer() {
        let theme = UiTheme::default();
        let members = presence_members(2);

        assert_eq!(
            member_color(&[0xff, 0xff, 0xff, 0xff], &members, &theme),
            None
        );
    }
    #[test]
    fn member_initials_fall_back_to_the_peer_id() {
        let mut members = presence_members(2);
        members[1].display_name = "  ".into();

        assert_eq!(member_initial(&members[0].peer_id, &members), 'M');
        assert_eq!(member_initial(&members[1].peer_id, &members), '0');
        assert_eq!(member_initial(&[0x9a], &members), '9');
    }
}
