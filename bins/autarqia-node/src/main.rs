use std::net::SocketAddr;
use std::path::PathBuf;

use autarqia_node::start_node_server;
use clap::Parser;
use k256::ecdsa::SigningKey;

#[derive(Parser, Debug)]
#[command(name = "autarqia-node", about = "Autarqia Energy-Backed AirLLM + MPP-I Node")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    #[arg(long, default_value = "data/rocksdb")]
    db_path: PathBuf,
    #[arg(long, default_value = "/tmp/autarqia-airllm.sock")]
    worker_uds: PathBuf,
    #[arg(long, default_value = "data/bufi-outbox.jsonl")]
    bufi_outbox: PathBuf,
    #[arg(long, default_value = "https://rpc.testnet.arc.network")]
    arc_rpc_url: String,
    #[arg(
        long,
        default_value = "2222222222222222222222222222222222222222222222222222222222222222"
    )]
    provider_private_key_hex: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let raw_sk = hex::decode(
        args.provider_private_key_hex
            .strip_prefix("0x")
            .unwrap_or(&args.provider_private_key_hex),
    )?;
    let sk = SigningKey::from_slice(&raw_sk)?;

    let (addr, _nucleus, handle) = start_node_server(
        args.bind,
        &args.db_path,
        &args.worker_uds,
        &args.bufi_outbox,
        &args.arc_rpc_url,
        sk,
        None,
    )
    .await?;

    println!(
        "{}",
        serde_json::json!({
            "event": "autarqia_node_listening",
            "bind_addr": addr.to_string(),
            "worker_uds": args.worker_uds,
            "db_path": args.db_path,
        })
    );

    handle.await?;
    Ok(())
}
