//! **The tenancy index: for each entity id freed as an item's number, how many items have held it
//! before the item that holds it now.**
//!
//! An item's `mosaica_id` carries its number's tenancy ([`mosaica_types::ItemHigh`]), and the
//! compaction that frees a number raises its tenancy by one, so the number's next holder has a
//! different `mosaica_id` from every earlier one. The index is twelve Roaring bitmaps, the k-th
//! holding the numbers whose tenancy has bit k set. A number in none is at tenancy 0, so a shard
//! where no number has been freed stores nothing.
//!
//! Each bit is a file of its own, so a compaction that raises some numbers writes the bits that
//! changed and hard-links the rest. A bit holding no number has no file.
//!
//! # The format
//!
//! ```text
//! header  := magic "MSTN" | u16 version | u16 bit | u64 cardinality | u32 minimum | u32 maximum
//!            | u64 payload length
//! payload := at 32 bytes, the bit's numbers as one CRoaring frozen bitmap
//! ```
//!
//! # What the open checks
//!
//! The header against the file and against the bit the side-manifest names the file for, and the
//! mapped bitmap's cardinality, minimum and maximum against the header's. A frozen view over bytes
//! that are not a frozen bitmap is undefined, so that the payload is what the writer produced rests
//! on the bundle's digest sweep, as it does for [`crate::term_images`].

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use croaring::{Bitmap, BitmapView, Frozen};
use memmap2::Mmap;
use mosaica_types::{EntityId, Tenancy};

use crate::error::{Result, StoreError};

/// How many bits a tenancy has, and so how many bitmaps the index holds.
pub const TENANCY_BITS: usize = Tenancy::BITS as usize;

/// The directory a partition's index files sit in, beside its side-manifests.
pub const TENANCY_DIR: &str = "tenancy";

const MAGIC: &[u8; 4] = b"MSTN";
const VERSION: u16 = 1;
/// CRoaring's frozen reader needs its bytes 32-byte aligned, and a mapping is page-aligned.
const HEADER_LEN: usize = 32;

/// The file bit `bit` is written to.
pub fn file_name(bit: u32) -> String {
    format!("bit-{bit:02}.tenancy")
}

fn malformed(path: &Path, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("tenancy index file {}: {detail}", path.display()),
    }
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// Write each of `bits` that holds a number to its own file in `dir`, fsynced, and return the bits
/// written with their paths. The caller digests and names the files, and syncs `dir`.
pub fn write(dir: &Path, bits: &[Bitmap; TENANCY_BITS]) -> Result<Vec<(u32, PathBuf)>> {
    let mut written = Vec::new();
    for (bit, numbers) in bits.iter().enumerate() {
        if let Some(path) = write_bit(dir, bit as u32, numbers)? {
            written.push((bit as u32, path));
        }
    }
    Ok(written)
}

/// Write bit `bit`'s file in `dir`, fsynced, or nothing where `numbers` is empty. A file already at
/// the path is refused and left alone: an index file is written once, by the publication that names
/// it.
pub fn write_bit(dir: &Path, bit: u32, numbers: &Bitmap) -> Result<Option<PathBuf>> {
    assert!(
        (bit as usize) < TENANCY_BITS,
        "a tenancy has {TENANCY_BITS} bits"
    );
    let (Some(minimum), Some(maximum)) = (numbers.minimum(), numbers.maximum()) else {
        return Ok(None);
    };
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let path = dir.join(file_name(bit));
    let mut payload = Vec::new();
    let frozen = numbers.serialize_into_vec::<Frozen>(&mut payload);
    let mut bytes = Vec::with_capacity(HEADER_LEN + frozen.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(bit as u16).to_le_bytes());
    bytes.extend_from_slice(&numbers.cardinality().to_le_bytes());
    bytes.extend_from_slice(&minimum.to_le_bytes());
    bytes.extend_from_slice(&maximum.to_le_bytes());
    bytes.extend_from_slice(&(frozen.len() as u64).to_le_bytes());
    bytes.extend_from_slice(frozen);
    let mut file = File::create_new(&path).map_err(io(&path))?;
    file.write_all(&bytes).map_err(io(&path))?;
    file.sync_all().map_err(io(&path))?;
    Ok(Some(path))
}

/// One bit's numbers: a frozen view over the mapped file that holds them.
struct MappedBit {
    /// Declared before `_map`, so it is dropped before the bytes it reads.
    view: BitmapView<'static>,
    _map: Mmap,
    minimum: u32,
    maximum: u32,
}

impl MappedBit {
    fn open(bit: u32, path: &Path) -> Result<MappedBit> {
        let file = File::open(path).map_err(io(path))?;
        // SAFETY: opened read-only and never written through. An index file is written once under
        // a name its publication gives it and is never truncated while named.
        let map = unsafe { Mmap::map(&file) }.map_err(io(path))?;
        if map.len() < HEADER_LEN {
            return Err(malformed(
                path,
                format!("{} bytes is shorter than the header", map.len()),
            ));
        }
        if &map[0..4] != MAGIC {
            return Err(malformed(path, "the magic is not MSTN"));
        }
        let version = u16::from_le_bytes([map[4], map[5]]);
        if version != VERSION {
            return Err(malformed(
                path,
                format!("version {version}, expected {VERSION}"),
            ));
        }
        let named = u16::from_le_bytes([map[6], map[7]]);
        if u32::from(named) != bit {
            return Err(malformed(
                path,
                format!("the file holds bit {named} and the side-manifest names it for bit {bit}"),
            ));
        }
        let cardinality = le_u64(&map, 8);
        let minimum = le_u32(&map, 16);
        let maximum = le_u32(&map, 20);
        let payload_len = le_u64(&map, 24);
        if payload_len.checked_add(HEADER_LEN as u64) != Some(map.len() as u64) {
            return Err(malformed(
                path,
                format!(
                    "the header says {payload_len} bytes of payload and the file holds {}",
                    map.len() - HEADER_LEN
                ),
            ));
        }
        // SAFETY: the mapping is page-aligned and the payload starts 32 bytes in, so the pointer
        // is 32-byte aligned, and its length is the one the writer recorded. That the bytes are a
        // frozen bitmap rests on the digest sweep (module doc). The view's lifetime is the
        // mapping's: both are moved into the value returned, which drops the view first.
        let bytes: &'static [u8] =
            unsafe { std::mem::transmute::<&[u8], &'static [u8]>(&map[HEADER_LEN..]) };
        let view = unsafe { BitmapView::deserialize::<Frozen>(bytes) };
        let bounds = (view.minimum(), view.maximum());
        if view.cardinality() != cardinality
            || (cardinality > 0 && bounds != (Some(minimum), Some(maximum)))
        {
            return Err(malformed(
                path,
                "the bitmap's cardinality, minimum or maximum is not the header's",
            ));
        }
        Ok(MappedBit {
            view,
            _map: map,
            minimum,
            maximum,
        })
    }
}

/// The tenancy index of one shard, mapped. Empty where no number has been freed.
#[derive(Default)]
pub struct TenancyIndex {
    bits: [Option<MappedBit>; TENANCY_BITS],
    /// The largest number any bit holds, or `None` where the index is empty.
    maximum: Option<u32>,
}

impl std::fmt::Debug for TenancyIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held: Vec<(usize, u64)> = self
            .bits
            .iter()
            .enumerate()
            .filter_map(|(bit, mapped)| Some((bit, mapped.as_ref()?.view.cardinality())))
            .collect();
        f.debug_struct("TenancyIndex")
            .field("numbers_by_bit", &held)
            .finish()
    }
}

impl TenancyIndex {
    /// Map each `(bit, path)` and check it (module doc). A bit no file is given for, and a file
    /// whose bitmap is empty, holds no number.
    pub fn open(files: &[(u32, PathBuf)]) -> Result<TenancyIndex> {
        let mut index = TenancyIndex::default();
        for (bit, path) in files {
            let slot = index
                .bits
                .get_mut(*bit as usize)
                .ok_or_else(|| malformed(path, format!("bit {bit} is past a tenancy's bits")))?;
            if slot.is_some() {
                return Err(malformed(path, format!("bit {bit} is named twice")));
            }
            let mapped = MappedBit::open(*bit, path)?;
            if mapped.view.is_empty() {
                continue;
            }
            index.maximum = index.maximum.max(Some(mapped.maximum));
            *slot = Some(mapped);
        }
        Ok(index)
    }

    /// Whether every number is at tenancy 0.
    pub fn is_empty(&self) -> bool {
        self.maximum.is_none()
    }

    /// The numbers whose tenancy has bit `bit` set, or `None` where none has.
    pub fn bit(&self, bit: usize) -> Option<&Bitmap> {
        self.bits.get(bit)?.as_ref().map(|mapped| &*mapped.view)
    }

    /// `number`'s tenancy. Every bit the index holds is probed, whatever the number.
    pub fn of(&self, number: EntityId) -> Tenancy {
        let Ok(number) = u32::try_from(number.raw()) else {
            return Tenancy::ZERO;
        };
        let mut raw = 0u16;
        for (bit, mapped) in self.bits.iter().enumerate() {
            if mapped.as_ref().is_some_and(|m| m.view.contains(number)) {
                raw |= 1 << bit;
            }
        }
        Tenancy::new(raw).expect("twelve bits hold at most the highest tenancy")
    }

    /// The tenancy of each of `numbers`, in their order. A number above every number the index
    /// holds is at tenancy 0 without a probe, and an empty index probes nothing.
    pub fn of_each<I>(&self, numbers: I) -> Vec<Tenancy>
    where
        I: IntoIterator<Item = EntityId>,
        I::IntoIter: ExactSizeIterator,
    {
        let numbers = numbers.into_iter();
        let Some(maximum) = self.maximum else {
            return vec![Tenancy::ZERO; numbers.len()];
        };
        numbers
            .map(|number| match u32::try_from(number.raw()) {
                Ok(number) if number <= maximum => {
                    let mut raw = 0u16;
                    for (bit, mapped) in self.bits.iter().enumerate() {
                        let Some(mapped) = mapped else { continue };
                        if (mapped.minimum..=mapped.maximum).contains(&number)
                            && mapped.view.contains(number)
                        {
                            raw |= 1 << bit;
                        }
                    }
                    Tenancy::new(raw).expect("twelve bits hold at most the highest tenancy")
                }
                _ => Tenancy::ZERO,
            })
            .collect()
    }

    /// `numbers` split by the tenancy each is at, with no entry for a tenancy holding none of
    /// them. Bitmap arithmetic, one pass over the bits the index holds.
    pub fn split(&self, numbers: &Bitmap) -> BTreeMap<Tenancy, Bitmap> {
        let mut groups: Vec<(u16, Bitmap)> = vec![(0, numbers.clone())];
        groups.retain(|(_, ids)| !ids.is_empty());
        for (bit, mapped) in self.bits.iter().enumerate() {
            let Some(mapped) = mapped else { continue };
            groups = groups
                .into_iter()
                .flat_map(|(raw, ids)| {
                    let set = ids.and(&mapped.view);
                    let unset = ids.andnot(&set);
                    [(raw | 1 << bit, set), (raw, unset)]
                })
                .filter(|(_, ids)| !ids.is_empty())
                .collect();
        }
        groups
            .into_iter()
            .map(|(raw, ids)| {
                let tenancy =
                    Tenancy::new(raw).expect("twelve bits hold at most the highest tenancy");
                (tenancy, ids)
            })
            .collect()
    }

    /// The bits of the index with each of `numbers` one tenancy higher: `Some` for a bit the raise
    /// changes, possibly to empty, and `None` for one it leaves as it is, whose file the next index
    /// can share. Each bit is the previous one with the numbers carried into it flipped, and the
    /// numbers it held among those carry on to the next bit.
    ///
    /// Panics where one of `numbers` is at [`Tenancy::MAX`]: such a number is retired, never freed,
    /// and the caller leaves it out.
    pub fn raised(&self, numbers: &Bitmap) -> [Option<Bitmap>; TENANCY_BITS] {
        let mut next: [Option<Bitmap>; TENANCY_BITS] = Default::default();
        let mut carry = numbers.clone();
        for (bit, out) in next.iter_mut().enumerate() {
            if carry.is_empty() {
                break;
            }
            let (flipped, carried) = match self.bit(bit) {
                Some(held) => (held.xor(&carry), held.and(&carry)),
                None => (carry, Bitmap::new()),
            };
            *out = Some(flipped);
            carry = carried;
        }
        assert!(
            carry.is_empty(),
            "a number at the highest tenancy is retired and cannot be raised"
        );
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: u16) -> Tenancy {
        Tenancy::new(raw).unwrap()
    }

    /// The bits that put each `(number, tenancy)` at its tenancy.
    fn bits_of(tenancies: &[(u32, u16)]) -> [Bitmap; TENANCY_BITS] {
        let mut bits: [Bitmap; TENANCY_BITS] = Default::default();
        for &(number, tenancy) in tenancies {
            for (bit, numbers) in bits.iter_mut().enumerate() {
                if tenancy & (1 << bit) != 0 {
                    numbers.add(number);
                }
            }
        }
        bits
    }

    fn written(dir: &Path, bits: &[Bitmap; TENANCY_BITS]) -> TenancyIndex {
        TenancyIndex::open(&write(dir, bits).unwrap()).unwrap()
    }

    #[test]
    fn what_is_written_reads_back_and_a_number_in_no_bit_is_at_tenancy_zero() {
        let dir = tempfile::tempdir().unwrap();
        let held = [(3, 1), (7, 2), (9, 3), (70_000, 4095), (1 << 31, 2048)];
        let files = write(dir.path(), &bits_of(&held)).unwrap();
        assert_eq!(files.len(), TENANCY_BITS, "every bit holds a number here");
        let index = TenancyIndex::open(&files).unwrap();
        assert!(!index.is_empty());
        for (number, tenancy) in held {
            assert_eq!(index.of(EntityId::new(u64::from(number))), at(tenancy));
        }
        for number in [0, 4, 8, 69_999, u64::from(u32::MAX), 1 << 40] {
            assert_eq!(index.of(EntityId::new(number)), Tenancy::ZERO);
        }
    }

    #[test]
    fn a_bit_holding_no_number_stores_nothing_and_an_index_of_none_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let files = write(dir.path(), &bits_of(&[(5, 4)])).unwrap();
        assert_eq!(
            files,
            vec![(2, dir.path().join(file_name(2)))],
            "only bit 2 is written"
        );
        assert!(TenancyIndex::open(&[]).unwrap().is_empty());
        let none = tempfile::tempdir().unwrap();
        assert!(write(none.path(), &Default::default()).unwrap().is_empty());
        assert_eq!(TenancyIndex::default().of(EntityId::new(5)), Tenancy::ZERO);
    }

    #[test]
    fn a_batch_answers_what_each_single_lookup_does() {
        let dir = tempfile::tempdir().unwrap();
        let held: Vec<(u32, u16)> = (0..2_000u32)
            .map(|n| (n * 37 + 11, ((n * 13) % 4096) as u16))
            .collect();
        let index = written(dir.path(), &bits_of(&held));
        // Unsorted, repeated, below and above every number held, and past u32.
        let asked: Vec<EntityId> = (0..80_000u64)
            .rev()
            .step_by(7)
            .chain([11, 11, 0, u64::from(u32::MAX), 1 << 33])
            .map(EntityId::new)
            .collect();
        let batch = index.of_each(asked.iter().copied());
        let single: Vec<Tenancy> = asked.iter().map(|n| index.of(*n)).collect();
        assert_eq!(batch, single);
        assert!(batch.iter().any(|t| *t != Tenancy::ZERO));
        assert_eq!(
            TenancyIndex::default().of_each(asked.iter().copied()),
            vec![Tenancy::ZERO; asked.len()]
        );
    }

    #[test]
    fn a_split_puts_each_number_at_the_tenancy_a_lookup_answers() {
        let dir = tempfile::tempdir().unwrap();
        let held: Vec<(u32, u16)> = (0..500u32).map(|n| (n * 3, (n % 7) as u16 * 600)).collect();
        let index = written(dir.path(), &bits_of(&held));
        let asked = Bitmap::from_range(0..1_600);
        let split = index.split(&asked);
        assert_eq!(
            split.values().map(Bitmap::cardinality).sum::<u64>(),
            asked.cardinality(),
            "every number is in exactly one tenancy"
        );
        for (tenancy, numbers) in &split {
            assert!(!numbers.is_empty(), "an empty tenancy has no entry");
            for number in numbers.iter() {
                assert_eq!(index.of(EntityId::new(u64::from(number))), *tenancy);
            }
        }
        assert_eq!(
            TenancyIndex::default().split(&asked),
            BTreeMap::from([(Tenancy::ZERO, asked.clone())])
        );
        assert!(index.split(&Bitmap::new()).is_empty());
        assert!(TenancyIndex::default().split(&Bitmap::new()).is_empty());
    }

    #[test]
    fn raising_carries_across_bits_and_shares_the_bits_it_leaves() {
        let dir = tempfile::tempdir().unwrap();
        // 1 → 2 and 3 → 4 carry out of bit 0; 4094 → 4095 sets bit 0 alone; 10 rises from 0.
        let held = [(100, 1), (200, 3), (300, 4094), (400, 6)];
        let index = written(&dir.path().join("before"), &bits_of(&held));
        let raised = index.raised(&Bitmap::of(&[100, 200, 300, 10]));
        assert!(
            raised[3..].iter().all(Option::is_none),
            "no carry reaches bit 3, so bits 3 and up are shared"
        );
        let next: [Bitmap; TENANCY_BITS] = std::array::from_fn(|bit| match &raised[bit] {
            Some(changed) => changed.clone(),
            None => index.bit(bit).cloned().unwrap_or_default(),
        });
        let next = written(&dir.path().join("after"), &next);
        for (number, tenancy) in [(100, 2), (200, 4), (300, 4095), (400, 6), (10, 1), (11, 0)] {
            assert_eq!(
                next.of(EntityId::new(number)),
                at(tenancy),
                "number {number}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "retired")]
    fn a_number_at_the_highest_tenancy_is_not_raised() {
        let dir = tempfile::tempdir().unwrap();
        let index = written(dir.path(), &bits_of(&[(8, 4095)]));
        let _ = index.raised(&Bitmap::of(&[8]));
    }

    #[test]
    fn a_file_for_another_bit_or_a_cut_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let files = write(dir.path(), &bits_of(&[(5, 1), (6, 2)])).unwrap();
        let swapped = vec![(1, files[0].1.clone())];
        assert!(
            TenancyIndex::open(&swapped).is_err(),
            "bit 0's file named for bit 1"
        );
        let twice = vec![files[0].clone(), files[0].clone()];
        assert!(TenancyIndex::open(&twice).is_err(), "one bit named twice");
        assert!(TenancyIndex::open(&[(12, files[0].1.clone())]).is_err());
        let bytes = std::fs::read(&files[0].1).unwrap();
        for cut in [0, 10, HEADER_LEN, bytes.len() - 1] {
            std::fs::write(&files[0].1, &bytes[..cut]).unwrap();
            assert!(TenancyIndex::open(&files[..1]).is_err(), "cut at {cut}");
        }
        std::fs::write(&files[0].1, &bytes).unwrap();
        assert!(
            write_bit(dir.path(), 0, &Bitmap::of(&[9])).is_err(),
            "a written file is not replaced"
        );
    }
}
