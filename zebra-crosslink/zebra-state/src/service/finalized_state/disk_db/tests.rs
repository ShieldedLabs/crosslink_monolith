//! Tests and test methods for low-level RocksDB access.

#![allow(clippy::unwrap_in_result)]
#![allow(dead_code)]

use std::ops::Deref;

use crate::service::finalized_state::disk_db::{DiskDb, DB};

// Enable older test code to automatically access the inner database via Deref coercion.
impl Deref for DiskDb {
    type Target = DB;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl DiskDb {
    /// Returns a list of column family names in this database.
    pub fn list_cf(&self) -> Result<Vec<String>, rocksdb::Error> {
        let opts = DiskDb::options();
        let path = self.path();

        rocksdb::DB::list_cf(&opts, path)
    }
}

/// Check that zs_iter_opts returns an upper bound one greater than provided inclusive end bounds.
#[test]
fn zs_iter_opts_increments_key_by_one() {
    let _init_guard = zebra_test::init();

    // TODO: add an empty key (`()` type or `[]` when serialized) test case
    let keys: [u32; 14] = [
        0,
        1,
        200,
        255,
        256,
        257,
        65535,
        65536,
        65537,
        16777215,
        16777216,
        16777217,
        16777218,
        u32::MAX,
    ];

    for key in keys {
        let (_, bytes) = DiskDb::zs_iter_bounds(&..=key.to_be_bytes().to_vec());
        let mut extra_bytes = bytes.expect("there should be an upper bound");
        let bytes = extra_bytes.split_off(extra_bytes.len() - 4);
        let upper_bound = u32::from_be_bytes(bytes.clone().try_into().expect("should be 4 bytes"));
        let expected_upper_bound = key.wrapping_add(1);

        assert_eq!(
            expected_upper_bound, upper_bound,
            "the upper bound should be 1 greater than the original key"
        );

        if expected_upper_bound == 0 {
            assert_eq!(
                extra_bytes,
                vec![1],
                "there should be an extra byte with a value of 1"
            );
        } else {
            assert_eq!(extra_bytes.len(), 0, "there should be no extra bytes");
        }
    }
}

#[cfg(test)]
mod wal_fuzz {
    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    use rand::{seq::SliceRandom, Rng, RngCore, SeedableRng};
    use rand_chacha::ChaCha20Rng;
    use zebra_chain::parameters::Network;

    use crate::{
        constants::{state_database_format_version_in_code, STATE_DATABASE_KIND},
        service::finalized_state::{disk_db::DiskDb, STATE_COLUMN_FAMILIES_IN_CODE},
        Config,
    };

    const MIB: u64 = 1 << 20;

    /// The write-ahead log files in `path`, oldest first, with the total of their sizes.
    fn write_ahead_logs(path: &Path) -> (Vec<u64>, u64) {
        let mut numbers = Vec::new();
        let mut total = 0;
        for entry in std::fs::read_dir(path).expect("database directory is readable").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(number) = name.strip_suffix(".log").and_then(|stem| stem.parse::<u64>().ok()) else { continue };
            // A log deleted since the directory was read is no longer there to count.
            let Ok(metadata) = entry.metadata() else { continue };
            numbers.push(number);
            total += metadata.len();
        }
        numbers.sort_unstable();
        (numbers, total)
    }

    fn put_random(db: &DiskDb, cf_name: &str, len: usize, rng: &mut ChaCha20Rng) {
        let cf = db.cf_handle(cf_name).expect("column family is open");
        let mut key = [0u8; 16];
        rng.fill_bytes(&mut key);
        // Runs of one random byte: the logs keep values as written, so they grow at full size,
        // while flushed tables compress them away and the disk does not fill with them.
        let value = vec![rng.gen::<u8>(); len];
        db.put_cf(&cf, key, value).expect("write succeeds");
    }

    /// A write-ahead log file is deleted only once every column family with data in it has
    /// flushed, and a family that gets only a few small writes never fills its write buffer. So
    /// without a cap the logs that the busy families write grow until the disk fills, and the
    /// oldest is never deleted while the node runs.
    ///
    /// Every run draws a fresh seed (set `CROSSLINK_FUZZ_SEED` to replay one): which families are
    /// busy, which quiet ones hold data in the first log, which get small writes later and when,
    /// and every key, value size and value.
    #[test]
    fn write_ahead_logs_stay_bounded_however_writes_fall_across_column_families() {
        let _init_guard = zebra_test::init();
        let seed: u64 = std::env::var("CROSSLINK_FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| rand::thread_rng().next_u64());
        eprintln!("CROSSLINK_FUZZ_SEED={seed}");
        let mut rng = ChaCha20Rng::seed_from_u64(seed);

        let db = DiskDb::new(
            &Config::ephemeral(),
            STATE_DATABASE_KIND,
            &state_database_format_version_in_code(),
            &Network::Mainnet,
            STATE_COLUMN_FAMILIES_IN_CODE.iter().map(ToString::to_string),
            false,
        )
        .expect("an ephemeral database opens");

        let mut families: Vec<&str> = STATE_COLUMN_FAMILIES_IN_CODE.to_vec();
        families.shuffle(&mut rng);
        let busy_n = rng.gen_range(1..=3);
        let (busy, quiet) = families.split_at(busy_n);
        let mut pinning: Vec<&str> = quiet.iter().copied().filter(|_| rng.gen_bool(0.3)).collect();
        if pinning.is_empty() {
            pinning.push(quiet[rng.gen_range(0..quiet.len())]);
        }
        for cf_name in &pinning {
            let len = rng.gen_range(1..=4096);
            put_random(&db, cf_name, len, &mut rng);
        }
        let (first_logs, _) = write_ahead_logs(db.path());
        // The quiet writes went to the log being written, the newest; any older one may be empty.
        let pinned = *first_logs.last().expect("the quiet writes are in a log");

        let cap = DiskDb::MAX_TOTAL_WAL_BYTES;
        let target = cap + rng.gen_range(256..=512) * MIB;
        let quiet_every = rng.gen_range(16..=1024);
        let (mut written, mut puts, mut peak) = (0, 0u64, 0);
        while written < target {
            let len = rng.gen_range(1024..=256 * 1024);
            put_random(&db, busy[rng.gen_range(0..busy.len())], len, &mut rng);
            written += len as u64;
            puts += 1;
            if rng.gen_range(0..quiet_every) == 0 {
                let len = rng.gen_range(1..=4096);
                put_random(&db, quiet[rng.gen_range(0..quiet.len())], len, &mut rng);
            }
            if puts % 64 == 0 {
                peak = peak.max(write_ahead_logs(db.path()).1);
            }
        }

        // Flushes and log deletions run in the background, so give them time to finish.
        let deadline = Instant::now() + Duration::from_secs(60);
        let (logs, total) = loop {
            let (logs, total) = write_ahead_logs(db.path());
            if (!logs.contains(&pinned) && total <= cap + 256 * MIB) || Instant::now() > deadline {
                break (logs, total);
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let case = format!(
            "seed {seed}: busy {busy:?}, {} quiet families in the first log, a quiet write every ~{quiet_every} puts, \
             {} MiB written in {puts} puts",
            pinning.len(),
            written / MIB,
        );
        assert!(!logs.contains(&pinned), "log {pinned}, holding the quiet writes, outlived {} MiB of writes; {case}", written / MIB);
        assert!(total <= cap + 256 * MIB, "{} MiB of logs left after the writes; {case}", total / MIB);
        assert!(peak <= cap + 512 * MIB, "{} MiB of logs at the peak; {case}", peak / MIB);
    }
}
