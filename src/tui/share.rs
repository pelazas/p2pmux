//! What the share modal offers, and the outcome of a copy.

use crate::tui::{ShareCopy, copy_selection_to_clipboard};

/// The line a guest runs, built from whichever invite is being handed over.
///
/// Both the panel and the clipboard go through here so that what the host reads on screen is
/// exactly what they paste into a chat window. A bare code would make the recipient ask what
/// to do with it; a runnable command answers that before it is asked, and `join` takes a code
/// and a ticket interchangeably, so one shape covers both.
pub(in crate::tui) fn join_command(invite: &str) -> String {
    format!("p2pmux join {invite}")
}

/// Run one share-modal copy and report the result back into the modal.
///
/// A code request with no code falls back to the ticket rather than reporting nothing: the
/// primary key should always yield a working invite, and the ticket is the one that never
/// depends on the rendezvous service being up.
pub(crate) fn share_copy_result(
    request: ShareCopy,
    ticket: Option<&str>,
    code: Option<&str>,
) -> String {
    let (what, text) = match request {
        ShareCopy::Code => match code {
            Some(code) => ("join command", Some(code)),
            None => ("ticket command", ticket),
        },
        ShareCopy::Ticket => ("ticket command", ticket),
    };
    let Some(text) = text else {
        return format!("no {what} to copy");
    };
    match copy_selection_to_clipboard(&join_command(text)) {
        Ok(_) => format!("✓ copied {what}"),
        Err(error) => format!("clipboard copy failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_copy_with_no_code_falls_back_to_the_ticket() {
        assert_eq!(
            share_copy_result(ShareCopy::Code, Some("p2pmux-v3:T"), None),
            share_copy_result(ShareCopy::Ticket, Some("p2pmux-v3:T"), None),
        );
        assert_eq!(
            share_copy_result(ShareCopy::Code, None, None),
            "no ticket command to copy"
        );
    }

    #[test]
    fn both_invites_copy_as_a_line_the_guest_can_run_unedited() {
        // Deliberately not asserted against the clipboard: a headless CI box has none, and
        // the thing worth pinning is the shape of the text, not the copy plumbing.
        assert_eq!(join_command("4KP7Q-M2XRW"), "p2pmux join 4KP7Q-M2XRW");
        assert_eq!(join_command("p2pmux-v3:T"), "p2pmux join p2pmux-v3:T");
    }
}
