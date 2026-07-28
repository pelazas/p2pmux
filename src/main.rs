use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // Returning Result from main would print the error with Debug, which shows users
    // Rust internals like `Custom { kind: TimedOut, error: "..." }` instead of the
    // message the error actually carries. Print Display and pick the exit code here.
    if let Err(error) = p2pmux::cli::parse_and_run().await {
        eprintln!("Error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
