//! Centred overlay panels: the share invite, the rename prompt, the delete-tab
//! confirmation, and the quit prompt.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

use crate::{
    config::UiTheme,
    tui::{
        RenamePrompt, RenameTarget, ShareView,
        render::footer::{
            ADD_MACHINE_HELP, SHARE_HELP, SHARE_HELP_GUEST, SHARE_HELP_NO_CODE,
            render_footer_segments,
        },
        share::{join_command, pair_command},
        text::{truncate_trailing, wrap_fixed},
    },
};

/// The invite panel behind Ctrl+S.
///
/// One join line, shown as the command the guest actually types. The code and the ticket are
/// not two invites to choose between — the code *resolves to* the ticket, so offering both
/// with equal billing only asks the host to make a decision that has no wrong answer. The
/// ticket therefore stays one keypress away rather than on screen, and is rendered only when
/// it is the invite that has to travel: a rendezvous outage leaves no code to send.
pub(in crate::tui) fn render_share_modal(
    frame: &mut Frame<'_>,
    theme: &UiTheme,
    share: ShareView<'_>,
) {
    let area = frame.area();
    let label = Style::default().fg(theme.agent_overlay_muted);
    let value = Style::default()
        .fg(theme.agent_overlay_foreground)
        .add_modifier(Modifier::BOLD);

    let width = area.width.saturating_sub(4).clamp(28, 72).min(area.width);
    let content_width = usize::from(width.saturating_sub(4)).max(1);
    let mut lines: Vec<Line> = Vec::new();
    match (share.ticket, share.code) {
        (Some(_), Some(code)) => {
            lines.push(Line::styled("Have them run:", label));
            lines.push(Line::raw(""));
            lines.extend(
                wrap_fixed(&join_command(code), content_width)
                    .into_iter()
                    .map(|chunk| Line::styled(chunk, value)),
            );
            lines.push(Line::raw(""));
            lines.push(Line::styled("Expires in 6h.", label));
            lines.push(Line::styled(
                "Anyone who runs it gets a full shell here.",
                Style::default().fg(theme.agent_overlay_warm),
            ));
        }
        // No code means the rendezvous was unreachable when the session started. The ticket
        // is then the only invite, so it earns the space the code would have had.
        (Some(ticket), None) => {
            lines.push(Line::styled(
                "No short code — rendezvous unreachable.",
                Style::default().fg(theme.agent_overlay_warm),
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled("Have them run:", label));
            lines.push(Line::raw(""));
            lines.extend(
                wrap_fixed(&join_command(ticket), content_width)
                    .into_iter()
                    .map(|chunk| Line::styled(chunk, value)),
            );
            lines.push(Line::raw(""));
            lines.push(Line::styled("Never expires.", label));
            lines.push(Line::styled(
                "Anyone who runs it gets a full shell here.",
                Style::default().fg(theme.agent_overlay_warm),
            ));
        }
        (None, _) => lines.push(Line::styled(
            "Only the host can share this session.",
            Style::default().fg(theme.agent_overlay_muted),
        )),
    }
    if let Some(notice) = share.notice {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            notice.to_owned(),
            Style::default().fg(theme.copy_feedback_accent),
        ));
    }

    // Two border rows, a leading blank, the body, a blank, then the help row.
    let height = u16::try_from(lines.len().saturating_add(5))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    let block = Block::bordered()
        .title(Line::styled(
            " Share this session ",
            Style::default()
                .fg(theme.agent_overlay_chrome)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.agent_overlay_chrome));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let body = Rect::new(
        inner.x.saturating_add(1),
        inner.y.saturating_add(1),
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(3).max(1),
    );
    frame.render_widget(Paragraph::new(lines), body);

    let help = Rect::new(
        inner.x,
        inner.y.saturating_add(inner.height.saturating_sub(1)),
        inner.width,
        1,
    );
    let buffer = frame.buffer_mut();
    buffer.set_stringn(
        help.x,
        help.y,
        " ".repeat(usize::from(help.width)),
        usize::from(help.width),
        Style::default().bg(theme.footer_background),
    );
    render_footer_segments(
        buffer,
        theme,
        help.x.saturating_add(1),
        help.y,
        help.right(),
        match (share.ticket.is_some(), share.code.is_some()) {
            (true, true) => SHARE_HELP,
            (true, false) => SHARE_HELP_NO_CODE,
            (false, _) => SHARE_HELP_GUEST,
        },
    );
}
/// The install line for a machine that has never had p2pmux on it.
///
/// The case the CLI flow never covered: `p2pmux pair <code>` is useless advice
/// on a machine with no p2pmux, and "add a machine" means a new one far more
/// often than it means one already set up.
const INSTALL_COMMAND: &str = "curl -fsSL https://p2pmux.com/install.sh | sh";

/// The panel behind `a` on the inbox: the one line to run on the machine being
/// added, and then the machine arriving.
///
/// It stays up after the code is shown because the second half of adding a
/// machine — finding out whether it worked — used to mean running something
/// else somewhere else. Here it is the same panel, which is what makes this
/// feel like adding a machine rather than like reading a manual.
pub(in crate::tui) fn render_add_machine_modal(
    frame: &mut Frame<'_>,
    theme: &UiTheme,
    share: ShareView<'_>,
    joined: Option<&str>,
    animation_phase: usize,
) {
    let area = frame.area();
    let label = Style::default().fg(theme.agent_overlay_muted);
    let value = Style::default()
        .fg(theme.agent_overlay_foreground)
        .add_modifier(Modifier::BOLD);

    let width = area.width.saturating_sub(4).clamp(32, 64).min(area.width);
    let content_width = usize::from(width.saturating_sub(4)).max(1);
    let mut lines: Vec<Line> = Vec::new();
    // The code resolves to the ticket, so it is the invite when there is one
    // and the ticket stands in when the rendezvous service was unreachable.
    match share.code.or(share.ticket) {
        Some(invite) => {
            lines.push(Line::styled("On the machine you want to add, run:", label));
            lines.push(Line::raw(""));
            lines.extend(
                wrap_fixed(&pair_command(invite), content_width)
                    .into_iter()
                    .map(|chunk| Line::styled(chunk, value)),
            );
            lines.push(Line::raw(""));
            lines.push(Line::styled("Not installed there yet? First run:", label));
            lines.extend(
                wrap_fixed(INSTALL_COMMAND, content_width)
                    .into_iter()
                    .map(|chunk| {
                        Line::styled(chunk, Style::default().fg(theme.agent_overlay_secondary))
                    }),
            );
            lines.push(Line::raw(""));
            lines.push(match joined {
                Some(name) => Line::styled(
                    format!("✓ {name} joined — it is in your machines now."),
                    Style::default().fg(theme.copy_feedback_accent),
                ),
                None => Line::styled(
                    format!("{} waiting for it to join…", waiting_glyph(animation_phase)),
                    label,
                ),
            });
            // The same warning the share panel carries, because this hands over
            // the same ticket. A friendlier way in must not be a quieter one.
            lines.push(Line::styled(
                "Anyone who runs it gets a full shell here.",
                Style::default().fg(theme.agent_overlay_warm),
            ));
        }
        None => lines.push(Line::styled(
            "Only the machine hosting this session can add one.",
            label,
        )),
    }
    if let Some(notice) = share.notice {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            notice.to_owned(),
            Style::default().fg(theme.copy_feedback_accent),
        ));
    }

    let height = u16::try_from(lines.len().saturating_add(5))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    let block = Block::bordered()
        .title(Line::styled(
            " Add a machine ",
            Style::default()
                .fg(theme.agent_overlay_chrome)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.agent_overlay_chrome));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            inner.x.saturating_add(1),
            inner.y.saturating_add(1),
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(3).max(1),
        ),
    );

    let help = Rect::new(
        inner.x,
        inner.y.saturating_add(inner.height.saturating_sub(1)),
        inner.width,
        1,
    );
    let buffer = frame.buffer_mut();
    buffer.set_stringn(
        help.x,
        help.y,
        " ".repeat(usize::from(help.width)),
        usize::from(help.width),
        Style::default().bg(theme.footer_background),
    );
    render_footer_segments(
        buffer,
        theme,
        help.x.saturating_add(1),
        help.y,
        help.right(),
        if share.code.is_some() || share.ticket.is_some() {
            ADD_MACHINE_HELP
        } else {
            SHARE_HELP_GUEST
        },
    );
}

/// The waiting spinner, on the same clock as the inbox's own.
fn waiting_glyph(animation_phase: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[animation_phase % FRAMES.len()]
}

pub(in crate::tui) fn render_rename_prompt(
    frame: &mut Frame<'_>,
    prompt: &RenamePrompt,
    theme: &UiTheme,
) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(28, 64).min(area.width);
    let height = 7_u16.min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let title = match prompt.target {
        RenameTarget::Pane(_) => " Rename pane ",
        RenameTarget::Tab(_) => " Rename tab ",
    };
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::bordered()
            .title(Line::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(theme.footer_accent)),
        panel,
    );
    let inner = Block::bordered().inner(panel);
    let field = truncate_trailing(&prompt.value, usize::from(inner.width));
    let mut lines = vec![Line::raw(field)];
    if let Some(error) = &prompt.error {
        lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
    } else {
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        "Enter save · Esc cancel",
        Style::default().fg(theme.footer_muted),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}
pub(in crate::tui) fn render_delete_tab_confirmation(
    frame: &mut Frame<'_>,
    pane_count: usize,
    theme: &UiTheme,
) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(28, 64).min(area.width);
    let height = 7_u16.min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::bordered().border_style(Style::default().fg(theme.footer_accent)),
        panel,
    );
    let inner = Block::bordered().inner(panel);
    let pane_label = if pane_count == 1 { "pane" } else { "panes" };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("Delete tab?", Style::default().add_modifier(Modifier::BOLD)),
            Line::raw(format!("{pane_count} {pane_label}")),
            Line::raw(""),
            Line::styled(
                "Enter/y yes · Esc/n no",
                Style::default().fg(theme.footer_muted),
            ),
        ]),
        inner,
    );
}

/// Ctrl+Q, asking which of the two leavings was meant.
///
/// They are one keystroke apart and could not be less alike afterwards: one is
/// undone by typing `p2pmux attach`, the other takes every running pane with
/// it. So each answer is labelled with its consequence rather than its verb,
/// and the reflex answer — Enter — is the reversible one.
pub(in crate::tui) fn render_quit_prompt(frame: &mut Frame<'_>, theme: &UiTheme) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(36, 60).min(area.width);
    let height = 9_u16.min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::bordered().border_style(Style::default().fg(theme.footer_accent)),
        panel,
    );
    let inner = Block::bordered().inner(panel);
    let key = Style::default()
        .fg(theme.footer_accent)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "Leave this session?",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("d", key),
                Span::raw("  detach — leave it running"),
            ]),
            Line::from(vec![
                Span::styled("k", key),
                Span::styled(
                    "  kill — end it, panes and all",
                    Style::default().fg(theme.agent_overlay_warm),
                ),
            ]),
            Line::raw(""),
            Line::styled(
                "Enter detach · Esc cancel",
                Style::default().fg(theme.footer_muted),
            ),
        ]),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        layout::{Node, Tab},
        tui::{
            ModalState, MultiPaneTui, ShareView, render_multi_pane_with_copy_feedback,
            test_support::layout,
        },
    };

    #[test]
    fn share_modal_shows_one_runnable_join_line_and_keeps_the_ticket_off_screen() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 1, 1)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("layout");
        tui.modal = ModalState::Share;
        let mut terminal = Terminal::new(TestBackend::new(160, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                render_multi_pane_with_copy_feedback(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    None,
                    None,
                    ShareView {
                        code: Some("4KP7Q-M2XRW"),
                        ticket: Some("p2pmux-v3:TICKETVALUE"),
                        notice: Some("✓ copied join command"),
                    },
                    None,
                    None,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Share this session"));
        // The whole invite, as the guest will type it. Not a bare code the host then has to
        // explain, and not two identifiers presented as a choice.
        assert!(rendered.contains("p2pmux join 4KP7Q-M2XRW"));
        assert!(rendered.contains("Expires in 6h."));
        // The ticket is one keypress away, but showing 200 characters beside an 11-character
        // code is what made the panel read as a decision rather than an instruction.
        assert!(!rendered.contains("p2pmux-v3:TICKETVALUE"));
        assert!(rendered.contains("✓ copied join command"));
        assert!(rendered.contains("TICKET, WORKS OFFLINE"));
    }

    #[test]
    fn share_modal_says_so_when_the_rendezvous_gave_no_code() {
        // A session whose node could not reach the service still has a working invite, so the
        // panel has to distinguish "no code" from "no session to share".
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 1, 1)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("layout");
        tui.modal = ModalState::Share;
        let mut terminal = Terminal::new(TestBackend::new(160, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                render_multi_pane_with_copy_feedback(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    None,
                    None,
                    ShareView {
                        code: None,
                        ticket: Some("p2pmux-v3:TICKETVALUE"),
                        notice: None,
                    },
                    None,
                    None,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("No short code — rendezvous unreachable."));
        // With no code the ticket is the only invite, so it comes back on screen — still as a
        // line the guest runs unedited rather than a labelled identifier.
        assert!(rendered.contains("p2pmux join p2pmux-v3:TICKETVALUE"));
        assert!(rendered.contains("Never expires."));
        // Enter already falls back to the ticket here, so offering `t` as well would be the
        // same key twice.
        assert!(!rendered.contains("TICKET, WORKS OFFLINE"));
    }

    fn add_machine_screen(tui: &MultiPaneTui) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                render_multi_pane_with_copy_feedback(
                    frame,
                    tui,
                    &BTreeMap::new(),
                    None,
                    None,
                    ShareView {
                        code: Some("4KP7Q-M2XRW"),
                        ticket: Some("p2pmux-v3:TICKETVALUE"),
                        notice: None,
                    },
                    None,
                    None,
                );
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn the_add_machine_panel_gives_a_line_to_run_and_a_way_to_get_p2pmux_there() {
        let mut tui = crate::tui::test_support::home_tui(&[]);
        tui.set_home_open(true, "test");
        tui.open_add_machine();

        let rendered = add_machine_screen(&tui);
        assert!(rendered.contains("Add a machine"));
        // `pair`, not `join`: the same code, but a standing arrangement rather
        // than one visit, and the word is what tells them apart beforehand.
        assert!(rendered.contains("p2pmux pair 4KP7Q-M2XRW"), "{rendered}");
        assert!(!rendered.contains("p2pmux join 4KP7Q-M2XRW"));
        // The case the CLI flow never covered: a machine with no p2pmux on it.
        assert!(rendered.contains("curl -fsSL https://p2pmux.com/install.sh | sh"));
        assert!(rendered.contains("waiting for it to join"));
        // The same ticket the share panel warns about, so the same warning.
        assert!(rendered.contains("Anyone who runs it gets a full shell here."));
    }

    #[test]
    fn the_add_machine_panel_reports_the_machine_that_turned_up() {
        let mut tui = crate::tui::test_support::home_tui(&[]);
        tui.set_home_open(true, "test");
        tui.open_add_machine();
        tui.snapshot.members.push(crate::layout::Member {
            peer_id: b"droplet".to_vec(),
            endpoint_addr: b"endpoint-droplet".to_vec(),
            display_name: String::from("droplet"),
        });

        let rendered = add_machine_screen(&tui);
        assert!(rendered.contains("droplet joined"), "{rendered}");
        assert!(!rendered.contains("waiting for it to join"));
    }

    /// Only the coordinator's node holds a ticket, so a guest is told rather
    /// than offered an invite it cannot make.
    #[test]
    fn a_member_with_no_invite_is_told_it_cannot_add_one() {
        let mut tui = crate::tui::test_support::home_tui(&[]);
        tui.set_home_open(true, "test");
        tui.open_add_machine();

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                render_multi_pane_with_copy_feedback(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    None,
                    None,
                    ShareView::default(),
                    None,
                    None,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("Only the machine hosting this session can add one."));
        assert!(!rendered.contains("p2pmux pair"));
    }
}
