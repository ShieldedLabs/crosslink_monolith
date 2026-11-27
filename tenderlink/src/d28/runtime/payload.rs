//! Runtime-only payload transport. Consensus still receives complete frames.
use super::{AtomicMessageCounts, MessageCounts, saturating_add};
use std::sync::atomic::{AtomicU64, Ordering};

/// Payload transport is opt-in; ledger bytes, body IDs and signatures do not change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PayloadMode {
    #[default]
    Inline,
    Outlined,
    /// Reserved for the separately validated chunked transport implementation.
    Merkle { leaf_bytes: usize },
}

/// Cumulative application offers, not transport delivery or peer custody.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PayloadMetricsSnapshot {
    pub body_cache_hits: u64,
    pub body_cache_misses: u64,
    pub body_requests: u64,
    pub body_responses: u64,
    /// Raw body bytes offered per recipient, including bodies in inline frames.
    /// A subset of encoded evidence/data bytes; never add it to total wire bytes.
    pub payload_bytes: u64,
    /// Locally known repeat raw-byte offers of the most recently offered body to
    /// that peer in the current height/session. Bounded one-ID history per peer;
    /// not a count of global duplicates, received duplicates, or delivery.
    pub duplicate_payload_bytes: u64,
    /// Subset of `offered.evidence.bytes`, never additional wire bytes.
    pub outlined_evidence_bytes: u64,
    /// Body request/ACK application records, separate from evidence and status.
    pub offered_control: MessageCounts,
    /// Encoded body responses, separate from evidence and status.
    pub offered_data: MessageCounts,
}

#[derive(Debug, Default)]
pub(super) struct AtomicPayloadMetrics {
    body_cache_hits: AtomicU64,
    body_cache_misses: AtomicU64,
    body_requests: AtomicU64,
    body_responses: AtomicU64,
    payload_bytes: AtomicU64,
    duplicate_payload_bytes: AtomicU64,
    outlined_evidence_bytes: AtomicU64,
    offered_control: AtomicMessageCounts,
    offered_data: AtomicMessageCounts,
}
impl AtomicPayloadMetrics {
    pub(super) fn snapshot(&self) -> PayloadMetricsSnapshot {
        PayloadMetricsSnapshot {
            body_cache_hits: self.body_cache_hits.load(Ordering::Relaxed),
            body_cache_misses: self.body_cache_misses.load(Ordering::Relaxed),
            body_requests: self.body_requests.load(Ordering::Relaxed),
            body_responses: self.body_responses.load(Ordering::Relaxed),
            payload_bytes: self.payload_bytes.load(Ordering::Relaxed),
            duplicate_payload_bytes: self.duplicate_payload_bytes.load(Ordering::Relaxed),
            outlined_evidence_bytes: self.outlined_evidence_bytes.load(Ordering::Relaxed),
            offered_control: self.offered_control.snapshot(),
            offered_data: self.offered_data.snapshot(),
        }
    }
}
