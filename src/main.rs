use std::process::ExitCode;

fn main() -> ExitCode {
    // Rust ignores SIGPIPE, so a reader that exits early (`| head`, `grep -q`)
    // turns the next stdout write into a panic and exit 101. The default
    // disposition kills the process with the signal, which is what every other
    // Unix tool does and what `set -o pipefail` is written against.
    reset_sigpipe();
    // A tab completion asks this process for candidates and exits. Anything
    // written before this would land in the script the shell is sourcing.
    clap_complete::CompleteEnv::with_factory(p2pmux::cli::command)
        .completer("p2pmux")
        .complete();
    let cli = p2pmux::cli::parse();
    // Some commands must not pay to start a runtime — see `run_without_runtime`.
    // Everything else gets one.
    let result = match p2pmux::cli::run_without_runtime(&cli) {
        Some(result) => result,
        None => match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime.block_on(p2pmux::cli::run(cli)),
            Err(error) => Err(error.into()),
        },
    };
    // Returning Result from main would print the error with Debug, which shows users
    // Rust internals like `Custom { kind: TimedOut, error: "..." }` instead of the
    // message the error actually carries. Print Display and pick the exit code here.
    if let Err(error) = result {
        eprintln!("Error: {error}");
        return ExitCode::FAILURE;
    }
    // Here rather than inside the client, because here is the one place every
    // path has already put the terminal back: the alternate screen is gone, raw
    // mode is off, and a line written now survives in the scrollback instead of
    // being wiped by the teardown a moment later. It prints at most once in the
    // life of a machine, and only on one that has had a second person in a
    // session.
    p2pmux::telemetry::ask_for_a_word();
    ExitCode::SUCCESS
}

#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY: this installs the kernel's default disposition, not a handler,
    // and it runs once before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}
