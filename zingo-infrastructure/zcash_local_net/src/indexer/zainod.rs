//! The Zainod executable support struct and associated.

use std::{path::PathBuf, process::Child};

use getset::{CopyGetters, Getters};
use portpicker::Port;
use tempfile::TempDir;

use zebra_chain::parameters::NetworkKind;

use crate::logs::LogsToDir;
use crate::logs::LogsToStdoutAndStderr as _;
use crate::{
    config,
    error::LaunchError,
    indexer::{Indexer, IndexerConfig},
    launch,
    logs::{self},
    network::{self},
    process::Process,
    utils::executable_finder::{pick_command, EXPECT_SPAWN},
    ProcessId,
};

/// Zainod configuration
///
/// If `listen_port` is `None`, a port is picked at random between 15000-25000.
///
/// The `validator_port` must be specified and the validator process must be running before launching Zainod.
///
/// `network` must match the configured network of the validator.
#[derive(Debug)]
pub struct ZainodConfig {
    /// Listen RPC port
    pub listen_port: Option<Port>,
    /// Validator RPC port
    pub validator_port: Port,
    /// Chain cache path
    pub chain_cache: Option<PathBuf>,
    /// Network type.
    pub network: NetworkKind,
}

impl Default for ZainodConfig {
    fn default() -> Self {
        ZainodConfig {
            listen_port: None,
            validator_port: 0,
            chain_cache: None,
            network: NetworkKind::Regtest,
        }
    }
}

impl IndexerConfig for ZainodConfig {
    fn setup_validator_connection<V: crate::validator::Validator>(&mut self, validator: &V) {
        self.validator_port = validator.get_port();
    }

    fn set_listen_port(&mut self, indexer_listen_port: Option<Port>) {
        self.listen_port = indexer_listen_port;
    }
}

/// This struct is used to represent and manage the Zainod process.
#[derive(Debug, Getters, CopyGetters)]
#[getset(get = "pub")]
pub struct Zainod {
    /// Child process handle
    handle: Child,
    /// RPC port
    #[getset(skip)]
    #[getset(get_copy = "pub")]
    port: Port,
    /// Logs directory
    logs_dir: TempDir,
    /// Config directory
    config_dir: TempDir,
    _data_dir: Option<TempDir>,
}

impl LogsToDir for Zainod {
    fn logs_dir(&self) -> &TempDir {
        &self.logs_dir
    }
}

impl Process for Zainod {
    const PROCESS: ProcessId = ProcessId::Zainod;

    type Config = ZainodConfig;

    async fn launch(config: Self::Config) -> Result<Self, LaunchError> {
        let logs_dir = tempfile::tempdir().unwrap();

        let port = network::pick_unused_port(config.listen_port);
        let config_dir = tempfile::tempdir().unwrap();

        let (cache_dir, data_dir) = if let Some(cache) = config.chain_cache.clone() {
            (cache, None)
        } else {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            (path, Some(dir))
        };

        let config_file_path = config::write_zainod_config(
            config_dir.path(),
            cache_dir,
            port,
            config.validator_port,
            config.network,
        )
        .unwrap();

        let mut command = pick_command("zainod");
        command
            .args([
                "--config",
                config_file_path.to_str().expect("should be valid UTF-8"),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut handle = command.spawn().expect(EXPECT_SPAWN);
        logs::write_logs(&mut handle, &logs_dir);
        launch::wait(
            ProcessId::Zainod,
            &mut handle,
            &logs_dir,
            None,
            &["Zaino Indexer started successfully."],
            &["Error:"],
            &[],
        )?;

        Ok(Zainod {
            handle,
            port,
            logs_dir,
            config_dir,
            _data_dir: data_dir,
        })
    }

    fn stop(&mut self) {
        self.handle.kill().expect("zainod couldn't be killed");
    }

    fn print_all(&self) {
        self.print_stdout();
        self.print_stderr();
    }
}

impl Indexer for Zainod {
    fn listen_port(&self) -> Port {
        self.port
    }
}

impl Drop for Zainod {
    fn drop(&mut self) {
        self.stop();
    }
}
