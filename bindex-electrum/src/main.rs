use anyhow::Context as _;
use bindex_electrum::{config::Config, server::Server};
use clap::Parser as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config = Config::parse();
    config
        .validate()
        .context("invalid electrum configuration")?;

    let server = Server::new(config).context("create electrum server")?;
    server.run().await.context("run electrum server")
}
