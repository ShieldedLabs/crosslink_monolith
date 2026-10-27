//! `import-chain` subcommand - starts the node and loads a chain prefix file into it (#241).

use std::path::PathBuf;

use abscissa_core::{config, Command, FrameworkError, Runnable};
use clap::Parser;

use crate::{commands::StartCmd, config::ZebradConfig};

/// Start the node and load a chain prefix file into it
///
/// Meant for an empty state: a node returned to the prefix's height. Every block in the file goes
/// through the node's normal admission paths, so the state comes out as a node following that
/// chain would have built it. The node keeps running afterwards.
#[derive(Command, Debug, Default, Parser)]
pub struct ImportChainCmd {
    /// The file to load.
    #[clap(help = "the chain prefix file to load (.zeccltf), as written by export-chain")]
    input: PathBuf,

    /// Skip the PoW blocks' proof, signature and script checks.
    #[clap(
        long,
        help = "skip proof, signature and script verification of the file's PoW blocks; \
                every other check still runs. Only for files from a node you trust"
    )]
    trusted: bool,
}

impl Runnable for ImportChainCmd {
    fn run(&self) {
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
        StartCmd::default().override_config(config)
    }
}
