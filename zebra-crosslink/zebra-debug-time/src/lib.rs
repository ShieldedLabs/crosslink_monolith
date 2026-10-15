//! Debug-only wall-clock dilation.
//!
//! A test network cannot wait a real week to see what the chain does after a
//! week. This crate lets every node in such a network agree to *pretend* the
//! clock runs fast: real time up to a shared instant, then `multiplier` seconds
//! of apparent time per real second after it.
//!
//! ```text
//!   apparent
//!      ^                 /
//!      |               /   slope = multiplier
//!      |             /
//!      |___________/
//!      |         .
//!      |       .   slope = 1
//!      |     .
//!      +---------------------> real
//!              start
//! ```
//!
//! The map is continuous at `start` (no jump), strictly increasing, and a pure
//! function of `(start, multiplier, real time)` — so two nodes with the same
//! config and NTP-synced clocks compute the same apparent time, which is what
//! makes a *distributed* fast-forward possible at all.
//!
//! # What this is allowed to touch
//!
//! Only consensus and business logic: the clock reads that decide what a block
//! header's timestamp should be, whether a received header's timestamp is
//! acceptable, and how far the tip is from "now". Transport must keep reading
//! the real clock — peer attestations, address-book liveness, handshake nonces
//! and every `Instant`-based timeout are about the physical network, which does
//! not speed up just because we said so. Call [`now`] from the former and plain
//! `Utc::now()` from the latter; the split is deliberate and per-call-site, so
//! `rg zebra_debug_time::` is the list of clock reads that were opted in.
//!
//! # Operational notes
//!
//! - Every node on the network must carry the identical `[debug_time_dilation]`
//!   section, including across restarts: a node that forgets it sees the chain's
//!   timestamps as hours in the future and rejects every block.
//! - Clock skew is multiplied too. Zcash rejects headers more than two hours
//!   ahead of local time, so the usable multiplier is bounded by
//!   `7200 / real_clock_skew_in_seconds`.
//! - The proof-of-work difficulty adjustment reads header timestamps, so it sees
//!   blocks arriving `multiplier` times slower than target and lowers difficulty
//!   accordingly, until the real block rate catches up (or difficulty hits the
//!   network minimum, which caps how much of the speed-up is real).

use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Time dilation settings, as supplied in the node config.
///
/// Zero is off: the default is `multiplier = 0`, which leaves the clock alone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The real UNIX time, in seconds, at which apparent time starts running
    /// fast. Before it, apparent time is real time.
    pub start_unix_time: i64,

    /// How many apparent seconds pass per real second after `start_unix_time`.
    /// `0` or `1` disables dilation.
    pub multiplier: i64,
}

/// Apparent time equals real time at and before this real UNIX time, in seconds.
static START_UNIX_TIME: AtomicI64 = AtomicI64::new(0);

/// Apparent seconds per real second after [`START_UNIX_TIME`]. `1` is off.
static MULTIPLIER: AtomicI64 = AtomicI64::new(1);

/// Installs `config` as the process-wide dilation. Call once, at config load,
/// before anything reads a clock. A `multiplier` below 2, or an unset (zero)
/// start, installs nothing — dilating from the epoch would land the chain
/// thousands of years in the future, which is never what anyone meant.
pub fn install(config: &Config) {
    if config.multiplier < 2 || config.start_unix_time <= 0 {
        return;
    }

    START_UNIX_TIME.store(config.start_unix_time, Ordering::Relaxed);
    MULTIPLIER.store(config.multiplier, Ordering::Relaxed);
}

/// Whether dilation is installed. Only for diagnostics — the map is the identity
/// when it is not, so callers never need to branch on this.
pub fn is_active() -> bool {
    MULTIPLIER.load(Ordering::Relaxed) > 1
}

/// A time supplied from outside, in whole UNIX seconds; `0` is none. A test that sets it decides
/// what "now" is for every opted-in clock read, so its outcome does not depend on when it runs.
static SUPPLIED_UNIX_TIME: AtomicI64 = AtomicI64::new(0);

/// Makes [`now`] answer `unix_time` (whole seconds) until it is set again; `0` returns to the
/// real clock. The supplied time does not advance by itself. The timers are not affected: they
/// wait in real time.
pub fn supply_time(unix_time: i64) {
    SUPPLIED_UNIX_TIME.store(unix_time, Ordering::Relaxed);
}

/// The current apparent time.
pub fn now() -> DateTime<Utc> {
    let supplied = SUPPLIED_UNIX_TIME.load(Ordering::Relaxed);
    if supplied != 0 {
        if let Some(time) = DateTime::from_timestamp(supplied, 0) {
            return time;
        }
    }
    dilate(Utc::now())
}

/// The current apparent time, as whole seconds since the UNIX epoch.
pub fn now_unix_time() -> i64 {
    now().timestamp()
}

/// Converts a duration measured in apparent time into the real duration it takes
/// to elapse.
///
/// This is the conversion the [timers](#timers) are built from. Prefer those at a
/// call site that is about to wait: `real_duration` returning a bare `Duration`
/// invites the arithmetic to be written out next to the thing being waited on,
/// which is how a dilation-aware wait comes to look like a magic division.
pub fn real_duration(apparent: std::time::Duration) -> std::time::Duration {
    let multiplier = MULTIPLIER.load(Ordering::Relaxed);
    if multiplier < 2 {
        return apparent;
    }

    apparent / multiplier.min(u32::MAX.into()) as u32
}

/// Maps a real instant to its apparent one. The identity before the start
/// instant, and when dilation is not installed.
pub fn dilate(real: DateTime<Utc>) -> DateTime<Utc> {
    let multiplier = i128::from(MULTIPLIER.load(Ordering::Relaxed));
    if multiplier < 2 {
        return real;
    }

    let start = i128::from(START_UNIX_TIME.load(Ordering::Relaxed)) * 1_000_000;
    let real_micros = i128::from(real.timestamp_micros());
    if real_micros <= start {
        return real;
    }

    // i128 throughout: at a large multiplier, a long-running network's apparent
    // time overruns i64 microseconds (year 294247) long before anyone notices.
    let apparent = start + (real_micros - start) * multiplier;
    match i64::try_from(apparent).ok().and_then(DateTime::from_timestamp_micros) {
        Some(apparent) => apparent,
        // Saturate rather than panic: this is a debug knob, and a node that
        // dies here dies in the middle of block verification.
        None => DateTime::<Utc>::MAX_UTC,
    }
}

// # Timers
//
// Waits expressed in *chain* time. A duration like `BLOCK_TEMPLATE_REFRESH_LIMIT`
// is a statement about how fast the chain changes, not about the wall clock, so a
// dilated network has to wait proportionally less of it. These do that conversion
// so no call site has to: `sleep(BLOCK_TEMPLATE_REFRESH_LIMIT)` says what it means,
// where `tokio::time::sleep(real_duration(BLOCK_TEMPLATE_REFRESH_LIMIT))` says how
// it is implemented.
//
// They are plain constructors returning tokio's own types, so they drop into
// `select!`, `OptionFuture` and everywhere else those already go.

/// Sleeps for `apparent` of chain time.
#[cfg(feature = "timers")]
pub fn sleep(apparent: std::time::Duration) -> tokio::time::Sleep {
    tokio::time::sleep(real_duration(apparent))
}

/// The instant `apparent` of chain time from now, for `tokio::time::sleep_until`.
#[cfg(feature = "timers")]
pub fn deadline(apparent: std::time::Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + real_duration(apparent)
}

/// A timer that ticks once per `apparent` of chain time.
///
/// Falling behind does not bank credit: a tick missed because the work between
/// ticks overran is skipped rather than fired immediately, so a slow patch cannot
/// be followed by a burst. As with `tokio::time::interval`, the first tick
/// completes immediately.
#[cfg(feature = "timers")]
pub fn interval(apparent: std::time::Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(real_duration(apparent));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

// # Supplied instants
//
// The timers above wait in real time, which is right for a node and wrong for a test that has to
// decide when a step times out. A test can instead supply the monotonic clock: [`instant_now`]
// then reads a start instant plus whatever the test has advanced it by, and nothing else moves it.
// Timeouts compare their deadline against this reading when the owning loop next looks, so they
// fire on the test's schedule however fast the loop spins.
//
// On a node nothing is ever supplied, and the read costs one load on top of
// `Instant::now()` (an acquire load, so it sees the base its supplied reading was taken from; on
// x86 that is a plain load).

/// How far the supplied instant has been advanced past [`INSTANT_BASE`], in nanoseconds;
/// `u64::MAX` is none, and [`instant_now`] reads the real clock.
#[cfg(feature = "timers")]
static SUPPLIED_INSTANT_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// The real instant the supplied clock started from.
#[cfg(feature = "timers")]
static INSTANT_BASE: std::sync::OnceLock<tokio::time::Instant> = std::sync::OnceLock::new();

/// The current monotonic instant: the real one, or the supplied one once [`advance_instant`] has
/// been called.
#[cfg(feature = "timers")]
pub fn instant_now() -> tokio::time::Instant {
    let supplied = SUPPLIED_INSTANT_NANOS.load(Ordering::Acquire);
    if supplied == u64::MAX {
        return tokio::time::Instant::now();
    }
    *INSTANT_BASE.get().expect("set before the first supplied reading") + std::time::Duration::from_nanos(supplied)
}

/// Moves the supplied instant forward by `by`. The first call stops [`instant_now`] following the
/// real clock: it reads the real instant of that call, plus `by`, and from then on only what later
/// calls add.
#[cfg(feature = "timers")]
pub fn advance_instant(by: std::time::Duration) {
    INSTANT_BASE.get_or_init(tokio::time::Instant::now);
    let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX - 1);
    let _ = SUPPLIED_INSTANT_NANOS.fetch_update(Ordering::Release, Ordering::Relaxed, |nanos| {
        Some(if nanos == u64::MAX { by } else { nanos.saturating_add(by).min(u64::MAX - 1) })
    });
}

/// Whether a test supplies the monotonic clock.
#[cfg(feature = "timers")]
pub fn instant_is_supplied() -> bool {
    SUPPLIED_INSTANT_NANOS.load(Ordering::Relaxed) != u64::MAX
}

#[cfg(all(test, feature = "timers"))]
mod tests {
    use super::*;

    #[test]
    fn a_supplied_instant_moves_only_when_advanced() {
        let before = instant_now();
        advance_instant(std::time::Duration::from_secs(5));
        let first = instant_now();
        assert!(first >= before + std::time::Duration::from_secs(5));

        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(instant_now(), first, "real time passing must not move a supplied instant");

        advance_instant(std::time::Duration::from_millis(250));
        assert_eq!(instant_now(), first + std::time::Duration::from_millis(250));
        assert!(instant_is_supplied());
    }
}
