use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use clap::Parser;
use handclip_core::resolve_auth_token;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about = "Handclip ordered clipboard-event relay")]
struct Args {
    #[arg(long, env = "HANDCLIP_LISTEN", default_value = "127.0.0.1:24871")]
    listen: SocketAddr,

    #[arg(
        long,
        env = "HANDCLIP_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<String>,

    #[arg(long, env = "HANDCLIP_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .compact()
        .init();

    let args = Args::parse();
    let auth_token = resolve_auth_token(args.token, args.token_file.as_deref())?;
    handclip_relay::serve(args.listen, auth_token).await
}
