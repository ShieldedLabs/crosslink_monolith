//! Fixed-size, opt-in wall-clock observations. Never used by protocol timers.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Cumulative completed timing samples, not CPU time. Durations use monotonic
/// `Instant` wall nanoseconds, include scheduling/preemption, and saturate.
/// Independent relaxed reads are not a transactional snapshot. A sample is added
/// on phase exit/completion: work spanning a snapshot boundary belongs to its
/// ending interval. Nested phases overlap, as do runtimes sharing one counter:
/// do not add them as elapsed time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallTimeSnapshot {
    pub calls: u64,
    pub wall_ns: u64,
}

#[derive(Debug, Default)]
pub(super) struct AtomicWallTime {
    calls: AtomicU64,
    wall_ns: AtomicU64,
}
impl AtomicWallTime {
    pub(super) fn snapshot(&self) -> WallTimeSnapshot {
        WallTimeSnapshot {
            calls: self.calls.load(Ordering::Relaxed),
            wall_ns: self.wall_ns.load(Ordering::Relaxed),
        }
    }

    pub(super) fn record(&self, elapsed: Duration) {
        add(
            &self.wall_ns,
            elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
        );
        add(&self.calls, 1);
    }

    pub(super) fn start(&self) -> WallTimer<'_> {
        WallTimer {
            counter: self,
            start: Instant::now(),
        }
    }
}

/// The disabled path does not read a clock or touch an atomic.
pub(super) fn measure<T>(counter: Option<&AtomicWallTime>, work: impl FnOnce() -> T) -> T {
    let _timer = counter.map(AtomicWallTime::start);
    work()
}

pub(super) struct WallTimer<'a> {
    counter: &'a AtomicWallTime,
    start: Instant,
}
impl Drop for WallTimer<'_> {
    fn drop(&mut self) {
        self.counter.record(self.start.elapsed());
    }
}
fn add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(amount))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_metrics_default_zero_and_saturate_without_wrapping() {
        let counter = AtomicWallTime::default();
        assert_eq!(counter.snapshot(), WallTimeSnapshot::default());
        counter.record(Duration::ZERO);
        assert_eq!(
            counter.snapshot(),
            WallTimeSnapshot {
                calls: 1,
                wall_ns: 0
            }
        );
        counter.calls.store(u64::MAX - 1, Ordering::Relaxed);
        counter.record(Duration::MAX);
        counter.record(Duration::from_nanos(1));
        assert_eq!(
            counter.snapshot(),
            WallTimeSnapshot {
                calls: u64::MAX,
                wall_ns: u64::MAX
            }
        );
    }

    #[test]
    fn wall_metrics_record_on_scope_exit_including_errors() {
        let counter = AtomicWallTime::default();
        let fail = || -> Result<(), ()> {
            let _timer = counter.start();
            Err(())
        };
        assert!(fail().is_err());
        assert_eq!(counter.snapshot().calls, 1);
        let before = counter.snapshot();
        drop(counter.start());
        let after = counter.snapshot();
        assert_eq!(after.calls - before.calls, 1);
        assert!(after.wall_ns >= before.wall_ns);
    }
}
