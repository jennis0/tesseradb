//! **Term images: one authorisation term's base posting, projected into a view's row space and
//! stored so a session reads it instead of walking it.**
//!
//! A session's row projection is the image of its mask fragment under the view's permutation, and
//! building it walks one permutation slot per held entity. A **term image** is that walk done once,
//! at build and at fold, for a single term: the term's base posting projected through
//! [`RowSpace::project_base`], run-optimised, and written in CRoaring frozen form. A session that
//! holds a term with an image unions the image; a session that holds a term without one walks its
//! entities as before. The two routes produce the same rows.
//!
//! One file holds every image of one (partition, view): a fixed header, a table with one entry per
//! term id, and the kept images at 32-byte-aligned offsets in ascending term order. The table is
//! dense so that a term without an image still reports its size, which is what the route chooser
//! below needs to price the residual walk.
//!
//! # The keep rule
//!
//! An image is kept only where its cardinality is strictly greater than [`KEEP_ROWS_PER_CONTAINER`]
//! rows per Roaring container. Below that the image costs more to read than the walk costs to run
//! (evidence memo `docs/evidence/memos/2026-09-14-term-images.md` §2). The constant is written into
//! the header and checked at open, so a file derived under a different rule is refused rather than
//! read against the wrong cost model.
//!
//! A posting of [`KEEP_ROWS_PER_CONTAINER`] entities or fewer is not projected at all. Projection
//! maps entities to rows one for one and drops the entities holding no row, so an image's
//! cardinality is at most its posting's; a non-empty image occupies at least one container. Such a
//! posting therefore cannot exceed 30 rows per container and cannot be kept, whatever the
//! permutation does. The skip is exact rather than an estimate. The table still records the
//! posting's cardinality, which is an upper bound on the image's rows and so a conservative
//! residual for the chooser.
//!
//! # Exactness
//!
//! Write `P_t` for term `t`'s base posting, `T` for the terms a session satisfies, `F` for its
//! fragment and `B` for [`RowSpace::project_base`]. The fragment is a union over the satisfied
//! terms, so `P_t ⊆ F` for every `t ∈ T`. `B` maps each entity to at most one row and reads no
//! other entity, so it distributes over union and `B(F ∩ P_t) = B(P_t)`. For any kept subset
//! `K ⊆ T` and any `S` with `F \ ∪_{t∈K} P_t ⊆ S ⊆ F`,
//!
//! ```text
//! B(F) = fast_or(image of t : t ∈ K) ∪ B(S).
//! ```
//!
//! The lower bound on `S` is what makes the union complete. The upper bound is what keeps it
//! inside the grant (I2): a caller assembling `S` from delta postings must intersect it with `F`
//! before projecting, because a delta tier can carry entities the fragment was not built from.
//!
//! # Deny, suppression and deletion
//!
//! Images are pre-overlay, as a row projection is. A suppressed entity stays in the posting and in
//! the image, and the effective mask composes deny and buffer on every request, so a suppression
//! applies from the moment it is accepted (decision 0041). A deletion leaves a posting and its
//! image at the fold that removes its rows, and by no other route (`write-path.md` §5.4).
//!
//! # The frozen view
//!
//! [`TermImages::view`] hands a mapped byte range to CRoaring's frozen deserialiser, which is
//! unsafe: the bytes must be a frozen bitmap, aligned to 32 bytes, and exactly the serialised
//! length. [`TermImages::open`] checks the alignment, checks that every kept entry's range lies
//! inside the payload and overlaps no other, and checks that its length is at least what the
//! entry's own container counts require. It does not read the payload and so does not establish
//! that a range holds a well-formed frozen bitmap. That rests on the bundle's digest sweep, which
//! covers this file as it covers every other file the manifest lists, the same discharge
//! `tessera_authz`'s cached fragment makes against its own recorded digest.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use croaring::{Bitmap, BitmapView, Frozen};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use tessera_types::view::ViewIncarnation;
use tessera_types::TermId;

use crate::derived::{PostingSlice, PostingWalk};
use crate::permutation::{ProjectScratch, RowSpace};

/// The cut an image is kept above: **more than thirty rows per Roaring container** (owner ruling,
/// 2026-09-16, from the thresholds compared in the evidence memo §2).
///
/// Written into every file's header and checked at open, because the rule decides which terms a
/// session may read rather than walk, and a file written under another rule would be priced wrongly
/// by the chooser.
pub const KEEP_ROWS_PER_CONTAINER: u64 = 30;

/// The first eight bytes of a term-image file.
pub const MAGIC: [u8; 8] = *b"TSMIMG01";

/// The header layout this module writes and reads.
pub const HEADER_VERSION: u32 = 1;

/// The fixed header, ahead of the table.
pub const HEADER_BYTES: usize = 128;

/// One table entry, one term id.
pub const TABLE_ENTRY_BYTES: usize = 40;

/// The alignment every kept image starts at. CRoaring's frozen deserialiser requires 32 bytes, and
/// a memory map's base address is page-aligned, so an offset that is a multiple of 32 gives an
/// aligned pointer.
pub const PAYLOAD_ALIGN: usize = 32;

// Header field offsets.
const OFF_MAGIC: usize = 0;
const OFF_HEADER_VERSION: usize = 8;
const OFF_KEEP_ROWS: usize = 12;
const OFF_DICT_LEN: usize = 16;
const OFF_TABLE_OFFSET: usize = 24;
const OFF_PAYLOAD_OFFSET: usize = 32;
const OFF_FILE_LEN: usize = 40;
const OFF_STAMP_DIGEST: usize = 48;
const OFF_INCARNATION: usize = 80;
const OFF_BASE_ROWS: usize = 88;
const OFF_BOUND: usize = 96;

// Table entry field offsets.
const ENTRY_OFFSET: usize = 0;
const ENTRY_LEN: usize = 8;
const ENTRY_ROWS: usize = 12;
const ENTRY_CONTAINERS: usize = 20;
const ENTRY_ARRAYS: usize = 24;
const ENTRY_RUNS: usize = 28;
const ENTRY_BITSETS: usize = 32;

/// Which row space a file's images belong to.
///
/// The three strings are hashed into the header rather than stored, so the header is fixed width;
/// the three numbers are stored as they are. [`TermImages::open`] compares all four against what
/// the caller expects, and a file that fails any of them is refused: an image projected through a
/// different permutation names rows this view does not hold, which is a disclosure rather than a
/// stale answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermImageStamp {
    /// The prefix the base segment belongs to.
    pub prefix: String,
    /// The view, including the `group:key` form.
    pub view: String,
    /// The base segment whose permutation the images were projected through.
    pub base_seg_id: String,
    /// The view incarnation the base segment was written under.
    pub incarnation: ViewIncarnation,
    /// The base segment's row count.
    pub base_rows: u32,
    /// The permutation's entity-space width.
    pub bound: u64,
}

impl TermImageStamp {
    /// SHA-256 over `prefix ‖ 0x00 ‖ view ‖ 0x00 ‖ base_seg_id`.
    ///
    /// The separators make the concatenation unambiguous: no field may contain a NUL, so no two
    /// distinct triples produce the same input.
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.prefix.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.view.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.base_seg_id.as_bytes());
        hasher.finalize().into()
    }
}

/// One term's row in the table.
///
/// `rows` means three different things, told apart by the other fields. For a term that was
/// projected it is the image's cardinality, kept or not, and the container counts describe that
/// image. For a term whose posting was too small to pass the keep rule it is the posting's
/// cardinality, an upper bound on the image's rows, and the container counts are zero. For a term
/// the posting walk carries no record of, every field is zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermImageEntry {
    /// Absolute file offset of the frozen image, or zero where none was kept.
    pub offset: u64,
    /// The frozen image's length in bytes.
    pub len: u32,
    /// The image's cardinality, or the posting's where the posting was not projected.
    pub rows: u64,
    /// Containers in the image.
    pub containers: u32,
    /// Array containers in the image.
    pub arrays: u32,
    /// Run containers in the image.
    pub runs: u32,
    /// Bitset containers in the image.
    pub bitsets: u32,
}

impl TermImageEntry {
    /// Does this term have an image in the payload?
    pub fn kept(&self) -> bool {
        self.offset != 0
    }

    fn encode(&self) -> [u8; TABLE_ENTRY_BYTES] {
        let mut bytes = [0u8; TABLE_ENTRY_BYTES];
        bytes[ENTRY_OFFSET..ENTRY_OFFSET + 8].copy_from_slice(&self.offset.to_le_bytes());
        bytes[ENTRY_LEN..ENTRY_LEN + 4].copy_from_slice(&self.len.to_le_bytes());
        bytes[ENTRY_ROWS..ENTRY_ROWS + 8].copy_from_slice(&self.rows.to_le_bytes());
        bytes[ENTRY_CONTAINERS..ENTRY_CONTAINERS + 4]
            .copy_from_slice(&self.containers.to_le_bytes());
        bytes[ENTRY_ARRAYS..ENTRY_ARRAYS + 4].copy_from_slice(&self.arrays.to_le_bytes());
        bytes[ENTRY_RUNS..ENTRY_RUNS + 4].copy_from_slice(&self.runs.to_le_bytes());
        bytes[ENTRY_BITSETS..ENTRY_BITSETS + 4].copy_from_slice(&self.bitsets.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Self {
        TermImageEntry {
            offset: le_u64(bytes, ENTRY_OFFSET),
            len: le_u32(bytes, ENTRY_LEN),
            rows: le_u64(bytes, ENTRY_ROWS),
            containers: le_u32(bytes, ENTRY_CONTAINERS),
            arrays: le_u32(bytes, ENTRY_ARRAYS),
            runs: le_u32(bytes, ENTRY_RUNS),
            bitsets: le_u32(bytes, ENTRY_BITSETS),
        }
    }

    /// The fewest bytes a frozen bitmap with these container counts can occupy: a four-byte header,
    /// five bytes of key, count and typecode per container, and each container's own payload floor
    /// of 8,192 bytes for a bitset, four for a run container holding one run and two for an array
    /// container holding one value.
    fn minimum_frozen_bytes(&self) -> u64 {
        4 + 5 * u64::from(self.containers)
            + 8192 * u64::from(self.bitsets)
            + 4 * u64::from(self.runs)
            + 2 * u64::from(self.arrays)
    }
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn align_up(at: u64) -> u64 {
    let align = PAYLOAD_ALIGN as u64;
    at.div_ceil(align) * align
}

// -------------------------------------------------------------------------------------------
// Derivation
// -------------------------------------------------------------------------------------------

/// How wide [`derive_term_images`] runs.
#[derive(Clone, Copy, Debug)]
pub struct DeriveOptions {
    /// Worker threads for the projection. One runs the whole derivation on the calling thread.
    pub threads: usize,
}

impl Default for DeriveOptions {
    fn default() -> Self {
        DeriveOptions { threads: 1 }
    }
}

/// What one derivation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermImageSummary {
    /// Table entries written, which is the dictionary length.
    pub terms: u32,
    /// Postings projected.
    pub derived: u32,
    /// Images written to the payload.
    pub kept: u32,
    /// Postings too small to pass the keep rule, so not projected.
    pub skipped_small: u32,
    /// Bytes of payload, including the padding between images.
    pub payload_bytes: u64,
    /// Bytes of table.
    pub table_bytes: u64,
    /// The largest single image.
    pub largest_image_bytes: u64,
    /// Wall time for the whole derivation, including the writes and the two fsyncs.
    pub wall: Duration,
}

/// One term's posting as the derivation holds it while the window is projected.
enum Posting {
    /// The walk carried no record of the term.
    Absent,
    /// At most [`KEEP_ROWS_PER_CONTAINER`] entities, so the image cannot be kept and is not built.
    Small(u64),
    /// Projected.
    Large(Bitmap),
}

/// One term's result, before it is placed in the file.
#[derive(Default)]
struct Outcome {
    rows: u64,
    containers: u32,
    arrays: u32,
    runs: u32,
    bitsets: u32,
    derived: bool,
    skipped_small: bool,
    /// The frozen bytes and where they start inside the buffer the serialiser aligned them in.
    image: Option<(Vec<u8>, usize)>,
}

/// Derive every term's image for one view and write them to `out`.
///
/// `posting` is called once per term id below `dict_len`, in ascending order, on the calling
/// thread: the walk reads a file and is not required to be callable from a worker. Each window of
/// `threads × 4` consecutive terms is read, then projected across the workers, then appended in
/// term order before the next window is read, so the bytes written do not depend on `threads` and
/// the memory held is one window's images plus one scratch per worker.
///
/// The header is written after the payload and the table, and the file is synced between the two
/// writes, so a file interrupted part way carries a zero header and is refused at open rather than
/// read short.
///
/// The caller names the file and syncs the directory it was placed in.
pub fn derive_term_images(
    space: &RowSpace,
    dict_len: u32,
    posting: PostingWalk<'_>,
    stamp: &TermImageStamp,
    out: &Path,
    options: DeriveOptions,
) -> io::Result<TermImageSummary> {
    let started = Instant::now();

    // A stamp describing another row space would be written into a file whose images belong to
    // this one, and open would then accept images that name rows the view does not hold.
    if stamp.base_rows != space.base_rows() || stamp.bound != space.base().bound() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "term images: the stamp names {} base rows over a bound of {} but the row space \
                 has {} rows over a bound of {}",
                stamp.base_rows,
                stamp.bound,
                space.base_rows(),
                space.base().bound()
            ),
        ));
    }

    let threads = options.threads.max(1);
    let window = threads * 4;
    let table_bytes = u64::from(dict_len) * TABLE_ENTRY_BYTES as u64;
    let table_offset = HEADER_BYTES as u64;
    let payload_offset = align_up(table_offset + table_bytes);

    let pool = if threads > 1 {
        Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .map_err(|e| io::Error::other(e.to_string()))?,
        )
    } else {
        None
    };
    let scratches: Mutex<Vec<ProjectScratch>> = Mutex::new(Vec::new());

    let mut file = File::create(out)?;
    file.set_len(payload_offset)?;
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut writer = io::BufWriter::new(file);

    let mut table = vec![TermImageEntry::default(); dict_len as usize];
    let mut summary = TermImageSummary {
        terms: dict_len,
        table_bytes,
        ..TermImageSummary::default()
    };
    let zeros = [0u8; PAYLOAD_ALIGN];
    let mut at = payload_offset;
    let mut window_postings: Vec<Posting> = Vec::with_capacity(window);

    let mut first = 0u32;
    while first < dict_len {
        let last = ((u64::from(first) + window as u64).min(u64::from(dict_len))) as u32;
        window_postings.clear();
        for term in first..last {
            window_postings.push(read_posting(posting, term)?);
        }

        let outcomes: Vec<Outcome> = match &pool {
            Some(pool) => pool.install(|| {
                window_postings
                    .par_iter()
                    .map(|p| derive_one(space, p, &scratches))
                    .collect()
            }),
            None => window_postings
                .iter()
                .map(|p| derive_one(space, p, &scratches))
                .collect(),
        };

        for (step, outcome) in outcomes.into_iter().enumerate() {
            let term = first + step as u32;
            let mut entry = TermImageEntry {
                offset: 0,
                len: 0,
                rows: outcome.rows,
                containers: outcome.containers,
                arrays: outcome.arrays,
                runs: outcome.runs,
                bitsets: outcome.bitsets,
            };
            if outcome.derived {
                summary.derived += 1;
            }
            if outcome.skipped_small {
                summary.skipped_small += 1;
            }
            if let Some((buffer, start)) = outcome.image {
                let bytes = &buffer[start..];
                let aligned = align_up(at);
                if aligned > at {
                    writer.write_all(&zeros[..(aligned - at) as usize])?;
                    at = aligned;
                }
                let len = u32::try_from(bytes.len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("term images: term {term}'s image is {} bytes", bytes.len()),
                    )
                })?;
                writer.write_all(bytes)?;
                entry.offset = at;
                entry.len = len;
                at += u64::from(len);
                summary.kept += 1;
                summary.largest_image_bytes = summary.largest_image_bytes.max(u64::from(len));
            }
            table[term as usize] = entry;
        }
        first = last;
    }

    summary.payload_bytes = at - payload_offset;

    let mut file = writer.into_inner().map_err(|e| e.into_error())?;
    file.seek(SeekFrom::Start(table_offset))?;
    {
        let mut table_writer = io::BufWriter::new(&mut file);
        for entry in &table {
            table_writer.write_all(&entry.encode())?;
        }
        table_writer.flush()?;
    }
    file.sync_all()?;

    let mut header = [0u8; HEADER_BYTES];
    header[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
    header[OFF_HEADER_VERSION..OFF_HEADER_VERSION + 4]
        .copy_from_slice(&HEADER_VERSION.to_le_bytes());
    header[OFF_KEEP_ROWS..OFF_KEEP_ROWS + 4]
        .copy_from_slice(&(KEEP_ROWS_PER_CONTAINER as u32).to_le_bytes());
    header[OFF_DICT_LEN..OFF_DICT_LEN + 4].copy_from_slice(&dict_len.to_le_bytes());
    header[OFF_TABLE_OFFSET..OFF_TABLE_OFFSET + 8].copy_from_slice(&table_offset.to_le_bytes());
    header[OFF_PAYLOAD_OFFSET..OFF_PAYLOAD_OFFSET + 8]
        .copy_from_slice(&payload_offset.to_le_bytes());
    header[OFF_FILE_LEN..OFF_FILE_LEN + 8].copy_from_slice(&at.to_le_bytes());
    header[OFF_STAMP_DIGEST..OFF_STAMP_DIGEST + 32].copy_from_slice(&stamp.digest());
    header[OFF_INCARNATION..OFF_INCARNATION + 8].copy_from_slice(&stamp.incarnation.to_le_bytes());
    header[OFF_BASE_ROWS..OFF_BASE_ROWS + 4].copy_from_slice(&stamp.base_rows.to_le_bytes());
    header[OFF_BOUND..OFF_BOUND + 8].copy_from_slice(&stamp.bound.to_le_bytes());

    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.sync_all()?;

    summary.wall = started.elapsed();
    Ok(summary)
}

/// Read one term's posting into a bitmap the workers can project without holding the walk.
fn read_posting(posting: PostingWalk<'_>, term: u32) -> io::Result<Posting> {
    let mut entities: Option<Bitmap> = None;
    posting(term, &mut |slice| {
        let next = match slice {
            PostingSlice::Array(bytes) => {
                let values: Vec<u32> = bytes
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("four bytes")))
                    .collect();
                let mut bitmap = Bitmap::new();
                bitmap.add_many(&values);
                bitmap
            }
            PostingSlice::Roaring(view) => view.clone(),
        };
        match entities {
            None => entities = Some(next),
            Some(ref mut held) => held.or_inplace(&next),
        }
    })?;
    Ok(match entities {
        None => Posting::Absent,
        Some(bitmap) => {
            let cardinality = bitmap.cardinality();
            if cardinality <= KEEP_ROWS_PER_CONTAINER {
                Posting::Small(cardinality)
            } else {
                Posting::Large(bitmap)
            }
        }
    })
}

/// Project one posting and decide whether its image is kept.
fn derive_one(
    space: &RowSpace,
    posting: &Posting,
    scratches: &Mutex<Vec<ProjectScratch>>,
) -> Outcome {
    match posting {
        Posting::Absent => Outcome::default(),
        Posting::Small(rows) => Outcome {
            rows: *rows,
            skipped_small: true,
            ..Outcome::default()
        },
        Posting::Large(entities) => {
            let mut scratch = scratches
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop()
                .unwrap_or_default();
            let mut image = space.project_base_with(entities, &mut scratch);
            scratches
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(scratch);

            image.run_optimize();
            let stats = image.statistics();
            let keep = stats.cardinality > KEEP_ROWS_PER_CONTAINER * u64::from(stats.n_containers);
            let bytes = keep.then(|| {
                let mut buffer = Vec::new();
                let len = image.serialize_into_vec::<Frozen>(&mut buffer).len();
                let start = buffer.len() - len;
                (buffer, start)
            });
            Outcome {
                rows: stats.cardinality,
                containers: stats.n_containers,
                arrays: stats.n_array_containers,
                runs: stats.n_run_containers,
                bitsets: stats.n_bitset_containers,
                derived: true,
                skipped_small: false,
                image: bytes,
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// Reading
// -------------------------------------------------------------------------------------------

/// Which check [`TermImages::open`] failed.
///
/// A caller treats any of these as "this view has no images": the walk answers the same rows, so a
/// refusal costs time and changes no answer.
#[derive(Debug)]
pub enum TermImageRefusal {
    /// The file could not be opened or mapped.
    Io(io::Error),
    /// The first eight bytes are not [`MAGIC`].
    Magic,
    /// The header version is not [`HEADER_VERSION`].
    Version { found: u32 },
    /// The file is shorter than the header, or the header's offsets do not describe this file.
    HeaderBounds,
    /// The file was derived under a different keep rule.
    KeepRowsPerContainer { found: u32 },
    /// The file covers a different number of terms than the caller's dictionary.
    DictLen { found: u32, expected: u32 },
    /// The table does not fit between the header and the payload.
    TableBounds,
    /// A kept image does not start on a [`PAYLOAD_ALIGN`] boundary.
    EntryMisaligned { term: u32 },
    /// A kept image's bytes lie outside the payload.
    EntryOutOfPayload { term: u32 },
    /// A kept image starts before the previous one ends, or before it starts.
    EntryOverlaps { term: u32 },
    /// A kept image does not pass the keep rule.
    EntryNotDense { term: u32 },
    /// A kept image is shorter than its own container counts allow.
    EntryTooShort { term: u32 },
    /// An entry's container counts do not sum, or an entry with no image carries a length.
    EntryMalformed { term: u32 },
    /// The header's stamp digest is not the caller's.
    StampDigest,
    /// The header's view incarnation is not the caller's.
    Incarnation { found: u64, expected: u64 },
    /// The header's base row count is not the caller's.
    BaseRows { found: u32, expected: u32 },
    /// The header's permutation bound is not the caller's.
    Bound { found: u64, expected: u64 },
}

impl std::fmt::Display for TermImageRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TermImageRefusal::Io(e) => write!(f, "term images: {e}"),
            TermImageRefusal::Magic => write!(f, "term images: not a term-image file"),
            TermImageRefusal::Version { found } => {
                write!(
                    f,
                    "term images: header version {found}, expected {HEADER_VERSION}"
                )
            }
            TermImageRefusal::HeaderBounds => {
                write!(f, "term images: the header does not describe this file")
            }
            TermImageRefusal::KeepRowsPerContainer { found } => write!(
                f,
                "term images: keep rule of {found} rows per container, expected \
                 {KEEP_ROWS_PER_CONTAINER}"
            ),
            TermImageRefusal::DictLen { found, expected } => {
                write!(f, "term images: {found} terms, expected {expected}")
            }
            TermImageRefusal::TableBounds => {
                write!(f, "term images: the table does not fit before the payload")
            }
            TermImageRefusal::EntryMisaligned { term } => {
                write!(f, "term images: term {term}'s image is not 32-byte aligned")
            }
            TermImageRefusal::EntryOutOfPayload { term } => {
                write!(
                    f,
                    "term images: term {term}'s image lies outside the payload"
                )
            }
            TermImageRefusal::EntryOverlaps { term } => {
                write!(
                    f,
                    "term images: term {term}'s image overlaps the one before it"
                )
            }
            TermImageRefusal::EntryNotDense { term } => {
                write!(
                    f,
                    "term images: term {term}'s image does not pass the keep rule"
                )
            }
            TermImageRefusal::EntryTooShort { term } => write!(
                f,
                "term images: term {term}'s image is shorter than its container counts allow"
            ),
            TermImageRefusal::EntryMalformed { term } => {
                write!(f, "term images: term {term}'s table entry is inconsistent")
            }
            TermImageRefusal::StampDigest => {
                write!(
                    f,
                    "term images: the file belongs to another view or segment"
                )
            }
            TermImageRefusal::Incarnation { found, expected } => {
                write!(f, "term images: incarnation {found}, expected {expected}")
            }
            TermImageRefusal::BaseRows { found, expected } => {
                write!(f, "term images: {found} base rows, expected {expected}")
            }
            TermImageRefusal::Bound { found, expected } => {
                write!(f, "term images: bound {found}, expected {expected}")
            }
        }
    }
}

impl std::error::Error for TermImageRefusal {}

/// One view's term images, mapped.
pub struct TermImages {
    mmap: memmap2::Mmap,
    stamp: TermImageStamp,
    dict_len: u32,
    table_offset: usize,
    payload_offset: usize,
    file_len: usize,
}

impl std::fmt::Debug for TermImages {
    /// The shape, not the mapping: a derived implementation would print the whole payload.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermImages")
            .field("stamp", &self.stamp)
            .field("dict_len", &self.dict_len)
            .field("payload_offset", &self.payload_offset)
            .field("file_len", &self.file_len)
            .finish()
    }
}

impl TermImages {
    /// Map `path` and run every check before returning.
    ///
    /// No input reaches a panic, including a zero-length file and a file truncated at any byte:
    /// every field is read from a range this checks first, and every entry is checked against the
    /// payload and against the entry before it.
    pub fn open(
        path: &Path,
        expected: &TermImageStamp,
        expected_dict_len: u32,
    ) -> Result<TermImages, TermImageRefusal> {
        let file = File::open(path).map_err(TermImageRefusal::Io)?;
        // SAFETY: a term-image file is written once under a fresh name and placed; nothing in this
        // process opens a published one for writing, so the mapping's bytes do not change under
        // the reader. The same discharge the other mapped bundle files make.
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(TermImageRefusal::Io)?;

        if mmap.len() < HEADER_BYTES {
            return Err(TermImageRefusal::HeaderBounds);
        }
        let header = &mmap[..HEADER_BYTES];
        if header[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(TermImageRefusal::Magic);
        }
        let version = le_u32(header, OFF_HEADER_VERSION);
        if version != HEADER_VERSION {
            return Err(TermImageRefusal::Version { found: version });
        }
        let keep = le_u32(header, OFF_KEEP_ROWS);
        if u64::from(keep) != KEEP_ROWS_PER_CONTAINER {
            return Err(TermImageRefusal::KeepRowsPerContainer { found: keep });
        }
        let dict_len = le_u32(header, OFF_DICT_LEN);
        if dict_len != expected_dict_len {
            return Err(TermImageRefusal::DictLen {
                found: dict_len,
                expected: expected_dict_len,
            });
        }

        let table_offset = le_u64(header, OFF_TABLE_OFFSET);
        let payload_offset = le_u64(header, OFF_PAYLOAD_OFFSET);
        let file_len = le_u64(header, OFF_FILE_LEN);
        if table_offset != HEADER_BYTES as u64
            || file_len != mmap.len() as u64
            || payload_offset > file_len
        {
            return Err(TermImageRefusal::HeaderBounds);
        }
        let table_bytes = u64::from(dict_len) * TABLE_ENTRY_BYTES as u64;
        if payload_offset != align_up(table_offset + table_bytes) {
            return Err(TermImageRefusal::TableBounds);
        }

        if header[OFF_STAMP_DIGEST..OFF_STAMP_DIGEST + 32] != expected.digest() {
            return Err(TermImageRefusal::StampDigest);
        }
        let incarnation = le_u64(header, OFF_INCARNATION);
        if incarnation != expected.incarnation {
            return Err(TermImageRefusal::Incarnation {
                found: incarnation,
                expected: expected.incarnation,
            });
        }
        let base_rows = le_u32(header, OFF_BASE_ROWS);
        if base_rows != expected.base_rows {
            return Err(TermImageRefusal::BaseRows {
                found: base_rows,
                expected: expected.base_rows,
            });
        }
        let bound = le_u64(header, OFF_BOUND);
        if bound != expected.bound {
            return Err(TermImageRefusal::Bound {
                found: bound,
                expected: expected.bound,
            });
        }

        let table_offset = table_offset as usize;
        let payload_offset = payload_offset as usize;
        let file_len = file_len as usize;
        let mut previous_end = payload_offset as u64;
        for term in 0..dict_len {
            let at = table_offset + term as usize * TABLE_ENTRY_BYTES;
            let entry = TermImageEntry::decode(&mmap[at..at + TABLE_ENTRY_BYTES]);
            if entry.containers != entry.arrays + entry.runs + entry.bitsets {
                return Err(TermImageRefusal::EntryMalformed { term });
            }
            if !entry.kept() {
                if entry.len != 0 {
                    return Err(TermImageRefusal::EntryMalformed { term });
                }
                continue;
            }
            if !entry.offset.is_multiple_of(PAYLOAD_ALIGN as u64) {
                return Err(TermImageRefusal::EntryMisaligned { term });
            }
            let end = entry.offset.saturating_add(u64::from(entry.len));
            if entry.offset < payload_offset as u64 || end > file_len as u64 {
                return Err(TermImageRefusal::EntryOutOfPayload { term });
            }
            if entry.offset < previous_end {
                return Err(TermImageRefusal::EntryOverlaps { term });
            }
            if entry.rows <= KEEP_ROWS_PER_CONTAINER * u64::from(entry.containers) {
                return Err(TermImageRefusal::EntryNotDense { term });
            }
            if u64::from(entry.len) < entry.minimum_frozen_bytes() {
                return Err(TermImageRefusal::EntryTooShort { term });
            }
            previous_end = end;
        }

        Ok(TermImages {
            mmap,
            stamp: expected.clone(),
            dict_len,
            table_offset,
            payload_offset,
            file_len,
        })
    }

    /// How many terms the table covers.
    pub fn dict_len(&self) -> u32 {
        self.dict_len
    }

    /// The row space these images belong to.
    pub fn stamp(&self) -> &TermImageStamp {
        &self.stamp
    }

    /// `term`'s table entry, or `None` where `term` is at or above [`Self::dict_len`].
    pub fn entry(&self, term: TermId) -> Option<TermImageEntry> {
        let index = term.raw();
        if index >= self.dict_len {
            return None;
        }
        let at = self.table_offset + index as usize * TABLE_ENTRY_BYTES;
        Some(TermImageEntry::decode(
            &self.mmap[at..at + TABLE_ENTRY_BYTES],
        ))
    }

    /// Does `term` have an image in the payload?
    pub fn kept(&self, term: TermId) -> bool {
        self.entry(term).is_some_and(|entry| entry.kept())
    }

    /// `term`'s image as a view over the mapped payload, or `None` where it has none.
    pub fn view(&self, term: TermId) -> Option<BitmapView<'_>> {
        let entry = self.entry(term)?;
        if !entry.kept() {
            return None;
        }
        let from = entry.offset as usize;
        let to = from + entry.len as usize;
        let bytes = &self.mmap[from..to];
        // SAFETY: `open` checked that `from` is a multiple of 32 and that `from..to` lies inside
        // the payload, and the mapping's base is page-aligned, so the pointer is 32-byte aligned
        // and the length is the one the writer recorded. That the bytes are a well-formed frozen
        // bitmap rests on the bundle's digest sweep over this file, as the module doc states.
        Some(unsafe { BitmapView::deserialize::<Frozen>(bytes) })
    }

    /// The union of the images of whichever of `terms` have one. Empty where none do. Terms at or
    /// above [`Self::dict_len`] are ignored.
    pub fn union(&self, terms: &[TermId]) -> Bitmap {
        let views: Vec<BitmapView<'_>> = terms
            .iter()
            .copied()
            .filter_map(|term| self.view(term))
            .collect();
        if views.is_empty() {
            return Bitmap::new();
        }
        let refs: Vec<&Bitmap> = views.iter().map(|view| &**view).collect();
        Bitmap::fast_or(&refs)
    }

    /// Where the payload starts, for a caller checking the file's shape.
    pub fn payload_offset(&self) -> usize {
        self.payload_offset
    }

    /// The mapped file's length.
    pub fn file_len(&self) -> usize {
        self.file_len
    }
}

// -------------------------------------------------------------------------------------------
// The route chooser
// -------------------------------------------------------------------------------------------

/// The rates the three session-start routes are priced at, in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub struct RouteCosts {
    /// Per held entity, for the walk.
    pub walk_ns_per_entity: f64,
    /// Per array or run container unioned, for the split.
    pub split_ns_per_array_or_run: f64,
    /// Per bitset container unioned, for the split.
    pub split_ns_per_bitset: f64,
    /// Per residual row walked, for the split.
    pub residual_ns_per_row: f64,
    /// Per entity outside the grant, for the complement.
    pub complement_ns_per_entity: f64,
}

/// ⊘ **Modelled 2026-09-16** from the eleven rung-6 principals in the evidence memo §3, not
/// measured as a rule. The walk rate predates the projection walk's bucket pool, which made the
/// walk 15 to 29% faster, so the walk is priced high.
pub const ROUTE_COSTS: RouteCosts = RouteCosts {
    walk_ns_per_entity: 6.5,
    split_ns_per_array_or_run: 350.0,
    split_ns_per_bitset: 1000.0,
    residual_ns_per_row: 11.0,
    complement_ns_per_entity: 11.0,
};

/// What [`choose`] prices a session's three routes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChooserInputs {
    /// Entities the session's fragment holds below `bound`.
    pub held: u64,
    /// The permutation's entity-space width.
    pub bound: u64,
    /// Whether the complement route can answer for this view at all.
    pub complement_valid: bool,
    /// Array and run containers across the kept images the session would union.
    pub kept_arrays_and_runs: u64,
    /// Bitset containers across the same images.
    pub kept_bitsets: u64,
    /// How many of the session's terms have an image.
    pub kept_terms: u32,
    /// An upper bound on the rows the residual walk would produce.
    pub residual_rows: u64,
}

/// How a session builds its row projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Walk every held entity through the permutation.
    Walk,
    /// Union the kept images and walk the rest.
    Split,
    /// Walk the entities outside the grant and subtract from the full row range.
    Complement,
}

/// Price the three routes and take the cheapest.
///
/// A tie goes to the walk, then to the split: the walk's rate is the one measured over the widest
/// set of principals, and the split's inputs are an overcount, so the cheaper-looking route at a
/// tie is the less certain one.
pub fn choose(inputs: &ChooserInputs, costs: &RouteCosts) -> Route {
    let mut route = Route::Walk;
    let mut cheapest = costs.walk_ns_per_entity * inputs.held as f64;

    // With no kept image the split is the walk plus the cost of deciding to do it.
    if inputs.kept_terms > 0 {
        let split = costs.split_ns_per_array_or_run * inputs.kept_arrays_and_runs as f64
            + costs.split_ns_per_bitset * inputs.kept_bitsets as f64
            + costs.residual_ns_per_row * inputs.residual_rows as f64;
        if split < cheapest {
            route = Route::Split;
            cheapest = split;
        }
    }
    if inputs.complement_valid {
        let outside = inputs.bound.saturating_sub(inputs.held);
        let complement = costs.complement_ns_per_entity * outside as f64;
        if complement < cheapest {
            route = Route::Complement;
        }
    }
    route
}

/// Sum the table over a session's satisfied terms into [`ChooserInputs`].
///
/// `satisfied` is ascending and deduplicated; `delta_rows` is the caller's sum of the same terms'
/// delta-posting cardinalities, which no image covers. Terms at or above the table's length have no
/// base posting and contribute nothing here; their rows arrive in `delta_rows`.
pub fn chooser_inputs(
    images: &TermImages,
    satisfied: &[TermId],
    held: u64,
    bound: u64,
    complement_valid: bool,
    delta_rows: u64,
) -> ChooserInputs {
    debug_assert!(
        satisfied.windows(2).all(|pair| pair[0] < pair[1]),
        "satisfied terms must be ascending and deduplicated"
    );
    let mut inputs = ChooserInputs {
        held,
        bound,
        complement_valid,
        ..ChooserInputs::default()
    };
    for term in satisfied.iter().copied() {
        let Some(entry) = images.entry(term) else {
            continue;
        };
        if entry.kept() {
            inputs.kept_terms += 1;
            inputs.kept_arrays_and_runs += u64::from(entry.arrays) + u64::from(entry.runs);
            inputs.kept_bitsets += u64::from(entry.bitsets);
        } else {
            inputs.residual_rows += entry.rows;
        }
    }
    inputs.residual_rows += delta_rows;
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tessera_types::EntityId;

    use crate::permutation::Permutation;

    fn stamp_of(space: &RowSpace) -> TermImageStamp {
        TermImageStamp {
            prefix: "prefix-000001".to_string(),
            view: "default".to_string(),
            base_seg_id: "seg-000001".to_string(),
            incarnation: 3,
            base_rows: space.base_rows(),
            bound: space.base().bound(),
        }
    }

    /// An identity permutation over `bound` entities, so an image's rows are its posting's
    /// entities and a test can place a container exactly.
    fn identity_space(dir: &Path, bound: u64) -> RowSpace {
        let entities: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
        let path = dir.join("permutation.bin");
        crate::write::write_permutation(&path, &entities, bound).expect("write_permutation");
        let perm = Permutation::load(&path).expect("load permutation");
        RowSpace::new(Arc::new(perm), bound as u32)
    }

    fn derive(space: &RowSpace, postings: &[Bitmap], out: &Path) -> TermImageSummary {
        let walk = |term: u32, visit: &mut dyn FnMut(PostingSlice<'_>)| -> io::Result<()> {
            if let Some(posting) = postings.get(term as usize) {
                visit(PostingSlice::Roaring(posting));
            }
            Ok(())
        };
        derive_term_images(
            space,
            postings.len() as u32,
            &walk,
            &stamp_of(space),
            out,
            DeriveOptions::default(),
        )
        .expect("derive")
    }

    /// A posting whose image has exactly `KEEP_ROWS_PER_CONTAINER` rows in each of two containers
    /// is not kept; one more row in the second container is.
    #[test]
    fn the_keep_rule_cuts_at_thirty_rows_per_container() {
        let dir = tempfile::tempdir().expect("tempdir");
        let space = identity_space(dir.path(), 1 << 17);
        let per = KEEP_ROWS_PER_CONTAINER as u32;

        let mut exactly = Bitmap::new();
        let mut one_more = Bitmap::new();
        for i in 0..per {
            exactly.add(i);
            exactly.add((1 << 16) + i);
            one_more.add(i);
            one_more.add((1 << 16) + i);
        }
        one_more.add((1 << 16) + per);

        let out = dir.path().join("boundary.timg");
        let postings = vec![exactly, one_more];
        let summary = derive(&space, &postings, &out);
        assert_eq!(summary.derived, 2, "both postings are above the skip");
        assert_eq!(summary.kept, 1);

        let images = TermImages::open(&out, &stamp_of(&space), 2).expect("a derived file opens");
        let at_cut = images.entry(TermId::new(0)).expect("term 0");
        assert_eq!(at_cut.rows, 2 * u64::from(per));
        assert_eq!(at_cut.containers, 2);
        assert!(!at_cut.kept(), "exactly 30 rows per container is not kept");
        let above_cut = images.entry(TermId::new(1)).expect("term 1");
        assert_eq!(above_cut.rows, 2 * u64::from(per) + 1);
        assert_eq!(above_cut.containers, 2);
        assert!(above_cut.kept(), "one row above the cut is kept");
    }

    /// A posting of exactly `KEEP_ROWS_PER_CONTAINER` entities is not projected: its row is the
    /// posting's cardinality with no container counts.
    #[test]
    fn a_posting_at_the_skip_is_not_projected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let space = identity_space(dir.path(), 1 << 17);
        let mut small = Bitmap::new();
        for i in 0..KEEP_ROWS_PER_CONTAINER as u32 {
            small.add(i);
        }
        let out = dir.path().join("small.timg");
        let summary = derive(&space, &[small], &out);
        assert_eq!(summary.derived, 0);
        assert_eq!(summary.skipped_small, 1);
        assert_eq!(summary.kept, 0);

        let images = TermImages::open(&out, &stamp_of(&space), 1).expect("opens");
        let entry = images.entry(TermId::new(0)).expect("term 0");
        assert_eq!(entry.rows, KEEP_ROWS_PER_CONTAINER);
        assert_eq!(entry.containers, 0);
        assert!(!entry.kept());
    }

    /// A term the walk carries no record of has an all-zero row.
    #[test]
    fn a_term_with_no_posting_has_an_empty_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let space = identity_space(dir.path(), 1 << 17);
        let walk =
            |_term: u32, _visit: &mut dyn FnMut(PostingSlice<'_>)| -> io::Result<()> { Ok(()) };
        let out = dir.path().join("absent.timg");
        let summary = derive_term_images(
            &space,
            4,
            &walk,
            &stamp_of(&space),
            &out,
            DeriveOptions::default(),
        )
        .expect("derive");
        assert_eq!(summary.terms, 4);
        assert_eq!(summary.derived, 0);
        assert_eq!(summary.skipped_small, 0);

        let images = TermImages::open(&out, &stamp_of(&space), 4).expect("opens");
        for term in 0..4u32 {
            assert_eq!(
                images.entry(TermId::new(term)),
                Some(TermImageEntry::default())
            );
        }
        assert_eq!(images.entry(TermId::new(4)), None, "above the dictionary");
    }

    /// A stamp naming another row space is refused before anything is written.
    #[test]
    fn a_stamp_that_does_not_match_the_row_space_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let space = identity_space(dir.path(), 1 << 17);
        let mut stamp = stamp_of(&space);
        stamp.base_rows += 1;
        let walk =
            |_term: u32, _visit: &mut dyn FnMut(PostingSlice<'_>)| -> io::Result<()> { Ok(()) };
        let out = dir.path().join("mismatched.timg");
        let refused = derive_term_images(&space, 1, &walk, &stamp, &out, DeriveOptions::default());
        assert!(refused.is_err());
    }

    // ----------------------------------------------------------------------------------------
    // Refusals
    // ----------------------------------------------------------------------------------------

    /// A file with two kept images, a skipped term and an absent term.
    fn valid_file(dir: &Path) -> (RowSpace, TermImageStamp, std::path::PathBuf) {
        let space = identity_space(dir, 1 << 18);
        let mut dense = Bitmap::new();
        for i in 0..4000u32 {
            dense.add(i);
        }
        let mut scattered = Bitmap::new();
        for i in 0..3000u32 {
            scattered.add((1 << 16) + i * 3);
        }
        let mut small = Bitmap::new();
        for i in 0..5u32 {
            small.add(i);
        }
        let postings = [dense, scattered, small];
        let walk = |term: u32, visit: &mut dyn FnMut(PostingSlice<'_>)| -> io::Result<()> {
            if let Some(posting) = postings.get(term as usize) {
                visit(PostingSlice::Roaring(posting));
            }
            Ok(())
        };
        let stamp = stamp_of(&space);
        let out = dir.join("valid.timg");
        let summary = derive_term_images(&space, 4, &walk, &stamp, &out, DeriveOptions::default())
            .expect("derive");
        assert_eq!(summary.kept, 2, "the fixture needs two kept images");
        (space, stamp, out)
    }

    /// Write `bytes` to a fresh path beside `original` and open it against `stamp`.
    fn open_mutated(
        original: &Path,
        name: &str,
        stamp: &TermImageStamp,
        dict_len: u32,
        mutate: impl FnOnce(&mut Vec<u8>),
    ) -> Result<TermImages, TermImageRefusal> {
        let mut bytes = std::fs::read(original).expect("read");
        mutate(&mut bytes);
        let path = original.with_file_name(name);
        std::fs::write(&path, &bytes).expect("write");
        TermImages::open(&path, stamp, dict_len)
    }

    fn first_kept_entry_at(bytes: &[u8]) -> usize {
        let dict_len = le_u32(bytes, OFF_DICT_LEN);
        for term in 0..dict_len as usize {
            let at = HEADER_BYTES + term * TABLE_ENTRY_BYTES;
            if le_u64(bytes, at + ENTRY_OFFSET) != 0 {
                return at;
            }
        }
        panic!("the fixture has no kept image");
    }

    fn second_kept_entry_at(bytes: &[u8]) -> usize {
        let dict_len = le_u32(bytes, OFF_DICT_LEN);
        let mut seen = 0;
        for term in 0..dict_len as usize {
            let at = HEADER_BYTES + term * TABLE_ENTRY_BYTES;
            if le_u64(bytes, at + ENTRY_OFFSET) != 0 {
                seen += 1;
                if seen == 2 {
                    return at;
                }
            }
        }
        panic!("the fixture has fewer than two kept images");
    }

    #[test]
    fn every_open_check_refuses_its_own_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_space, stamp, out) = valid_file(dir.path());

        TermImages::open(&out, &stamp, 4).expect("the unmutated file opens");

        macro_rules! case {
            ($name:literal, $pattern:pat, $mutate:expr) => {{
                let refusal = open_mutated(&out, $name, &stamp, 4, $mutate);
                match refusal {
                    Err($pattern) => {}
                    Err(other) => panic!("{}: refused as {other:?}", $name),
                    Ok(_) => panic!("{}: opened", $name),
                }
            }};
        }

        case!("magic.timg", TermImageRefusal::Magic, |b: &mut Vec<u8>| {
            b[0] ^= 0xff;
        });
        case!(
            "version.timg",
            TermImageRefusal::Version { .. },
            |b: &mut Vec<u8>| {
                b[OFF_HEADER_VERSION] = 9;
            }
        );
        case!(
            "header-bounds.timg",
            TermImageRefusal::HeaderBounds,
            |b: &mut Vec<u8>| {
                let wrong = (b.len() as u64 + 64).to_le_bytes();
                b[OFF_FILE_LEN..OFF_FILE_LEN + 8].copy_from_slice(&wrong);
            }
        );
        case!(
            "keep-rule.timg",
            TermImageRefusal::KeepRowsPerContainer { .. },
            |b: &mut Vec<u8>| {
                b[OFF_KEEP_ROWS..OFF_KEEP_ROWS + 4].copy_from_slice(&10u32.to_le_bytes());
            }
        );
        case!(
            "dict-len.timg",
            TermImageRefusal::DictLen { .. },
            |b: &mut Vec<u8>| {
                b[OFF_DICT_LEN..OFF_DICT_LEN + 4].copy_from_slice(&5u32.to_le_bytes());
            }
        );
        case!(
            "table-bounds.timg",
            TermImageRefusal::TableBounds,
            |b: &mut Vec<u8>| {
                let wrong = (HEADER_BYTES as u64 + 32).to_le_bytes();
                b[OFF_PAYLOAD_OFFSET..OFF_PAYLOAD_OFFSET + 8].copy_from_slice(&wrong);
            }
        );
        case!(
            "misaligned.timg",
            TermImageRefusal::EntryMisaligned { .. },
            |b: &mut Vec<u8>| {
                let at = first_kept_entry_at(b) + ENTRY_OFFSET;
                let moved = le_u64(b, at) + 8;
                b[at..at + 8].copy_from_slice(&moved.to_le_bytes());
            }
        );
        case!(
            "out-of-payload.timg",
            TermImageRefusal::EntryOutOfPayload { .. },
            |b: &mut Vec<u8>| {
                let at = first_kept_entry_at(b) + ENTRY_LEN;
                b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            }
        );
        case!(
            "overlap.timg",
            TermImageRefusal::EntryOverlaps { .. },
            |b: &mut Vec<u8>| {
                let first = le_u64(b, first_kept_entry_at(b) + ENTRY_OFFSET);
                let at = second_kept_entry_at(b) + ENTRY_OFFSET;
                b[at..at + 8].copy_from_slice(&first.to_le_bytes());
            }
        );
        case!(
            "not-dense.timg",
            TermImageRefusal::EntryNotDense { .. },
            |b: &mut Vec<u8>| {
                let at = first_kept_entry_at(b) + ENTRY_ROWS;
                b[at..at + 8].copy_from_slice(&1u64.to_le_bytes());
            }
        );
        case!(
            "too-short.timg",
            TermImageRefusal::EntryTooShort { .. },
            |b: &mut Vec<u8>| {
                let at = first_kept_entry_at(b) + ENTRY_LEN;
                b[at..at + 4].copy_from_slice(&4u32.to_le_bytes());
            }
        );
        case!(
            "counts.timg",
            TermImageRefusal::EntryMalformed { .. },
            |b: &mut Vec<u8>| {
                let at = first_kept_entry_at(b) + ENTRY_ARRAYS;
                let wrong = le_u32(b, at) + 7;
                b[at..at + 4].copy_from_slice(&wrong.to_le_bytes());
            }
        );
        case!(
            "unkept-with-length.timg",
            TermImageRefusal::EntryMalformed { .. },
            |b: &mut Vec<u8>| {
                // Term 3 has no posting, so its row is all zero.
                let at = HEADER_BYTES + 3 * TABLE_ENTRY_BYTES + ENTRY_LEN;
                b[at..at + 4].copy_from_slice(&16u32.to_le_bytes());
            }
        );
        case!(
            "stamp-digest.timg",
            TermImageRefusal::StampDigest,
            |b: &mut Vec<u8>| {
                b[OFF_STAMP_DIGEST] ^= 0x01;
            }
        );
        case!(
            "incarnation.timg",
            TermImageRefusal::Incarnation { .. },
            |b: &mut Vec<u8>| {
                b[OFF_INCARNATION..OFF_INCARNATION + 8].copy_from_slice(&99u64.to_le_bytes());
            }
        );
        case!(
            "base-rows.timg",
            TermImageRefusal::BaseRows { .. },
            |b: &mut Vec<u8>| {
                let wrong = le_u32(b, OFF_BASE_ROWS) + 1;
                b[OFF_BASE_ROWS..OFF_BASE_ROWS + 4].copy_from_slice(&wrong.to_le_bytes());
            }
        );
        case!(
            "bound.timg",
            TermImageRefusal::Bound { .. },
            |b: &mut Vec<u8>| {
                let wrong = le_u64(b, OFF_BOUND) + 1;
                b[OFF_BOUND..OFF_BOUND + 8].copy_from_slice(&wrong.to_le_bytes());
            }
        );
    }

    /// Each stamp field, mutated one at a time, is refused: the three strings through the digest,
    /// the three numbers on their own.
    #[test]
    fn every_stamp_field_is_compared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_space, stamp, out) = valid_file(dir.path());

        let mut prefix = stamp.clone();
        prefix.prefix.push('x');
        let mut view = stamp.clone();
        view.view.push('x');
        let mut seg = stamp.clone();
        seg.base_seg_id.push('x');
        for wrong in [prefix, view, seg] {
            match TermImages::open(&out, &wrong, 4) {
                Err(TermImageRefusal::StampDigest) => {}
                other => panic!("a changed string must fail the digest, got {other:?}"),
            }
        }

        let mut incarnation = stamp.clone();
        incarnation.incarnation += 1;
        assert!(matches!(
            TermImages::open(&out, &incarnation, 4),
            Err(TermImageRefusal::Incarnation { .. })
        ));
        let mut base_rows = stamp.clone();
        base_rows.base_rows += 1;
        assert!(matches!(
            TermImages::open(&out, &base_rows, 4),
            Err(TermImageRefusal::BaseRows { .. })
        ));
        let mut bound = stamp.clone();
        bound.bound += 1;
        assert!(matches!(
            TermImages::open(&out, &bound, 4),
            Err(TermImageRefusal::Bound { .. })
        ));
    }

    /// Truncated at every byte, a valid file is refused and nothing panics.
    #[test]
    fn a_file_truncated_at_any_byte_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_space, stamp, out) = valid_file(dir.path());
        let bytes = std::fs::read(&out).expect("read");
        let path = out.with_file_name("truncated.timg");
        for cut in 0..bytes.len() {
            std::fs::write(&path, &bytes[..cut]).expect("write");
            assert!(
                TermImages::open(&path, &stamp, 4).is_err(),
                "a file cut at {cut} of {} bytes must be refused",
                bytes.len()
            );
        }
        std::fs::write(&path, &bytes).expect("write");
        TermImages::open(&path, &stamp, 4).expect("the whole file still opens");
    }

    // ----------------------------------------------------------------------------------------
    // The chooser
    // ----------------------------------------------------------------------------------------

    #[test]
    fn each_route_wins_where_its_cost_is_least() {
        let walk = ChooserInputs {
            held: 1_000,
            bound: 2_000,
            complement_valid: true,
            kept_arrays_and_runs: 1_000,
            kept_bitsets: 1_000,
            kept_terms: 4,
            residual_rows: 1_000_000,
        };
        assert_eq!(choose(&walk, &ROUTE_COSTS), Route::Walk);

        let split = ChooserInputs {
            held: 1_000_000_000,
            bound: 2_000_000_000,
            complement_valid: true,
            kept_arrays_and_runs: 1_000,
            kept_bitsets: 100,
            kept_terms: 4,
            residual_rows: 10_000,
        };
        assert_eq!(choose(&split, &ROUTE_COSTS), Route::Split);

        let complement = ChooserInputs {
            held: 1_000_000_000,
            bound: 1_000_001_000,
            complement_valid: true,
            kept_arrays_and_runs: 10_000_000,
            kept_bitsets: 10_000_000,
            kept_terms: 4,
            residual_rows: 10_000_000_000,
        };
        assert_eq!(choose(&complement, &ROUTE_COSTS), Route::Complement);
    }

    #[test]
    fn the_split_needs_a_kept_term_and_the_complement_needs_a_valid_row_space() {
        let no_images = ChooserInputs {
            held: 1_000_000_000,
            bound: 2_000_000_000,
            complement_valid: false,
            kept_arrays_and_runs: 0,
            kept_bitsets: 0,
            kept_terms: 0,
            residual_rows: 0,
        };
        assert_eq!(
            choose(&no_images, &ROUTE_COSTS),
            Route::Walk,
            "a session with no image must not take the split, however cheap it prices"
        );

        let would_be_complement = ChooserInputs {
            held: 1_000_000_000,
            bound: 1_000_001_000,
            complement_valid: false,
            kept_arrays_and_runs: 10_000_000,
            kept_bitsets: 10_000_000,
            kept_terms: 4,
            residual_rows: 10_000_000_000,
        };
        assert_eq!(choose(&would_be_complement, &ROUTE_COSTS), Route::Walk);
    }

    #[test]
    fn a_tie_goes_to_the_walk_then_the_split() {
        // walk = 6.5 × 100 = 650; split = 350 × 1 + 11 × 0 + 1000 × 0 = 350 with two containers.
        let costs = RouteCosts {
            walk_ns_per_entity: 1.0,
            split_ns_per_array_or_run: 1.0,
            split_ns_per_bitset: 1.0,
            residual_ns_per_row: 1.0,
            complement_ns_per_entity: 1.0,
        };
        let tied = ChooserInputs {
            held: 100,
            bound: 200,
            complement_valid: true,
            kept_arrays_and_runs: 100,
            kept_bitsets: 0,
            kept_terms: 1,
            residual_rows: 0,
        };
        assert_eq!(
            choose(&tied, &costs),
            Route::Walk,
            "walk ties beat the rest"
        );

        let split_then_complement = ChooserInputs {
            held: 100,
            bound: 150,
            complement_valid: true,
            kept_arrays_and_runs: 50,
            kept_bitsets: 0,
            kept_terms: 1,
            residual_rows: 0,
        };
        assert_eq!(
            choose(&split_then_complement, &costs),
            Route::Split,
            "the split and the complement both price 50 against the walk's 100"
        );
    }

    #[test]
    fn chooser_inputs_sums_the_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (space, stamp, out) = valid_file(dir.path());
        let images = TermImages::open(&out, &stamp, 4).expect("opens");

        // Terms 0 and 1 are kept, term 2 is the skipped small posting, term 4 is above the table.
        let satisfied = [
            TermId::new(0),
            TermId::new(1),
            TermId::new(2),
            TermId::new(4),
        ];
        let inputs = chooser_inputs(&images, &satisfied, 7_000, space.base().bound(), true, 25);

        let mut arrays_and_runs = 0u64;
        let mut bitsets = 0u64;
        for term in [0u32, 1] {
            let entry = images.entry(TermId::new(term)).expect("kept");
            assert!(entry.kept());
            arrays_and_runs += u64::from(entry.arrays) + u64::from(entry.runs);
            bitsets += u64::from(entry.bitsets);
        }
        let skipped = images.entry(TermId::new(2)).expect("term 2");
        assert!(!skipped.kept());

        assert_eq!(inputs.kept_terms, 2);
        assert_eq!(inputs.kept_arrays_and_runs, arrays_and_runs);
        assert_eq!(inputs.kept_bitsets, bitsets);
        assert_eq!(
            inputs.residual_rows,
            skipped.rows + 25,
            "the unkept term's rows plus the caller's delta rows, and nothing for term 4"
        );
        assert_eq!(inputs.held, 7_000);
        assert_eq!(inputs.bound, space.base().bound());
        assert!(inputs.complement_valid);
    }
}
