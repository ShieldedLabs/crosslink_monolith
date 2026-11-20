//! Agreed application persistence boundary; deliberately no filesystem backend
//! or state-image serializer in this baseline.
use super::Result;

/// A single-writer store of one fixed-size committed block.
///
/// `block_size()` is fixed for the lifetime of the store. The application picks
/// its maximum validator count and maximum body size before allocating the
/// block; changing the live roster must not change the block size. Every call
/// to `load` and `replace` must use **exactly** this size, otherwise return an
/// error without modifying the committed block.
///
/// `load` returns `Ok(false)` only for an uninitialized store and `Ok(true)`
/// after filling the entire buffer with the last committed block. Corruption
/// or I/O failure is an error, never an uninitialized result.
///
/// A successful `replace` durably commits the entire newer block. After a
/// crash, `load` must return either the complete old block or the complete new
/// block, never a mixture. Replacement does not resize or append history. Only
/// one writer may operate on a store. On any replacement error the outcome may
/// be ambiguous: the caller must stop signing/sending, discard its working
/// image, and reload the committed block before resuming consensus. A failed
/// write must never be treated as permission to send.
///
/// This is the public physical persistence contract, **not** a WAL request.
/// The baseline's `Storage`/`MemoryStore` implement logical bounded workspace
/// and deterministic publication/crash tests. An adapter from that workspace
/// to this block API, its bounded image encoding, and a durable backend are
/// intentionally deferred. `MemoryStore` alone is not durable storage.
pub trait AtomicBlockStore {
    fn block_size(&self) -> usize;
    fn load(&mut self, block: &mut [u8]) -> Result<bool>;
    fn replace(&mut self, next: &[u8]) -> Result<()>;
}
