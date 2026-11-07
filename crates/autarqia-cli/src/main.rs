use std::net::SocketAddr;
use std::path::PathBuf;

use autarqia_cli::{
    start_local_openai_mppi_proxy, verify_workspace_dag, AutonomousBuyerClient, BuyerRunOptions,
};
use autarqia_primitives::{ChatMessage, SettlementRail, TokenAtomicAmount};
use clap::{Parser, Subcommand};
use k256::ecdsa::SigningKey;

#[derive(Parser, Debug)]
#[command(name = "autarqia-cli", about = "Autarqia DAG Verifier, Autonomous Buyer & MPP-I Proxy")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    VerifyDag {
        #[arg(long, default_value = "Cargo.toml")]
        manifest_path: PathBuf,
    },
    Buy {
        #[arg(long, default_value = "http://127.0.0.1:8787")]
        provider_url: String,
        #[arg(long, default_value = "Qwen/Qwen2.5-0.5B-Instruct")]
        model_id: String,
        #[arg(long, default_value = "Explain how solar power backs AI compute in one sentence.")]
        prompt: String,
        #[arg(long, default_value_t = 8)]
        max_output_tokens: u32,
        #[arg(
            long,
            default_value = "3333333333333333333333333333333333333333333333333333333333333333"
        )]
        buyer_private_key_hex: String,
    },
    Proxy {
        #[arg(long, default_value = "127.0.0.1:8788")]
        bind: SocketAddr,
        #[arg(long, default_value = "http://127.0.0.1:8787")]
        provider_url: String,
        #[arg(
            long,
            default_value = "3333333333333333333333333333333333333333333333333333333333333333"
        )]
        buyer_private_key_hex: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::VerifyDag { manifest_path } => {
            let report = verify_workspace_dag(&manifest_path)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Buy {
            provider_url,
            model_id,
            prompt,
            max_output_tokens,
            buyer_private_key_hex,
        } => {
            let raw = hex::decode(
                buyer_private_key_hex
                    .strip_prefix("0x")
                    .unwrap_or(&buyer_private_key_hex),
            )?;
            let sk = SigningKey::from_slice(&raw)?;
            let buyer = AutonomousBuyerClient::new(provider_url, sk);
            let outcome = buyer
                .execute_run(BuyerRunOptions {
                    rail: SettlementRail::AndoMppiArcV1,
                    model_id,
                    messages: vec![ChatMessage {
                        role: "user".to_owned(),
                        content: prompt,
                    }],
                    max_output_tokens,
                    channel_deposit_atomic: TokenAtomicAmount::new(50_000),
                    initial_voucher_atomic: TokenAtomicAmount::new(1_000),
                    stop_acks_after_tokens: None,
                    order_id: None,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        Commands::Proxy {
            bind,
            provider_url,
            buyer_private_key_hex,
        } => {
            let raw = hex::decode(
                buyer_private_key_hex
                    .strip_prefix("0x")
                    .unwrap_or(&buyer_private_key_hex),
            )?;
            let sk = SigningKey::from_slice(&raw)?;
            let (addr, handle) = start_local_openai_mppi_proxy(bind, provider_url, sk).await?;
            println!(
                "{}",
                serde_json::json!({
                    "event": "autarqia_openai_mppi_proxy_listening",
                    "bind_addr": addr.to_string(),
                })
            );
            handle.await?;
        }
    }
    Ok(())
}
