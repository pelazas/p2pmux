//! The opt-in UI trace log behind `P2PMUX_DEBUG_UI`.

use std::{
    env, fmt,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Instant,
};

struct UiDebugLog {
    path: Option<PathBuf>,
    start: Instant,
    write_lock: Mutex<()>,
}
pub(crate) fn ui_debug_log(event: &str, fields: fmt::Arguments<'_>) {
    static UI_DEBUG_LOG: OnceLock<UiDebugLog> = OnceLock::new();
    let debug_log = UI_DEBUG_LOG.get_or_init(|| {
        let path = env::var("P2PMUX_DEBUG_UI").ok().and_then(|value| {
            if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("false") {
                None
            } else if value.contains('/') || value.starts_with('.') {
                Some(PathBuf::from(value))
            } else {
                Some(PathBuf::from("/tmp/p2pmux-ui.log"))
            }
        });
        UiDebugLog {
            path,
            start: Instant::now(),
            write_lock: Mutex::new(()),
        }
    });
    let Some(path) = &debug_log.path else {
        return;
    };
    let line = format!(
        "p2pmux ui: {} {} {}\n",
        debug_log.start.elapsed().as_millis(),
        event,
        fields
    );
    let Ok(_write_lock) = debug_log.write_lock.lock() else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}
