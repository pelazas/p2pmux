//! What the share modal offers, and the outcome of a copy.

use crate::tui::{ShareCopy, copy_selection_to_clipboard};

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
            Some(code) => ("code", Some(code)),
            None => ("ticket", ticket),
        },
        ShareCopy::Ticket => ("ticket", ticket),
    };
    let Some(text) = text else {
        return format!("no {what} to copy");
    };
    match copy_selection_to_clipboard(text) {
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
            "no ticket to copy"
        );
    }
}
