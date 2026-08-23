use std::process::ExitCode;

fn main() -> ExitCode {
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
