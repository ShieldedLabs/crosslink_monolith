use std::{fs, path::PathBuf};

use clap::{Parser, ValueEnum};
use tonic::transport::Channel;
use wallet::{CompactTxStreamerClient, ManualWallet, StakingCliNetwork};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NetworkArg {
    Main,
    Regtest,
}

impl From<NetworkArg> for StakingCliNetwork {
    fn from(value: NetworkArg) -> Self {
        match value {
            NetworkArg::Main => StakingCliNetwork::Main,
            NetworkArg::Regtest => StakingCliNetwork::Regtest,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "staking-cli",
    about = "sequential multi-bond stake cli on top of stake_orchard_to_finalizer_batch",
    long_about = "Build and optionally broadcast a sequence of orchard delegation-bond stake transactions.\n\nThe seed file must contain either a plain-text bip39 mnemonic or a 32-byte hex seed. The hex seed format is for testing only.\n\nThis command requires a synced lightwalletd or zainod endpoint at --lightwalletd-url."
)]
struct Args {
    #[arg(long, help = "target finalizer pubkey in wallet-expected byte order")]
    target: String,

    #[arg(
        long,
        value_delimiter = ',',
        help = "comma-separated list of amounts in zatoshis"
    )]
    amounts: Vec<u64>,

    #[arg(
        long,
        value_name = "PATH",
        long_help = "Path to a seed file. The file must contain either a plain-text bip39 mnemonic or a 32-byte hex seed. The hex seed format is for testing only."
    )]
    seed: PathBuf,

    #[arg(long, value_enum, help = "network parameter")]
    network: NetworkArg,

    #[arg(
        long,
        default_value = "http://127.0.0.1:9067",
        help = "lightwalletd or zainod endpoint"
    )]
    lightwalletd_url: String,

    #[arg(long, help = "do not broadcast, just print what would be sent")]
    dry_run: bool,
}

fn parse_target(hex_target: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_target)
        .map_err(|err| format!("failed to decode --target as hex: {err}"))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        format!(
            "--target must decode to exactly 32 bytes, got {}",
            bytes.len()
        )
    })
}

fn print_results(results: &[Option<(String, u64)>], amounts: &[u64], dry_run: bool) {
    for (index, amount) in amounts.iter().copied().enumerate() {
        match results.get(index).and_then(|entry| entry.as_ref()) {
            Some((txid, _)) if dry_run => {
                println!("{index}: would broadcast built_txid={txid} amount={amount}");
            }
            Some((txid, _)) => {
                println!("{index}: ok txid={txid} amount={amount}");
            }
            None => {
                println!("{index}: FAILED amount={amount}");
            }
        }
    }
}

async fn run() -> Result<(), String> {
    let args = Args::parse();
    if args.amounts.is_empty() {
        return Err("--amounts must not be empty".to_owned());
    }

    let target = parse_target(&args.target)?;
    let seed_text = fs::read_to_string(&args.seed)
        .map_err(|err| format!("failed to read {}: {err}", args.seed.display()))?;
    let network = StakingCliNetwork::from(args.network);

    let channel = Channel::from_shared(args.lightwalletd_url.clone())
        .map_err(|err| {
            format!(
                "invalid --lightwalletd-url {}: {err}",
                args.lightwalletd_url
            )
        })?
        .connect()
        .await
        .map_err(|err| format!("failed to connect to {}: {err}", args.lightwalletd_url))?;
    let mut client = CompactTxStreamerClient::new(channel);

    let (mut wallet, usk) = ManualWallet::from_seed_text(network, &seed_text)?;
    let orchard_tree = wallet.sync_from_lightwalletd(network, &mut client).await?;

    if args.dry_run {
        let results = wallet.dry_run_stake_orchard_to_finalizer_batch(
            network,
            &mut client,
            &usk,
            &args.amounts,
            &orchard_tree,
            target,
        );
        print_results(&results, &args.amounts, true);
    } else {
        let results = wallet
            .stake_orchard_to_finalizer_batch(
                network,
                &mut client,
                &usk,
                &args.amounts,
                &orchard_tree,
                target,
            )
            .await;
        print_results(&results, &args.amounts, false);
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
