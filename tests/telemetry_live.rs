//! The one test that proves the client and the service agree.
//!
//! Everything else about telemetry can be right while this is wrong, and the
//! failure would be invisible: the send path is silent by design, so a payload
//! the service rejects looks exactly like a payload nobody sent, which looks
//! exactly like nobody using p2pmux. A shape mismatch here would be discovered
//! as a permanently empty dashboard, weeks later, with no way to tell it from
//! the truth.
//!
//! Skipped unless `P2PMUX_METRICS_URL` names a service to talk to, the same way
//! `tests/hosted_rendezvous.rs` skips without `P2PMUX_RENDEZVOUS_URL`. Point it
//! at a staging Worker, or at the real one — a row from version `0.0.0` is
//! recognisable and deletable.
//!
//! One test in this file on purpose: it sets `HOME` for the whole process, which
//! is only safe while nothing else in the binary is running.

use std::{fs, path::PathBuf};

use p2pmux::telemetry::{self, Consent, Counter};

fn temporary_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("p2pmux-telemetry-live-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).expect("temporary home");
    home
}

#[test]
fn a_real_ping_is_accepted_by_a_real_service() {
    if std::env::var("P2PMUX_METRICS_URL").is_err() {
        return;
    }
    let home = temporary_home();
    // SAFETY: this is the only test in this binary, so nothing else is reading
    // the environment while it is written.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("DO_NOT_TRACK");
        std::env::remove_var("CI");
        std::env::remove_var("P2PMUX_TELEMETRY");
    }

    // Before the first `consent()` call, which caches for the process.
    telemetry::set_consent(Consent::Granted);
    assert_eq!(telemetry::consent(), Consent::Granted);

    telemetry::bump(Counter::Sessions, 1);
    telemetry::bump(Counter::Peers, 2);
    telemetry::bump(Counter::Agents, 3);
    telemetry::mark_activated();

    let payload = telemetry::would_send();
    assert_eq!(payload.id.len(), 32, "{payload:?}");
    assert_eq!(payload.sessions, 1, "counters must reach the payload");
    assert_eq!(payload.peers, 2);
    assert_eq!(payload.agents, 3);
    assert!(payload.activated);

    assert!(
        telemetry::send_if_due(),
        "the service rejected a payload this client produced"
    );

    // A send that landed must not be repeated: the whole point of the daily
    // stamp is that a person who opens twenty sessions appears once.
    assert!(
        !telemetry::send_if_due(),
        "a second send inside the day would count one machine twice"
    );

    // And the counters it carried are gone, so tomorrow's line is tomorrow's.
    let after = telemetry::would_send();
    assert_eq!(after.sessions, 0, "{after:?}");
    assert_eq!(after.peers, 0);
    assert_eq!(after.agents, 0);
    assert!(after.activated, "activation is sticky, not spent");

    let _ = fs::remove_dir_all(&home);
}
