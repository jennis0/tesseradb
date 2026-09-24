//! The fold's mapped scatter for `entities/ext-locator.u32` (contracts §2.4, compaction §3 pass 3).
//!
//! **A sibling to [`crate::write::PermutationWriter`], not a reuse of it, and the reason is the
//! format.** `permutation.bin` carries a 16-byte header (magic, version, `bound`; contracts R4);
//! `ext-locator.u32` is contractually a raw array with **no header at all** — "one raw u32 array,
//! no header, no `<k>` suffix" (contracts §2.4). Widening `PermutationWriter` to make its header
//! optional would be exactly the format ambiguity `write.rs`'s own module doc warns against ("a
//! second writer that knows the layout is how two come to disagree"); a second, single-purpose
//! writer for a second, different byte format is the same choice this crate already made for
//! [`crate::write::RunWriter`] alongside `SegmentWriter`. It also keeps this change inside the
//! file compaction's pass 3 owns rather than editing `write.rs`, which a sibling change is mid-edit
//! on.
//!
//! **Sentinel-filled at create, for the reason [`crate::write::PermutationWriter`] states**: a
//! freshly extended file reads as zeros, and zero is ordinal 0 — a real position in a real run —
//! so an unfilled slot would answer a real key for every entity that never held one.
//! [`tessera_types::ROW_ABSENT`] (`0xFFFF_FFFF`) is the same sentinel `entities/ext-locator.u32`
//! already uses at build (contracts §2.4) and the one `coalesce`'s in-memory locator fills today;
//! this writer's only difference is where the bytes live while they are filled.
//!
//! **Why the fold needs this and an ordinary coalesce does not.** An ordinary coalesce's span is
//! bounded by the maintenance policy that caps external-id run count (contracts §2.4), so an
//! in-memory `Vec` costs nothing worth avoiding. The fold's span is the whole entity space — 4 GB
//! at 10⁹ (compaction §3) — and a `Vec` of that size is anonymous memory the kernel can only page
//! to swap; written through a mapping it is page cache, reclaimable under pressure, exactly as
//! `Permutation::load` already treats `permutation.bin` at read.

use std::fs::File;
use std::io;
use std::path::Path;

/// Writes a raw, headerless `u32` locator array through a mapping, scattering
/// `locator[slot] = ordinal` in any order — the fold's pass 3 learns a surviving pair's ordinal in
/// merge order, not entity order, and a sequential writer would have to buffer the whole array to
/// reorder it, which is the cost this avoids (the same argument `PermutationWriter` makes for
/// `permutation.bin`).
pub(crate) enum LocatorWriter {
    /// `span == 0`: no entity in range, so the array is the empty file contracts §2.4 already
    /// treats as legitimate for a deployment with no external ids at all — memmap2 refuses a
    /// zero-length mapping, and there is nothing to scatter into regardless.
    Empty,
    Mapped {
        map: memmap2::MmapMut,
        span: u64,
    },
}

impl LocatorWriter {
    /// Create `path` sized for `span` entities (`span * 4` bytes, no header), every slot filled
    /// with [`ROW_ABSENT`] up front — **not optional**, see the module doc.
    pub(crate) fn create(path: &Path, span: u64) -> io::Result<Self> {
        if span == 0 {
            File::create(path)?;
            return Ok(LocatorWriter::Empty);
        }
        let bytes = span.checked_mul(4).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("locator: span {span} overflows a byte length"),
            )
        })?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(bytes)?;

        // SAFETY: this process created and sized the file the statement before and holds the only
        // handle to it; nothing else maps or truncates it for the writer's lifetime — identical
        // justification to `PermutationWriter::create`.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }?;
        map.fill(0xFF);
        Ok(LocatorWriter::Mapped { map, span })
    }

    /// Record that `slot` holds ordinal `ordinal`. `slot` is already entity-space-relative,
    /// offset by whatever `entity_lo` the caller is using — this type has no opinion about what a
    /// slot means, only about where the bytes live.
    pub(crate) fn set(&mut self, slot: u64, ordinal: u32) -> io::Result<()> {
        match self {
            LocatorWriter::Empty => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("locator: slot {slot} written but the locator span is zero"),
            )),
            LocatorWriter::Mapped { map, span } => {
                if slot >= *span {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("locator: slot {slot} is out of bound (span = {span})"),
                    ));
                }
                let at = (slot as usize) * 4;
                map[at..at + 4].copy_from_slice(&ordinal.to_le_bytes());
                Ok(())
            }
        }
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        match self {
            LocatorWriter::Empty => Ok(()),
            LocatorWriter::Mapped { map, .. } => map.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::ROW_ABSENT;

    /// **The file is pre-sized for the whole span at `create`, before any slot is set.** This is
    /// the property that makes the writer page cache rather than a `Vec`: an implementation that
    /// accumulated ordinals and serialised them once at the end would leave the output file absent
    /// or empty until `finish`, which is exactly the shape this type replaces.
    ///
    /// **Mutation:** replace the mapped implementation with a buffered `Vec<u32>` flushed at
    /// `finish` (i.e. reintroduce what `coalesce`'s ordinary path still legitimately does, but for
    /// this writer), and this fails — the file would not yet be `span * 4` bytes here.
    #[test]
    fn the_file_is_sized_for_the_whole_span_before_any_slot_is_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ext-locator.u32");
        let writer = LocatorWriter::create(&path, 1000).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4000);
        writer.finish().unwrap();
    }

    /// An untouched slot reads as [`ROW_ABSENT`], never zero — zero is ordinal 0, a real position
    /// in a real run, so an unfilled slot would silently serve one key's row under every entity
    /// that never carried an external id.
    ///
    /// **Mutation:** fill with `0x00` instead of `0xFF` at create (or skip the fill), and entity 2
    /// here reads as bound to the run's first row instead of absent.
    #[test]
    fn an_unset_slot_is_the_absent_sentinel_not_zero() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ext-locator.u32");
        let mut writer = LocatorWriter::create(&path, 5).unwrap();
        writer.set(2, 7).unwrap();
        writer.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let slots: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        assert_eq!(
            slots,
            vec![ROW_ABSENT, ROW_ABSENT, 7, ROW_ABSENT, ROW_ABSENT]
        );
    }

    /// A slot at or past the span is refused rather than panicking or silently truncating — the
    /// same fail-closed shape `PermutationWriter::set`'s bound check uses.
    #[test]
    fn a_slot_outside_the_span_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ext-locator.u32");
        let mut writer = LocatorWriter::create(&path, 3).unwrap();
        assert!(writer.set(3, 0).is_err());
    }

    /// A zero-span locator is the legitimate empty case (contracts §2.4: a deployment whose
    /// callers supply no external ids writes no locator at all), not a crash on memmap2's
    /// zero-length refusal.
    #[test]
    fn a_zero_span_locator_is_an_empty_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ext-locator.u32");
        let writer = LocatorWriter::create(&path, 0).unwrap();
        writer.finish().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }
}
