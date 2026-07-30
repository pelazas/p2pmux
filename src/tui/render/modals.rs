//! Centred overlay panels: the share invite, the rename prompt, and the
//! delete-tab confirmation.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph},
};

use crate::{
    config::UiTheme,
    tui::{
        RenamePrompt, RenameTarget, ShareView,
        render::footer::{
            SHARE_HELP, SHARE_HELP_GUEST, SHARE_HELP_NO_CODE, render_footer_segments,
        },
        text::{truncate_trailing, wrap_fixed},
    },
};

/// The invite panel behind Ctrl+S.
///
/// Both identifiers are shown with what they actually reach, because the difference is the
/// whole point: only the ticket travels to another machine.
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
    match share.ticket {
        Some(ticket) => {
            // The code first, because it is the one a person can read down a phone line.
            // Each carries what it actually costs: the code is short but expires and needs
            // our service to be up; the ticket is unwieldy but depends on nothing.
            match share.code {
                Some(code) => {
                    lines.push(Line::styled("CODE — p2pmux join, expires in 6h", label));
                    lines.push(Line::styled(code.to_owned(), value));
                }
                None => lines.push(Line::styled(
                    "CODE — unavailable, rendezvous unreachable",
                    Style::default().fg(theme.agent_overlay_warm),
                )),
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("TICKET — never expires, no service", label));
            lines.extend(
                wrap_fixed(ticket, content_width)
                    .into_iter()
                    .map(|chunk| Line::styled(chunk, value)),
            );
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Anyone with the ticket can join this session.",
                Style::default().fg(theme.agent_overlay_warm),
            ));
        }
        None => lines.push(Line::styled(
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
    fn share_modal_shows_both_invites_with_what_each_costs() {
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
                        notice: Some("✓ copied code"),
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
        // Each identifier is shown with what it actually costs, because the difference is the
        // whole point: the code is readable but expires and needs our service up.
        assert!(rendered.contains("CODE — p2pmux join, expires in 6h"));
        assert!(rendered.contains("4KP7Q-M2XRW"));
        assert!(rendered.contains("TICKET — never expires, no service"));
        assert!(rendered.contains("p2pmux-v3:TICKETVALUE"));
        assert!(rendered.contains("✓ copied code"));
        assert!(rendered.contains("COPY CODE"));
        assert!(rendered.contains("COPY TICKET"));
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

        assert!(rendered.contains("CODE — unavailable, rendezvous unreachable"));
        assert!(rendered.contains("p2pmux-v3:TICKETVALUE"));
        // Enter has to fall back to the ticket, so the help row must not promise a code.
        assert!(!rendered.contains("COPY CODE"));
        assert!(rendered.contains("COPY TICKET"));
    }
}
