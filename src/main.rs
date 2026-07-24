#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    p2pmux::cli::parse_and_run().await
}
