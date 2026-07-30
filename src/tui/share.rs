//! What the share modal offers: the local ticket for a join code, and the
//! outcome of a copy.

use crate::tui::{ShareCopy, copy_selection_to_clipboard};

/// Read the full ticket the local rendezvous record holds for a join code.
///
/// Only a coordinator publishes a record, so a guest resolves nothing and the share modal
/// says so rather than offering an invite it cannot make.
pub(crate) fn resolve_local_ticket(join_code: &str) -> Option<String> {
    crate::rendezvous::LocalRendezvous::for_current_user()
        .and_then(|store| store.resolve(join_code))
        .ok()
        .map(|ticket| ticket.to_string())
}
/// Run one share-modal copy and report the result back into the modal.
pub(crate) fn share_copy_result(
    request: ShareCopy,
    ticket: Option<&str>,
    join_code: Option<&str>,
) -> String {
    let (what, text) = match request {
        ShareCopy::Ticket => ("ticket", ticket),
        ShareCopy::Code => ("code", join_code),
    };
    let Some(text) = text else {
        return format!("no {what} to copy");
    };
    match copy_selection_to_clipboard(text) {
        Ok(_) => format!("✓ copied {what}"),
        Err(error) => format!("clipboard copy failed: {error}"),
    }
}
