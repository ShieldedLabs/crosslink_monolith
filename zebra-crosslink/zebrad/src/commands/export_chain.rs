//! `export-chain` subcommand - writes the chain on disk, from genesis to a height, to a chain
//! prefix file that `import-chain` loads into a fresh state (#241).

use std::{path::PathBuf, sync::Arc};

use abscissa_core::{Application, Command, Runnable};
use clap::Parser;
use zebra_chain::{block::Height, parameters::SlashForkSchedule};
use zebra_state::BftCutoff;

use crate::prelude::APPLICATION;

/// Write the chain on disk, from genesis to a height, to a chain prefix file
#[derive(Command, Debug, Default, Parser)]
pub struct ExportChainCmd {
    /// The file to write.
    #[clap(help = "the chain prefix file to write (.zeccltf)")]
    output: PathBuf,

    /// The last PoW height to include.
    #[clap(long, help = "the last PoW height to include [default: the best tip]")]
    height: Option<u32>,

    /// Which decided BFT blocks to include.
    #[clap(
        long,
        default_value = "cited",
        help = "which decided BFT blocks to include: `cited` (up to the newest one a PoW block in \
                the prefix cites), `supported` (every one whose snapshot is in the prefix), \
                `none`, or a count from BFT genesis"
    )]
    bft: BftCutoff,

    /// Path to Zebra's cached state.
    #[clap(long, short, help = "path to directory with the Zebra chain state")]
    cache_dir: Option<PathBuf>,
}

impl Runnable for ExportChainCmd {
    /// `export-chain` sub-command entrypoint.
    #[allow(clippy::print_stdout, clippy::print_stderr)]
    fn run(&self) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime builds: nothing else in this command has started one");
        match runtime.block_on(self.export()) {
            Ok(export) => println!(
                "wrote {} PoW blocks (heights 0..={}, tip {}) and {} BFT blocks ({}) to {}, {} bytes",
                export.pow_blocks,
                export.tip_height,
                export.tip_hash,
                export.bft_blocks,
                export.bft_cutoff,
                export.path,
                export.bytes,
            ),
            Err(error) => {
                eprintln!("export-chain failed: {error}");
                std::process::exit(1);
            }
        }
    }
}

impl ExportChainCmd {
    async fn export(&self) -> Result<zebra_state::crosslink::ChainPrefixExport, String> {
        let config = APPLICATION.config();
        let network = &config.network.network;

        let mut state_config = config.state.clone();
        if let Some(cache_dir) = self.cache_dir.clone() {
            state_config.cache_dir = cache_dir;
        }
        // As `start` builds it: restoring the blocks above `fin` validates them against it.
        state_config.slashfork_schedule = Arc::new(SlashForkSchedule::from_canonical(
            config.crosslink.slashforks.clone(),
        ));
        // Read the backup of the blocks above `fin`, but leave its directory as it is.
        state_config.debug_skip_non_finalized_state_backup_task = true;

        println!(
            "opening state cache at {:?} (the node must be stopped)",
            state_config.cache_dir,
        );
        // The full state, not a read-only view of the database: only it restores the
        // non-finalized backup, which holds every block above `fin`. The finalized database
        // alone ends at `fin`.
        let (_, max_checkpoint_height) =
            zebra_consensus::router::init_checkpoint_list(config.consensus.clone(), network);
        let (_state, read_state, _latest_chain_tip, _chain_tip_change, _block_writer) =
            zebra_state::init(state_config, network, max_checkpoint_height, 0).await;

        let prefix = read_state.chain_prefix(self.height.map(Height))?;
        zebra_crosslink::chain_prefix::write_prefix_file(
            &network.crosslink_parameters(),
            &prefix,
            self.bft,
            &self.output,
        )
    }
}
