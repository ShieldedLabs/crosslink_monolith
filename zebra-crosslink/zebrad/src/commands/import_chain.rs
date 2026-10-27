//! `import-chain` subcommand - builds a new state from a chain prefix file and runs a node on it
//! (#241).

use std::path::{Path, PathBuf};

use abscissa_core::{config, Application, Command, FrameworkError, Runnable};
use clap::Parser;

use crate::{commands::StartCmd, config::ZebradConfig, prelude::APPLICATION};

/// Build a new state from a chain prefix file and run a node on it
///
/// The configured state is never touched: the node runs on a new, empty state directory, by
/// default one beside the configured one named for the file, and the command refuses a
/// directory that already holds anything. Every block in the file goes through the node's normal
/// admission paths, so the state comes out as a node following that chain would have built it.
/// The node keeps running afterwards.
///
/// A new state directory is a new node: its `secret.seed`, and with it the node's network
/// identity and default finalizer key, is generated fresh. Copy `secret.seed` into the new
/// directory first to keep the old ones.
#[derive(Command, Debug, Default, Parser)]
pub struct ImportChainCmd {
    /// The file to load.
    #[clap(help = "the chain prefix file to load (.zeccltf), as written by export-chain")]
    input: PathBuf,

    /// The new state directory.
    #[clap(
        long,
        short,
        help = "the new state directory to build, which must be empty or absent \
                [default: beside the configured one, named for the file]"
    )]
    cache_dir: Option<PathBuf>,

    /// Skip the PoW blocks' proof, signature and script checks.
    #[clap(
        long,
        help = "skip proof, signature and script verification of the file's PoW blocks; \
                every other check still runs. Only for files from a node you trust"
    )]
    trusted: bool,

    /// Keep the configured peers and internal miner.
    #[clap(
        long,
        help = "keep the configured PoW and BFT peers and the internal miner while loading; \
                by default the node is isolated so that only the file's blocks reach it"
    )]
    connect: bool,
}

impl ImportChainCmd {
    /// The state directory the import builds, given the configured one.
    fn target(&self, configured: &Path) -> PathBuf {
        self.cache_dir.clone().unwrap_or_else(|| {
            let stem = self
                .input
                .file_stem()
                .map_or_else(|| "chain-prefix".into(), |stem| stem.to_string_lossy());
            let mut name = configured.file_name().unwrap_or_default().to_os_string();
            name.push(format!("-import-{stem}"));
            configured.with_file_name(name)
        })
    }
}

impl Runnable for ImportChainCmd {
    #[allow(clippy::print_stderr)]
    fn run(&self) {
        // `override_config` has already pointed the state at the target.
        let target = APPLICATION.config().state.cache_dir.clone();
        let occupied =
            std::fs::read_dir(&target).map_or(false, |mut entries| entries.next().is_some());
        if occupied {
            eprintln!(
                "import-chain: {} is not empty. A chain prefix is imported into a new state; \
                 delete it or choose another directory with --cache-dir.",
                target.display(),
            );
            std::process::exit(2);
        }
        tracing::info!("import-chain: building a new state at {}", target.display());

        *zebra_crosslink::chain_prefix::CHAIN_IMPORT.lock().unwrap() =
            Some(zebra_crosslink::chain_prefix::ChainImport {
                path: self.input.clone(),
                trusted: self.trusted,
            });
        StartCmd::default().run();
    }
}

impl config::Override<ZebradConfig> for ImportChainCmd {
    fn override_config(&self, config: ZebradConfig) -> Result<ZebradConfig, FrameworkError> {
        let mut config = StartCmd::default().override_config(config)?;
        config.state.cache_dir = self.target(&config.state.cache_dir);
        // A cookie written where the configured node keeps its own would lock out that node's
        // RPC clients.
        config.rpc.cookie_dir = config.state.cache_dir.clone();
        if !self.connect {
            config.network.initial_mainnet_peers.clear();
            config.network.initial_testnet_peers.clear();
            config.state.network_initial_peers.clear();
            config.crosslink.bft_peers.clear();
            config.mining.internal_miner = false;
        }
        Ok(config)
    }
}
