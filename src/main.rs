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
    ExitCode::SUCCESS
}
