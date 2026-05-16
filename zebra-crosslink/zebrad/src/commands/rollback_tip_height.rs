//! `rollback-tip-height` subcommand - rebuilds Zebra's persisted chain state to a height.
//!
//! This command avoids mutating finalized RocksDB column families in place. Instead, it replays
//! verified blocks from the current state into a fresh state directory up to the requested height,
//! then atomically swaps that rebuilt state into the active state path.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use abscissa_core::{Application, Command, Runnable};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};

use zebra_chain::parameters::Network;
use zebra_state::{constants::STATE_DATABASE_KIND, state_database_format_version_in_code};

use crate::{
    components::tokio::{RuntimeRun, TokioComponent},
    prelude::APPLICATION,
};

use super::copy_state::CopyStateCmd;

/// Rebuild Zebra's cached chain state so its persisted tip is at or below a height.
#[derive(Command, Debug, Default, Parser)]
pub struct RollbackTipHeightCmd {
    /// The maximum persisted tip height to keep.
    #[clap(long, short = 'H', help = "rebuild state up to this block height")]
    height: u32,

    /// Path to Zebra's cached state.
    #[clap(long, short, help = "path to directory with the Zebra chain state")]
    cache_dir: Option<PathBuf>,

    /// The network whose state should be rebuilt.
    #[clap(long, short, help = "the network of the chain to rebuild")]
    network: Option<Network>,
}

impl Runnable for RollbackTipHeightCmd {
    /// `rollback-tip-height` sub-command entrypoint.
    fn run(&self) {
        info!(height = self.height, "starting cached chain state rollback");

        let rt = APPLICATION
            .state()
            .components_mut()
            .get_downcast_mut::<TokioComponent>()
            .expect("TokioComponent should be available")
            .rt
            .take();

        rt.expect("runtime should not already be taken")
            .run(self.start());
    }
}

impl RollbackTipHeightCmd {
    /// Rebuild the active state directory to the requested height.
    async fn start(&self) -> Result<()> {
        let base_config = APPLICATION.config();
        let network = self
            .network
            .as_ref()
            .unwrap_or(&base_config.network.network);

        let mut source_config = base_config.state.clone();
        if let Some(cache_dir) = self.cache_dir.clone() {
            source_config.cache_dir = cache_dir;
        }

        if source_config.ephemeral {
            return Err(eyre!(
                "rollback-tip-height requires a persistent state cache, but state.ephemeral is true"
            ));
        }

        let source_cache_dir = source_config.cache_dir.clone();
        let active_db_path = state_db_path(&source_config, network);
        if !active_db_path.exists() {
            return Err(eyre!(
                "active state directory does not exist: {}",
                active_db_path.display()
            ));
        }

        let temp_cache_dir = source_config
            .cache_dir
            .join(unique_dir_name(self.height, "tmp"));
        let backup_db_path = active_db_path.with_file_name(unique_dir_name(self.height, "backup"));

        if temp_cache_dir.exists() {
            return Err(eyre!(
                "temporary rollback directory already exists: {}",
                temp_cache_dir.display()
            ));
        }
        if backup_db_path.exists() {
            return Err(eyre!(
                "backup state directory already exists: {}",
                backup_db_path.display()
            ));
        }

        fs::create_dir_all(&temp_cache_dir).wrap_err_with(|| {
            format!(
                "failed to create temporary rollback cache directory: {}",
                temp_cache_dir.display()
            )
        })?;

        let mut target_config = source_config.clone();
        target_config.cache_dir = temp_cache_dir.clone();
        target_config.ephemeral = false;
        target_config.delete_old_database = false;

        let target_db_path = state_db_path(&target_config, network);

        info!(
            height = self.height,
            active_db_path = %active_db_path.display(),
            target_db_path = %target_db_path.display(),
            backup_db_path = %backup_db_path.display(),
            "rebuilding state into temporary directory"
        );

        CopyStateCmd::copy_state_to_height(
            network,
            source_config,
            target_config,
            Some(self.height),
            None,
        )
        .await
        .map_err(|error| eyre!(error))?;

        if !target_db_path.exists() {
            return Err(eyre!(
                "rebuilt state directory was not created: {}",
                target_db_path.display()
            ));
        }

        install_rebuilt_state(&active_db_path, &target_db_path, &backup_db_path)?;
        reset_tip_dependent_caches(&source_cache_dir, self.height)?;

        if let Err(error) = fs::remove_dir_all(&temp_cache_dir) {
            warn!(
                ?error,
                temp_cache_dir = %temp_cache_dir.display(),
                "failed to remove temporary rollback cache directory"
            );
        }

        warn!(
            backup_db_path = %backup_db_path.display(),
            "old state directory was preserved as a rollback backup"
        );
        info!(height = self.height, "finished cached chain state rollback");

        Ok(())
    }
}

fn reset_tip_dependent_caches(cache_dir: &Path, height: u32) -> Result<()> {
    // These caches contain data derived from the previous tip. Keeping them after a rollback can
    // make wallet sync and Zaino observe a chain that no longer exists in the active state DB.
    move_as_backup_if_exists(
        &cache_dir.join("wallet.snapshot"),
        &unique_dir_name(height, "wallet-snapshot-backup"),
    )?;

    remove_dir_if_exists(&cache_dir.join("zaino"))?;

    Ok(())
}

fn move_as_backup_if_exists(path: &Path, backup_name: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let backup_path = path.with_file_name(backup_name);
    fs::rename(path, &backup_path).wrap_err_with(|| {
        format!(
            "failed to move {} to backup {}",
            path.display(),
            backup_path.display()
        )
    })?;

    warn!(
        path = %path.display(),
        backup_path = %backup_path.display(),
        "moved tip-dependent wallet snapshot out of the active cache"
    );

    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {
            warn!(
                path = %path.display(),
                "removed tip-dependent cache directory"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "failed to remove tip-dependent cache directory {}",
                path.display()
            )
        }),
    }
}

fn state_db_path(config: &zebra_state::Config, network: &Network) -> PathBuf {
    config.db_path(
        STATE_DATABASE_KIND,
        state_database_format_version_in_code().major,
        network,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn install_rebuilt_state_swaps_active_state_and_keeps_backup() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir should be created");
        let active_db_path = temp_dir.path().join("active");
        let target_db_path = temp_dir.path().join("target");
        let backup_db_path = temp_dir.path().join("backup");

        fs::create_dir(&active_db_path).expect("active dir should be created");
        fs::write(active_db_path.join("old-state"), b"old").expect("old state should be written");
        fs::create_dir(&target_db_path).expect("target dir should be created");
        fs::write(target_db_path.join("new-state"), b"new").expect("new state should be written");

        install_rebuilt_state(&active_db_path, &target_db_path, &backup_db_path)
            .expect("rebuilt state should install");

        assert!(active_db_path.join("new-state").exists());
        assert!(backup_db_path.join("old-state").exists());
        assert!(!target_db_path.exists());
    }

    #[test]
    fn reset_tip_dependent_caches_moves_wallet_snapshot_and_removes_zaino() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir should be created");
        let cache_dir = temp_dir.path();
        let wallet_snapshot_path = cache_dir.join("wallet.snapshot");
        let zaino_cache_path = cache_dir.join("zaino");

        fs::write(&wallet_snapshot_path, b"snapshot").expect("wallet snapshot should be written");
        fs::create_dir(&zaino_cache_path).expect("zaino cache dir should be created");
        fs::write(zaino_cache_path.join("cache"), b"cache").expect("zaino cache should be written");

        reset_tip_dependent_caches(cache_dir, 42).expect("tip-dependent caches should reset");

        assert!(!wallet_snapshot_path.exists());
        assert!(!zaino_cache_path.exists());

        let wallet_backup_exists = fs::read_dir(cache_dir)
            .expect("cache dir should be readable")
            .filter_map(std::result::Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rollback-tip-height-wallet-snapshot-backup-42-")
            });
        assert!(wallet_backup_exists, "wallet snapshot backup should exist");
    }
}

fn install_rebuilt_state(
    active_db_path: &Path,
    target_db_path: &Path,
    backup_db_path: &Path,
) -> Result<()> {
    fs::rename(active_db_path, backup_db_path).wrap_err_with(|| {
        format!(
            "failed to move active state directory {} to backup {}",
            active_db_path.display(),
            backup_db_path.display()
        )
    })?;

    if let Err(error) = fs::rename(target_db_path, active_db_path) {
        let restore_result = fs::rename(backup_db_path, active_db_path);

        return Err(match restore_result {
            Ok(()) => eyre!(
                "failed to install rebuilt state from {} to {}; restored original state from backup: {error}",
                target_db_path.display(),
                active_db_path.display()
            ),
            Err(restore_error) => eyre!(
                "failed to install rebuilt state from {} to {}; also failed to restore backup {}: install error: {error}; restore error: {restore_error}",
                target_db_path.display(),
                active_db_path.display(),
                backup_db_path.display()
            ),
        });
    }

    Ok(())
}

fn unique_dir_name(height: u32, label: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    format!(
        "rollback-tip-height-{label}-{height}-{}-{timestamp}",
        std::process::id()
    )
}
