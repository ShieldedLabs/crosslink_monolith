//! `fixup-db-stake` subcommand - repairs aggregated-stakes rows torn from their
//! blocks by the pre-atomic snapshot write. Also reachable as
//! `zebrad --fixup-db-stake`.

use std::path::PathBuf;

use abscissa_core::{Application, Command, Runnable};
use clap::Parser;

use crate::prelude::APPLICATION;

/// Check the state cache's aggregated-stakes rows and repair any missing ones
#[derive(Command, Debug, Default, Parser)]
pub struct FixupDbStakeCmd {
    /// Path to Zebra's cached state.
    #[clap(long, short, help = "path to directory with the Zebra chain state")]
    cache_dir: Option<PathBuf>,

    /// Cross-check every stored row against the replay even when none are missing.
    #[clap(
        long,
        help = "replay and cross-check every stored aggregated-stakes row \
                even when none are missing"
    )]
    verify: bool,
}

impl Runnable for FixupDbStakeCmd {
    /// `fixup-db-stake` sub-command entrypoint.
    #[allow(clippy::print_stderr)]
    fn run(&self) {
        let config = APPLICATION.config();

        let mut state_config = config.state.clone();
        if let Some(cache_dir) = self.cache_dir.clone() {
            state_config.cache_dir = cache_dir;
        }

        if let Err(error) = zebra_state::fixup_aggregated_stakes(
            &state_config,
            &config.network.network,
            self.verify,
        ) {
            eprintln!("fixup-db-stake failed: {error}");
            std::process::exit(1);
        }
    }
}
