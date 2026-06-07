#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gigatiff_server::run_from_cli().await
}
