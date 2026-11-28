//! Agreed application persistence boundary. [`super::durable`] implements the
//! bounded workspace image; applications supply the physical durable backend.
use super::{Error, Result};
use std::ops::Range;

/// Validate the structural part of a region replacement before any mutation.
/// The full image must have the exact fixed size, and the nonempty range list
/// must contain nonempty, ordered, disjoint, in-bounds ranges. Adjacent ranges
/// are allowed. This does not compare bytes outside the ranges: preserving those
/// bytes is the single owner's caller invariant, not an extra full-image scan.
pub fn validate_regions(block_size: usize, next: &[u8], changed: &[Range<usize>]) -> Result<()> {
    if next.len() != block_size || changed.is_empty() {
        return Err(Error::Storage);
    }
    let mut end = 0;
    for range in changed {
        if range.start < end || range.start >= range.end || range.end > block_size {
            return Err(Error::Storage);
        }
        end = range.end;
    }
    Ok(())
}

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
/// [`super::durable::DurableStore`] atomically replaces its bounded image on
/// every mutation in eager mode, or at explicit signing/publication/checkpoint
/// barriers in buffered mode. The backend contract is identical in both modes,
/// including exclusive writer ownership. `MemoryStore` alone remains only a
/// logical publication/crash model, not durable storage.
pub trait AtomicBlockStore {
    fn block_size(&self) -> usize;
    fn load(&mut self, block: &mut [u8]) -> Result<bool>;
    fn replace(&mut self, next: &[u8]) -> Result<()>;

    /// Atomically commit ALL selected ranges as one replacement transaction.
    /// `next` is the complete fixed-size new image, not packed range data. The
    /// single owner must ensure that every byte outside `changed` equals the
    /// committed image. On first initialization there is no old image: the
    /// backend must initialize the entire `next`, even if fewer ranges are given.
    ///
    /// Validate the exact size and every range before any mutation (see
    /// [`validate_regions`]). Success durably commits the complete new image.
    /// After a crash or an ambiguous error, `load` must yield the complete old
    /// image or the complete new image, never some of the changed ranges from
    /// each. Per-range atomic writes do NOT satisfy this contract. The same
    /// exclusive-owner and poison/reload rules as [`Self::replace`] apply.
    ///
    /// Backends without atomic batch support safely fall back to whole-image
    /// replacement. An override may copy/write only selected regions, but must
    /// not weaken the all-ranges old-or-new guarantee or retain unbounded history.
    fn replace_regions(&mut self, next: &[u8], changed: &[Range<usize>]) -> Result<()> {
        validate_regions(self.block_size(), next, changed)?;
        self.replace(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WholeImage {
        image: [u8; 8],
        writes: usize,
    }
    impl AtomicBlockStore for WholeImage {
        fn block_size(&self) -> usize {
            self.image.len()
        }
        fn load(&mut self, block: &mut [u8]) -> Result<bool> {
            if block.len() != self.image.len() {
                return Err(Error::Storage);
            }
            block.copy_from_slice(&self.image);
            Ok(true)
        }
        fn replace(&mut self, next: &[u8]) -> Result<()> {
            if next.len() != self.image.len() {
                return Err(Error::Storage);
            }
            self.image.copy_from_slice(next);
            self.writes += 1;
            Ok(())
        }
    }

    #[test]
    fn region_default_matches_whole_image_replacement() {
        let mut regions = WholeImage { image: [1; 8], writes: 0 };
        let mut whole = WholeImage { image: [1; 8], writes: 0 };
        let next = [1, 2, 3, 1, 1, 4, 5, 1];
        regions.replace_regions(&next, &[1..3, 5..7]).unwrap();
        whole.replace(&next).unwrap();
        assert_eq!(regions.image, whole.image);
        assert_eq!(regions.writes, 1);
        assert!(validate_regions(8, &next, &[0..3, 3..8]).is_ok());
    }

    #[test]
    fn malformed_regions_fail_before_default_replacement() {
        let mut store = WholeImage { image: [1; 8], writes: 0 };
        for bad in [
            vec![], vec![0..0], vec![4..3], vec![0..9], vec![8..9],
            vec![0..3, 2..4], vec![5..7, 1..3], vec![0..2, 4..4],
            vec![0..2, 3..9], vec![0..2, usize::MAX..usize::MAX],
        ] {
            assert_eq!(store.replace_regions(&[2; 8], &bad), Err(Error::Storage));
            assert_eq!(store.image, [1; 8]);
            assert_eq!(store.writes, 0);
        }
        for next in [&[2; 7][..], &[2; 9][..]] {
            assert_eq!(store.replace_regions(next, &[0..7]), Err(Error::Storage));
            assert_eq!(store.image, [1; 8]);
            assert_eq!(store.writes, 0);
        }
    }
}
