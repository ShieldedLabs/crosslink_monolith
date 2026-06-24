//! Holds database migration tests.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, DatabaseSize, Network, StorageConfig};

use crate::chain_index::finalised_state::capability::DbCore as _;
use crate::chain_index::finalised_state::db::DbBackend;
use crate::chain_index::finalised_state::ZainoDB;
use crate::chain_index::source::test::MockchainSource;
use crate::chain_index::tests::init_tracing;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, load_test_vectors, TestVectorData,
};
use crate::BlockCacheConfig;

async fn spawn_zaino_db_with_retry(
    config: &BlockCacheConfig,
    source: &MockchainSource,
) -> ZainoDB {
    let mut attempts = 0;
    loop {
        match ZainoDB::spawn(config.clone(), source.clone()).await {
            Ok(db) => return db,
            Err(e) if attempts < 10 => {
                attempts += 1;
                let delay = std::time::Duration::from_millis(200 * (1 << attempts));
                tracing::warn!(
                    "ZainoDB::spawn failed (attempt {}/10): {}. Retrying in {:?}...",
                    attempts, e, delay
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => panic!("Failed to open ZainoDB after {} attempts: {}", attempts, e),
        }
    }
}

async fn spawn_v1_backend_with_retry(config: &BlockCacheConfig) -> DbBackend {
    let mut attempts = 0;
    loop {
        match DbBackend::spawn_v1(config).await {
            Ok(db) => return db,
            Err(e) if attempts < 10 => {
                attempts += 1;
                let delay = std::time::Duration::from_millis(200 * (1 << attempts));
                tracing::warn!(
                    "DbBackend::spawn_v1 failed (attempt {}/10): {}. Retrying in {:?}...",
                    attempts, e, delay
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => panic!(
                "Failed to open DbBackend after {} attempts: {}",
                attempts, e
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_full() {
    init_tracing();

    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                size: DatabaseSize::Gb(1),
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                size: DatabaseSize::Gb(1),
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        blocks.clone(),
        None,
    )
    .await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    // Open v1 database and check migration (retry for Windows LMDB).
    let zaino_db_2 = spawn_zaino_db_with_retry(&v1_config, &source).await;
    zaino_db_2.wait_until_ready().await;
    dbg!(zaino_db_2.status());
    let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();
    assert_eq!(db_height.0, 200);
    dbg!(zaino_db_2.shutdown().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_interrupted() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                size: DatabaseSize::Gb(1),
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                size: DatabaseSize::Gb(1),
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        blocks.clone(),
        None,
    )
    .await;
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    // Partial build v1 database (retry for Windows LMDB).
    let zaino_db = spawn_v1_backend_with_retry(&v1_config).await;
    crate::chain_index::tests::vectors::sync_db_with_blockdata(&zaino_db, blocks.clone(), Some(50))
        .await;

    dbg!(zaino_db.shutdown().await.unwrap());

    // Open v1 database and check migration (retry for Windows LMDB).
    let zaino_db_2 = spawn_zaino_db_with_retry(&v1_config, &source).await;
    zaino_db_2.wait_until_ready().await;
    dbg!(zaino_db_2.status());
    let db_height = dbg!(zaino_db_2.db_height().await.unwrap()).unwrap();
    assert_eq!(db_height.0, 200);
    dbg!(zaino_db_2.shutdown().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn v0_to_v1_partial() {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let v0_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path.clone(),
                size: DatabaseSize::Gb(1),
            },
            ..Default::default()
        },
        db_version: 0,
        network: Network::Regtest(ActivationHeights::default()),
    };
    let v1_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                size: DatabaseSize::Gb(1),
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let source = build_mockchain_source(blocks.clone());

    // Build v0 database.
    let zaino_db = ZainoDB::spawn(v0_config, source.clone()).await.unwrap();
    crate::chain_index::tests::vectors::sync_db_with_blockdata(
        zaino_db.router(),
        blocks.clone(),
        None,
    )
    .await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());

    // Open v1 database (retry for Windows LMDB file mapping release).
    let zaino_db = spawn_zaino_db_with_retry(&v1_config, &source).await;
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status());
    dbg!(zaino_db.db_height().await.unwrap());
    dbg!(zaino_db.shutdown().await.unwrap());
}
