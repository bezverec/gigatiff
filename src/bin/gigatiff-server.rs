#[cfg(feature = "server")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gigatiff::server::run_from_cli().await
}

#[cfg(not(feature = "server"))]
fn main() {
    eprintln!("gigatiff-server was built without the `server` feature");
}
