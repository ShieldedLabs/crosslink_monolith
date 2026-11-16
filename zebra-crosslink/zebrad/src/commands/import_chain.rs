//! `import-chain` subcommand - builds a new state from a chain prefix file and runs a node on it
//! (#241).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
/// identity, wallet and default finalizer key, is generated fresh. `--keep-identity` carries the
/// configured node's over instead, and refuses while that node is running.
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

    /// The last PoW height to load.
    #[clap(
        long,
        help = "the last PoW height to load: the file runs up to its first PoW block above it, \
                and nothing after that runs, appended scenario instructions included \
                [default: the whole file]"
    )]
    height: Option<u32>,

    /// Run as the configured node.
    #[clap(
        long,
        help = "run as the configured node: copy its secret.seed, and with it its network identity, \
                wallet and default finalizer key, into the new state. Refused while the configured \
                node is running, since two nodes with one seed vote as the same finalizer \
                [default: a new node, with a fresh secret.seed]"
    )]
    keep_identity: bool,

    /// Keep the configured peers and internal miner.
    #[clap(
        long,
        help = "keep the configured PoW and BFT peers and the internal miner while loading; \
                by default the node is isolated so that only the file's blocks reach it"
    )]
    connect: bool,
}

/// The configured state, as it was before `override_config` pointed the node at the new one.
static CONFIGURED_STATE: OnceLock<(zebra_state::Config, zebra_chain::parameters::Network)> = OnceLock::new();

/// The seed file a node keeps in its state directory.
const SECRET_SEED_FILE: &str = "secret.seed";

impl ImportChainCmd {
    /// Copies the configured node's `secret.seed` into `target`, or says why it cannot.
    fn keep_identity(&self, target: &Path) -> Result<(), String> {
        let (configured, network) = CONFIGURED_STATE.get().expect("override_config runs before run");
        let seed = configured.cache_dir.join(SECRET_SEED_FILE);
        if !seed.is_file() {
            return Err(format!("{} does not exist, so the configured node has no identity to keep", seed.display()));
        }
        if configured.state_database_in_use(network) {
            return Err(format!(
                "the configured node's state at {} is in use. Stop that node first: two nodes with one \
                 secret.seed vote as the same finalizer",
                configured.cache_dir.display(),
            ));
        }
        std::fs::create_dir_all(target).map_err(|err| format!("could not create {}: {err}", target.display()))?;
        std::fs::copy(&seed, target.join(SECRET_SEED_FILE))
            .map_err(|err| format!("could not copy {} into {}: {err}", seed.display(), target.display()))?;
        Ok(())
    }

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
                 delete it or choose another directory with --cache-dir. To keep the configured \
                 node's identity, use --keep-identity rather than copying its secret.seed in.",
                target.display(),
            );
            std::process::exit(2);
        }
        if self.keep_identity {
            if let Err(why) = self.keep_identity(&target) {
                eprintln!("import-chain: --keep-identity: {why}.");
                std::process::exit(2);
            }
            tracing::info!("import-chain: running as the configured node, with its secret.seed");
        }
        tracing::info!("import-chain: building a new state at {}", target.display());

        *zebra_crosslink::chain_prefix::CHAIN_IMPORT.lock().unwrap() =
            Some(zebra_crosslink::chain_prefix::ChainImport {
                path: self.input.clone(),
                trusted: self.trusted,
                height: self.height,
            });
        StartCmd::default().run();
    }
}

impl config::Override<ZebradConfig> for ImportChainCmd {
    fn override_config(&self, config: ZebradConfig) -> Result<ZebradConfig, FrameworkError> {
        let mut config = StartCmd::default().override_config(config)?;
        let _ = CONFIGURED_STATE.set((config.state.clone(), config.network.network.clone()));
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
