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

/// The install line for a machine that has never had p2pmux on it.
pub(in crate::tui) const INSTALL_COMMAND: &str = "curl -fsSL https://p2pmux.com/install.sh | sh";

/// A command plus the line that makes it runnable on a machine without p2pmux.
///
/// The invite panel hands its text to someone who is, by definition, not in the
/// session yet — and quite often has never had the binary. `p2pmux join <code>`
/// alone is `command not found` on that machine, with nothing on screen saying
/// what to install or where from. The add-machine panel already learned this and
/// carries the install line; the invite that travels to another person is the one
/// that needed it most and did not have it.
///
/// A shell comment rather than a second command: pasted into a chat window it
/// reads as a note, and pasted into a terminal by someone who *does* have p2pmux
/// it does nothing at all. Nobody gets a reinstall they did not ask for.
fn with_install_hint(command: String) -> String {
    format!("{command}\n# not installed? {INSTALL_COMMAND}")
}

/// The line a machine you own runs to be added to the fleet, permanently.
///
/// Deliberately not [`join_command`], which the same code would also satisfy: a
/// join is one visit and pairing is a standing arrangement, and the word on
/// screen is the only thing that tells the two apart before the fact.
pub(in crate::tui) fn pair_command(invite: &str) -> String {
    format!("p2pmux pair {invite}")
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
        // The panel prefers the short code and falls back to the ticket for the
        // same reason the share modal does: a rendezvous outage costs the code,
        // never the invite.
        ShareCopy::Pair => ("pair command", code.or(ticket)),
    };
    let Some(text) = text else {
        return format!("no {what} to copy");
    };
    let line = with_install_hint(match request {
        ShareCopy::Pair => pair_command(text),
        ShareCopy::Code | ShareCopy::Ticket => join_command(text),
    });
    match copy_selection_to_clipboard(&line) {
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

    /// Pairing and joining resolve the same code, so the only thing that says
    /// which one is happening is the word in the line that gets pasted.
    #[test]
    fn a_pair_copy_is_the_pair_line_and_falls_back_to_the_ticket_too() {
        assert_eq!(pair_command("4KP7Q-M2XRW"), "p2pmux pair 4KP7Q-M2XRW");
        assert_eq!(
            share_copy_result(ShareCopy::Pair, None, None),
            "no pair command to copy"
        );
        // A rendezvous outage costs the short code, never the invite, so the
        // panel still has a line to hand over.
        assert_ne!(
            share_copy_result(ShareCopy::Pair, Some("p2pmux-v3:T"), None),
            "no pair command to copy"
        );
    }

    #[test]
    fn both_invites_copy_as_a_line_the_guest_can_run_unedited() {
        // Deliberately not asserted against the clipboard: a headless CI box has none, and
        // the thing worth pinning is the shape of the text, not the copy plumbing.
        assert_eq!(join_command("4KP7Q-M2XRW"), "p2pmux join 4KP7Q-M2XRW");
        assert_eq!(join_command("p2pmux-v3:T"), "p2pmux join p2pmux-v3:T");
    }

    /// The person an invite is pasted to does not have p2pmux — that is what
    /// being invited means — so the paste has to say where to get it.
    #[test]
    fn every_copied_invite_carries_the_install_line() {
        for command in [
            join_command("4KP7Q-M2XRW"),
            pair_command("4KP7Q-M2XRW"),
            join_command("p2pmux-v3:T"),
        ] {
            let paste = with_install_hint(command.clone());
            let (first, hint) = paste.split_once('\n').expect("two lines");
            // The runnable line stays first and stays untouched: it is what the
            // host reads out over a call, and what the panel shows above it.
            assert_eq!(first, command);
            assert!(hint.contains("p2pmux.com/install.sh"), "{hint}");
            // A comment, so a recipient who already has p2pmux can paste the
            // whole thing into a shell and get exactly one command run.
            assert!(hint.starts_with('#'), "{hint}");
        }
    }
}
