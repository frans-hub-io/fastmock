#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fastmock_server::cli::run().await
}
