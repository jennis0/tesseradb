//! Does a band over the identity column answer a whole-map viewport, and at what cost?
//!
//! A viewport needs two things per tile (architecture §7.2): `C_θ(T)`, how many of the tile's
//! visible rows carry a `tessera_id` below the depth's cut `P_d`, and the `m(T)` smallest such
//! identities. Every shipped route obtains both by reading the 8 B/row identity column, which at
//! the largest corpus here is 28 GB and does not stay resident under the cap the server runs in.
//! This probe runs a second evaluation beside the served one and asks whether a small band-major
//! structure — written by `identity_bands_build` — answers the same question from a few tens of
//! megabytes, and whether the cut index can answer the *position* of a served row at cell
//! resolution without touching `morton.u32` or the residual column.
//!
//! **Nothing here ships.** This is a measurement against structures no bundle format knows about,
//! on a probe branch; the design it informs is
//! `docs/evidence/memos/2026-09-14-whole-map-selection-under-a-cap.md` §4, which the owner has
//! not accepted. The probe directory is `probes/2026-09-14-identity-bands/`.
//!
//! # Three arms over one request
//!
//! - **R**, the reference: `Engine::viewport`, the shipped path, gather included.
//! - **B**, the band route, evaluated here against the builder's files and **the engine's own
//!   composed mask, cut, tile ranges and [`SelectParams`]** ([`tessera_engine::Engine::composed_mask`]).
//!   Sharing those inputs is what makes the comparison one of two evaluations rather than two
//!   transcriptions. Per tile, B asserts its served identity set against R's; a disagreement is
//!   recorded with the tile and the first differing identity and the run continues.
//! - **G**, the render: the scattered `morton.u32` and `residual` reads a served row's position
//!   costs today, against the cut-index lookup that would give the same position at cell
//!   resolution.
//!
//! # What the arms cannot be read as
//!
//! R runs on the engine's pool and B is one thread, so **wall is not comparable between them and
//! CPU is**. B implements the route rather than a tuned version of it: it is an upper bound on
//! what the structure costs, not a lower one.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin identity_bands_probe -- \
//!     --bundle <bundle> --bands <builder out> --principal p100=US,AU,... --out results.json
//! ```

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::hint::black_box;
use std::ops::Range;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use memmap2::Mmap;
use serde_json::{json, Value};

use tessera_engine::occupancy::occupied_tiles_ladder;
use tessera_engine::select::{served_count, SelectParams, Threshold};
use tessera_engine::viewport::{segments_with_row_bases, ViewportRequest};
use tessera_engine::{compose::EffectiveMask, Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::{tiles_for_bbox, Bounds, Tile};
use tessera_store::read::{open_bundle, SegmentData};
use tessera_store::tile_ranges_all;

/// Server defaults, so the reference arm measures a deployment somebody runs (`arms::viewport`,
/// and `viewport_sweep` beside this).
const K_MAX_MARKS: usize = 500;
const THETA_TARGET: u64 = 16;
const MAX_K: usize = 5_000;
const K_MIN: usize = 2;
/// `tessera-server`'s own default (`config.rs`). **`EngineConfig` validates nothing here** — the
/// field is a plain `usize` and the refusal is per request, against the tile count the request
/// demands — so this is the deployment's ceiling rather than a limit the type imposes. At this
/// value a whole-extent request is served to depth 9 (4⁹ = 262,144 tiles) and refused at 10.
const MAX_TILES_PER_REQUEST: usize = 262_144;
/// The deepest whole-extent depth [`MAX_TILES_PER_REQUEST`] admits.
const MAX_WHOLE_EXTENT_DEPTH: u8 = 9;
/// How deep the occupancy ladder is walked, once per principal.
///
/// **One walk answers every case.** A rung is the count of distinct depth-`d` ancestors of the
/// walked tiles, and the depth-`d` ancestors of the occupied depth-16 tiles are exactly the
/// occupied depth-`d` tiles — so `ladder(16).at(d)` equals `ladder(d).at(d)`, which is what
/// `Engine::occupied_tiles` computes for `N_occ(d)`. The tail the two share
/// (`occupancy::finish_ladder`) clamps each rung at `4^d` and takes a running maximum across
/// rungs, both of which are per-rung and depth-independent. The probe therefore walks once at 16
/// and reads the rung each case needs, instead of walking the Morton column once a case.
const LADDER_DEPTH: u8 = 16;
/// The grid is 2^16 × 2^16 (§5.2), so a leaf cell is 2⁻¹⁶ of the extent on each axis.
const GRID: f64 = 65_536.0;
/// The viewport width the pixel figures are quoted at.
const VIEWPORT_PIXELS: f64 = 2_000.0;
/// The `J` of each `top-J.bin` the builder writes, ascending.
const LISTS: [u32; 4] = [4, 6, 8, 10];

#[derive(Parser)]
#[command(about = "Price a band route over the identity column against the shipped selection")]
struct Args {
    /// Bundle root (the directory holding `CURRENT`).
    #[arg(long)]
    bundle: PathBuf,
    /// The `identity_bands_build` output directory.
    #[arg(long)]
    bands: PathBuf,
    /// `NAME=TERM,TERM,...`, repeatable. One authorised session each.
    #[arg(long = "principal")]
    principals: Vec<String>,
    /// The client's drawn-mark budget, which chooses the whole-map depth.
    #[arg(long, default_value_t = 2_000_000u64)]
    budget: u64,
    /// The `k` of the small-`k` cases — the battery's own request shape.
    #[arg(long = "k-small", default_value_t = 30usize)]
    k_small: usize,
    /// Which conditions to run, in this order.
    #[arg(long, value_delimiter = ',', default_value = "cold,hot")]
    conditions: Vec<String>,
    /// Which arms to run: `R` the reference, `B` the band route, `G` the render. `B` alone skips
    /// the reference and the equality comparison, for a re-run on a corpus where equality is
    /// already established; `G` needs `B`, whose served rows it reads.
    #[arg(long, value_delimiter = ',', default_value = "R,B,G")]
    arms: Vec<String>,
    /// Only these cases, by name. Every case by default.
    #[arg(long, value_delimiter = ',')]
    cases: Vec<String>,
    /// Skip step 5's assertion that a served row's cell code is its own `morton.u32` entry.
    ///
    /// The assertion checks the cut index's contract and is **not part of the route**: it reads
    /// the scattered geometry column the route exists to avoid, at one read a served row. A run
    /// measuring the position lookup's cost passes this; a run establishing correctness does not.
    #[arg(long = "no-code-check")]
    no_code_check: bool,
    /// Also compute the `fp16` count beside the exact one.
    ///
    /// Off by default, because it is an experiment on top of the route rather than part of it:
    /// where a list supplies the candidate its identity comes with it, so the exact count needs no
    /// quantised prefix and the two-byte column is read for the comparison alone.
    #[arg(long = "fp16")]
    fp16: bool,
    /// `madvise(MADV_RANDOM)` over the identity column and `lz.u8` before the arms run.
    ///
    /// Both are read scattered, and the kernel's default read-ahead answers a scattered read by
    /// pulling a window around it — which is how an arm touching a few million rows comes to move
    /// tens of gigabytes. **It applies to the mapping**, so it reaches the reference arm's reads
    /// of the same column as well as the band arm's.
    #[arg(long = "madv-random")]
    madv_random: bool,
    /// Where the results go.
    #[arg(long)]
    out: PathBuf,
    /// Also write the reference arm's whole per-tile table — `(tile, visible, served)` and the
    /// served identities — as one NDJSON file per principal and case under this directory.
    ///
    /// Off by default because it is one line a tile: a 262,144-tile case at six principals is
    /// hundreds of megabytes, and the equality it exists to support is checked over every tile in
    /// memory whether or not it is written out.
    #[arg(long = "per-tile")]
    per_tile: Option<PathBuf>,
}

// ------------------------------------------------------------------------------------------
// Process counters
// ------------------------------------------------------------------------------------------

/// `utime + stime` in ticks, `majflt`, and `read_bytes`, read from `/proc/self`.
///
/// All three are process-wide: the engine's pool threads are counted here too, which is what
/// makes CPU the figure to compare R against B by and wall the figure not to.
#[derive(Clone, Copy)]
struct Counters {
    started: Instant,
    cpu_s: f64,
    cpu_ticks: u64,
    majflt: u64,
    read_bytes: u64,
}

/// Process CPU time, every thread summed, at the clock's own resolution.
///
/// `utime + stime` from `/proc/self/stat` is the same quantity in scheduler ticks, which on this
/// kernel is 100 Hz — so an arm under 10 ms reads as 0 there. Both are recorded; this is the one
/// to quote.
fn process_cpu_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes into a `timespec` this scope owns and reads nothing else.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) } != 0 {
        return 0.0;
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn clock_ticks() -> f64 {
    // SAFETY: `sysconf` takes an int and returns a long; no pointers are involved.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as f64
    } else {
        100.0
    }
}

fn proc_stat() -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    // `comm` may hold spaces and parentheses; everything after the LAST ')' is field 3 onward.
    let Some(tail) = text.rfind(')').map(|i| &text[i + 1..]) else {
        return (0, 0);
    };
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let at = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f.parse::<u64>().ok())
            .unwrap_or(0)
    };
    // Field 12 is `majflt`, 14 `utime`, 15 `stime`; `fields[i]` is field `i + 3`.
    (at(9), at(11) + at(12))
}

fn io_read_bytes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/io") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("read_bytes:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

impl Counters {
    fn now() -> Self {
        let (majflt, cpu_ticks) = proc_stat();
        Counters {
            started: Instant::now(),
            cpu_s: process_cpu_s(),
            cpu_ticks,
            majflt,
            read_bytes: io_read_bytes(),
        }
    }

    /// The deltas since this snapshot, as JSON.
    fn since(&self, ticks: f64) -> Value {
        let wall_s = self.started.elapsed().as_secs_f64();
        let cpu_s = process_cpu_s() - self.cpu_s;
        let (majflt, cpu_ticks) = proc_stat();
        json!({
            "wall_s": wall_s,
            "cpu_s": cpu_s,
            "cpu_ticks_s": (cpu_ticks.saturating_sub(self.cpu_ticks)) as f64 / ticks,
            "majflt": majflt.saturating_sub(self.majflt),
            "read_bytes": io_read_bytes().saturating_sub(self.read_bytes),
        })
    }
}

/// `posix_fadvise(POSIX_FADV_DONTNEED)` over every file under each root — the eviction available
/// without root on this box, and the one `serve_battery.py` uses.
///
/// **It does not reach a page another process holds mapped**, so a cold arm whose major-fault
/// delta is zero proved nothing about being cold. Every arm records that delta beside its wall.
fn evict(roots: &[&Path]) -> usize {
    fn walk(path: &Path, advised: &mut usize) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, advised);
            } else if let Ok(file) = File::open(&path) {
                // SAFETY: a read-only advisory call on a descriptor this scope owns.
                let rc = unsafe {
                    libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
                };
                if rc == 0 {
                    *advised += 1;
                }
            }
        }
    }
    let mut advised = 0;
    for root in roots {
        walk(root, &mut advised);
    }
    advised
}

/// `madvise(MADV_RANDOM)` over one mapped byte range, clamped to whole pages, returning the bytes
/// advised.
///
/// The address a caller hands over sits inside a mapping and need not be page-aligned — an Arrow
/// column starts at whatever offset its IPC header leaves — so the start rounds up and the length
/// rounds down. A refusal leaves a correct mapping that is merely no gentler than before, so it
/// returns zero rather than failing.
fn madvise_random(ptr: *const u8, bytes: usize) -> usize {
    const PAGE: usize = 4096;
    let start = ptr as usize;
    let aligned = start.div_ceil(PAGE) * PAGE;
    let end = start + bytes;
    if end <= aligned {
        return 0;
    }
    let len = (end - aligned) / PAGE * PAGE;
    if len == 0 {
        return 0;
    }
    // SAFETY: the range lies inside a mapping this process holds for the whole run, and
    // `MADV_RANDOM` changes read-ahead and nothing else.
    let rc = unsafe { libc::madvise(aligned as *mut libc::c_void, len, libc::MADV_RANDOM) };
    if rc == 0 {
        len
    } else {
        0
    }
}

// ------------------------------------------------------------------------------------------
// The band files
// ------------------------------------------------------------------------------------------

/// The builder's output, mapped: what the proposed structure would be at open.
struct Bands {
    lz: Mmap,
    fp16: Mmap,
    /// `(J, entries)` ascending in `J`; each entry is `(row: u32, id: u64, code: u32)`, 16 B.
    lists: Vec<(u32, Mmap)>,
    cell_codes: Mmap,
    bands_json: Value,
}

/// `(row: u32, id: u64, code: u32)`, little-endian, packed.
const ENTRY: usize = 16;

impl Bands {
    fn open(dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let map = |name: &str| -> Result<Mmap, Box<dyn std::error::Error>> {
            let file = File::open(dir.join(name))?;
            // SAFETY: read-only for this struct's lifetime, as every other mapping in this tree.
            Ok(unsafe { Mmap::map(&file)? })
        };
        let mut lists = Vec::new();
        for &j in &LISTS {
            lists.push((j, map(&format!("top-{j}.bin"))?));
        }
        Ok(Bands {
            lz: map("lz.u8")?,
            fp16: map("fp16.u16")?,
            lists,
            cell_codes: map("cell-codes.u32")?,
            bands_json: serde_json::from_slice(&std::fs::read(dir.join("bands.json"))?)?,
        })
    }

    fn lz(&self, row: u32) -> u32 {
        u32::from(self.lz[row as usize])
    }

    fn fp16(&self, row: u32) -> u16 {
        let at = row as usize * 2;
        u16::from_le_bytes([self.fp16[at], self.fp16[at + 1]])
    }

    fn cell_code(&self, cell: usize) -> u32 {
        let at = cell * 4;
        u32::from_le_bytes(self.cell_codes[at..at + 4].try_into().expect("four bytes"))
    }

    /// The widest list whose `J` is at or below `j`, and its entries — `None` where `j` is below
    /// every list the builder wrote and the `lz` column is the only source.
    fn list_for(&self, j: u32) -> Option<(u32, &Mmap)> {
        self.lists
            .iter()
            .rev()
            .find(|(lj, _)| *lj <= j)
            .map(|(lj, m)| (*lj, m))
    }
}

/// One entry of a `top-J.bin`.
fn entry_at(list: &Mmap, i: usize) -> (u32, u64, u32) {
    let at = i * ENTRY;
    let row = u32::from_le_bytes(list[at..at + 4].try_into().expect("four bytes"));
    let id = u64::from_le_bytes(list[at + 4..at + 12].try_into().expect("eight bytes"));
    let code = u32::from_le_bytes(list[at + 12..at + 16].try_into().expect("four bytes"));
    (row, id, code)
}

/// The first index of `list` whose row is at or after `row`. The entries are written in row
/// order, so this is a binary search.
fn seek_row(list: &Mmap, row: u32) -> usize {
    let (mut lo, mut hi) = (0usize, list.len() / ENTRY);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if entry_at(list, mid).0 < row {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The identity ordering implied by two quantised prefixes, or `Equal` where they share one and
/// nothing short of a full identity read separates them.
///
/// **The packed `u16` is not itself monotone in the identity.** A larger leading-zero count is a
/// smaller identity while a larger fraction is a larger one, so the two fields are compared in
/// opposite directions — see `identity_bands_build`'s `fp16_of`.
fn fp16_cmp(a: u16, b: u16) -> Ordering {
    let (lz_a, fr_a) = (a >> 10, a & 0x3FF);
    let (lz_b, fr_b) = (b >> 10, b & 0x3FF);
    lz_b.cmp(&lz_a).then(fr_a.cmp(&fr_b))
}

/// [`identity_bands_build::fp16_of`], transcribed because a binary cannot call another's private
/// function. The builder's doc carries the definition; this must agree with it, and the probe's
/// `fp16` count against the exact count is what says whether it does.
fn fp16_of(id: u64) -> u16 {
    let lz = id.leading_zeros();
    if lz >= 64 {
        return 63 << 10;
    }
    let frac = if lz >= 63 {
        0
    } else {
        ((id << (lz + 1)) >> 54) as u16
    };
    ((lz.min(63) as u16) << 10) | frac
}

/// The `cap` smallest `(identity, row)` pairs of a stream, in `O(cap)`.
struct Smallest {
    cap: usize,
    heap: BinaryHeap<(u64, u32)>,
}

impl Smallest {
    fn new(cap: usize) -> Self {
        Smallest {
            cap,
            heap: BinaryHeap::with_capacity(cap.saturating_add(1)),
        }
    }

    fn offer(&mut self, id: u64, row: u32) {
        if self.cap == 0 {
            return;
        }
        if self.heap.len() == self.cap {
            if id >= self.heap.peek().expect("non-empty at len == cap").0 {
                return;
            }
            self.heap.pop();
        }
        self.heap.push((id, row));
    }

    /// The `m` smallest, ascending by identity.
    fn take(self, m: usize) -> Vec<(u64, u32)> {
        let mut kept = self.heap.into_vec();
        kept.sort_unstable();
        kept.truncate(m);
        kept
    }
}

// ------------------------------------------------------------------------------------------
// The band route
// ------------------------------------------------------------------------------------------

/// What one band-route evaluation of one request read and answered.
#[derive(Default)]
struct BandOutcome {
    tiles: u64,
    /// Tiles the band actually settled — everything but the fallback scans. The quantised counts
    /// below are accumulated over these alone, so their ratios against [`Self::exact_banded`] are
    /// taken over the same tiles.
    band_tiles: u64,
    j_sum: u64,
    j_min: u32,
    j_max: u32,
    /// `Σ|S|`, the candidate rows the band offered, after masking.
    s_total: u64,
    /// `Σ|{row ∈ S : lz ≥ j + 1}|` — the next band up, as a quantised count. Free: every
    /// candidate's identity is already in hand, so its leading-zero count is a register operation
    /// and reads nothing.
    band_above_total: u64,
    /// The `fp16` count and the identity reads its ties forced, or zero where `--fp16` was not
    /// asked for. [`Self::fp16_measured`] says which.
    fp16_total: u64,
    fp16_tie_reads: u64,
    fp16_measured: bool,
    /// `Σ C_θ`, the exact count, over every tile.
    exact_total: u64,
    /// `Σ C_θ` over the band-settled tiles alone.
    exact_banded: u64,
    served_total: u64,
    column_reads: u64,
    list_bytes: u64,
    list_entries_walked: u64,
    lz_bytes: u64,
    fp16_bytes: u64,
    /// Served rows whose cell code arrived with the `top-J.bin` entry that offered them, so the
    /// position cost nothing beyond the list read the search had already paid for.
    codes_from_list: u64,
    /// Served rows whose cell code needed one `cuts.u32` binary search and one `cell-codes.u32`
    /// read, because no list supplied them.
    codes_from_cut_index: u64,
    floor_widened_tiles: u64,
    /// How the floor was settled, where it bound: by the band already in hand, by a wider list, or
    /// by reading the tile's visible identities from the column.
    floor_settled_by_band: u64,
    floor_settled_by_list: u64,
    floor_settled_by_column: u64,
    /// Shares of [`Self::list_bytes`] and [`Self::column_reads`] the floor's widening accounts
    /// for, so the floor's cost is separable from the search's.
    floor_list_bytes: u64,
    floor_column_rows_read: u64,
    fallback_scan_tiles: u64,
    /// Tiles whose threshold was saturated, so no cut existed to band against.
    saturated_tiles: u64,
    /// Tiles whose cut lay in the top band (`j == 0`), which excludes no identity.
    top_band_tiles: u64,
    /// Steps 1 to 4 — candidates, counts and the served set — over every tile.
    search_wall_s: f64,
    search_cpu_s: f64,
    /// Step 5 — each served row's position at cell resolution — over every served row.
    position_wall_s: f64,
    position_cpu_s: f64,
    /// Served rows, every tile concatenated — the render arm's input.
    served_rows: Vec<u32>,
    /// Whether a reference arm's served identities were available to compare against at all.
    compared: bool,
    disagreements: Vec<Value>,
}

impl BandOutcome {
    fn new() -> Self {
        BandOutcome {
            j_min: u32::MAX,
            ..Default::default()
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "tiles": self.tiles,
            "band_tiles": self.band_tiles,
            "j_min": if self.j_min == u32::MAX { Value::Null } else { json!(self.j_min) },
            "j_max": self.j_max,
            "j_mean": if self.tiles == 0 { 0.0 } else { self.j_sum as f64 / self.tiles as f64 },
            "s_total": self.s_total,
            "band_above_total": self.band_above_total,
            "fp16_measured": self.fp16_measured,
            "fp16_total": self.fp16_total,
            "fp16_tie_reads": self.fp16_tie_reads,
            "exact_total": self.exact_total,
            "exact_banded": self.exact_banded,
            "served_total": self.served_total,
            "column_reads": self.column_reads,
            "list_bytes": self.list_bytes,
            "list_entries_walked": self.list_entries_walked,
            "lz_bytes": self.lz_bytes,
            "fp16_bytes": self.fp16_bytes,
            "codes_from_list": self.codes_from_list,
            "codes_from_cut_index": self.codes_from_cut_index,
            "floor_widened_tiles": self.floor_widened_tiles,
            "floor_settled_by_band": self.floor_settled_by_band,
            "floor_settled_by_list": self.floor_settled_by_list,
            "floor_settled_by_column": self.floor_settled_by_column,
            "floor_list_bytes": self.floor_list_bytes,
            "floor_column_rows_read": self.floor_column_rows_read,
            "fallback_scan_tiles": self.fallback_scan_tiles,
            "saturated_tiles": self.saturated_tiles,
            "top_band_tiles": self.top_band_tiles,
            "search_wall_s": self.search_wall_s,
            "search_cpu_s": self.search_cpu_s,
            "position_wall_s": self.position_wall_s,
            "position_cpu_s": self.position_cpu_s,
            "compared": self.compared,
            "disagreeing_tiles": self.disagreements.len(),
            "disagreements": self.disagreements,
        })
    }
}

/// Everything the band route and the reference arm share for one request.
struct Inputs<'a> {
    mask: &'a EffectiveMask,
    segment: &'a SegmentData,
    bands: &'a Bands,
    params: &'a SelectParams,
    tiles: &'a [Tile],
    ranges: &'a [Range<u32>],
}

/// What the band arm does beyond answering the definition.
#[derive(Clone, Copy)]
struct BandOptions {
    /// Compute the `fp16` count beside the exact one. **An experiment on top of the route, not
    /// part of it**: where a list supplied the candidate its identity came with it, so the exact
    /// count needs no quantised prefix and the two-byte column is read for the comparison alone.
    fp16: bool,
    /// Assert each served row's cell code against its own `morton.u32` entry. A check on the cut
    /// index's contract, not part of the route, and it reads the scattered geometry column the
    /// route exists to avoid.
    check_codes: bool,
}

/// One tile's served rows inside the flat served list, with what the search concluded about it.
struct TileGroup {
    tile: u64,
    depth: u8,
    visible: u64,
    j: u32,
    exact: u64,
    start: usize,
    len: usize,
}

/// The entries of one `top-J.bin` inside `range` that the mask admits, as `(row, id, code)`.
///
/// Every entry of the list carries `lz >= J` by construction, so a caller wanting a narrower band
/// than the list's own filters on the identity afterwards.
fn list_candidates(
    list: &Mmap,
    range: &Range<u32>,
    mask: &EffectiveMask,
    entries_walked: &mut u64,
    out: &mut Vec<(u32, u64, Option<u32>)>,
) {
    out.clear();
    let mut i = seek_row(list, range.start);
    let n = list.len() / ENTRY;
    while i < n {
        let (row, id, code) = entry_at(list, i);
        if row >= range.end {
            break;
        }
        *entries_walked += 1;
        if mask.contains_row(row) {
            out.push((row, id, Some(code)));
        }
        i += 1;
    }
}

/// Evaluate §7.2's definition over every tile of one request by the band route.
///
/// **Three phases, and only the first two are the route.** The search (steps 1 to 4) and the
/// position lookup (step 5) are separately timed because they answer different questions and cost
/// differently: the search is what a band replaces the identity column with, the position is what
/// the cut index replaces the geometry columns with. The comparison against `reference` is the
/// probe's own check and is timed as part of neither. `reference` is `None` when the reference arm
/// was not run, and then no comparison is made and [`BandOutcome::compared`] records that.
fn band_route(
    inputs: &Inputs<'_>,
    reference: Option<&HashMap<u64, (u64, Vec<u64>)>>,
    options: BandOptions,
) -> BandOutcome {
    let mut out = BandOutcome::new();
    out.compared = reference.is_some();
    out.fp16_measured = options.fp16;
    let ids = inputs.segment.columns.tessera_id();
    let starts = inputs.segment.cuts.starts();
    let cut = match inputs.params.threshold {
        Threshold::Cut(cut) => Some(cut),
        Threshold::Saturated => None,
    };
    let cut_fp16 = cut.map(fp16_of);

    // Every served row of the request, in tile order, with the cell code where a list supplied
    // one. `groups` indexes into it per tile, so the position phase is one flat pass and the
    // comparison needs no second walk of the tiles.
    let mut served: Vec<(u64, u32, Option<u32>)> = Vec::new();
    let mut groups: Vec<TileGroup> = Vec::new();
    let mut candidates: Vec<(u32, u64, Option<u32>)> = Vec::new();
    let mut wider: Vec<(u32, u64, Option<u32>)> = Vec::new();

    // ---- Phase 1: the search, steps 1 to 4.
    let search_started = Instant::now();
    let search_cpu = process_cpu_s();
    for (tile, range) in inputs.tiles.iter().zip(inputs.ranges) {
        if range.start >= range.end {
            continue;
        }
        let visible = inputs.mask.count_range(range.clone());
        if visible == 0 {
            continue;
        }
        out.tiles += 1;

        // Step 1: the band holding the cut. `{id < P_d} ⊆ {lz ≥ j}` because an identity below a
        // cut with `j` leading zeros has at least `j` of its own. A saturated threshold, or a cut
        // in the top band, leaves nothing for a band to exclude.
        let j = cut.map_or(0, |c| c.leading_zeros());
        out.j_sum += u64::from(j);
        out.j_min = out.j_min.min(j);
        out.j_max = out.j_max.max(j);

        // `(row, id, code)`: the code is present exactly when a list offered the row, and it is
        // the row's cell code, so step 5 has nothing left to look up for it.
        candidates.clear();
        let mut from_scan = false;
        if cut.is_none() || j == 0 {
            // The whole tile is the band, so it excludes nothing and every visible identity is
            // read. That is the scan, and it is the route's floor rather than an error.
            if cut.is_none() {
                out.saturated_tiles += 1;
            } else {
                out.top_band_tiles += 1;
            }
            out.fallback_scan_tiles += 1;
            from_scan = true;
        } else if let Some((list_j, list)) = inputs.bands.list_for(j) {
            // Step 2a: the band's own list, located by binary search and then walked. The list's
            // `J` is at or below `j`, so its entries are filtered down to band `j` here.
            let before = out.list_entries_walked;
            list_candidates(
                list,
                range,
                inputs.mask,
                &mut out.list_entries_walked,
                &mut candidates,
            );
            out.list_bytes += (out.list_entries_walked - before) * ENTRY as u64;
            if list_j < j {
                candidates.retain(|&(_, id, _)| id.leading_zeros() >= j);
            }
        } else {
            // Step 2b: no list is narrow enough, so the `lz` column decides membership and the
            // identity column answers each member. This is the sparse-principal route, and it is
            // the only reader of `lz.u8`.
            inputs.mask.for_each_visible_run(range.clone(), |run| {
                for row in run {
                    out.lz_bytes += 1;
                    if inputs.bands.lz(row) >= j {
                        candidates.push((row, ids[row as usize], None));
                        out.column_reads += 1;
                    }
                }
            });
        }

        // Step 3: the exact count, and the quantised counts beside it.
        let mut exact: u64 = 0;
        // Assigned exactly once, by whichever route below settles the tile.
        let served_source: Vec<(u64, u32, Option<u32>)>;
        if from_scan {
            let mut smallest = Smallest::new(inputs.params.cap);
            inputs.mask.for_each_visible_run(range.clone(), |run| {
                for row in run {
                    let id = ids[row as usize];
                    out.column_reads += 1;
                    if inputs.params.threshold.admits(id) {
                        exact += 1;
                    }
                    smallest.offer(id, row);
                }
            });
            // No band settled this tile, so no quantised count is accumulated for it.
            let m = served_count(exact, inputs.params, visible);
            served_source = smallest
                .take(m)
                .into_iter()
                .map(|(id, row)| (id, row, None))
                .collect();
        } else {
            out.band_tiles += 1;
            out.s_total += candidates.len() as u64;
            let mut below: Vec<(u64, u32, Option<u32>)> = Vec::with_capacity(candidates.len());
            for &(row, id, code) in &candidates {
                // Band `j + 1`, the next one up. The identity is in hand, so this is a register
                // operation and reads nothing.
                if id.leading_zeros() > j {
                    out.band_above_total += 1;
                }
                if options.fp16 {
                    // The quantised prefixes settle all but a tie, and a tie is one identity read.
                    let row_fp16 = inputs.bands.fp16(row);
                    out.fp16_bytes += 2;
                    match fp16_cmp(
                        row_fp16,
                        cut_fp16.expect("a cut, since `from_scan` is false"),
                    ) {
                        Ordering::Less => out.fp16_total += 1,
                        Ordering::Greater => {}
                        Ordering::Equal => {
                            out.fp16_tie_reads += 1;
                            out.column_reads += 1;
                            if ids[row as usize] < cut.expect("a cut") {
                                out.fp16_total += 1;
                            }
                        }
                    }
                }
                if inputs.params.threshold.admits(id) {
                    exact += 1;
                    below.push((id, row, code));
                }
            }
            let m = served_count(exact, inputs.params, visible);
            if exact >= m as u64 {
                // Identities are unique within a tile, so the third element never decides the
                // order and the sort is by `(id, row)` as it was.
                below.sort_unstable();
                below.truncate(m);
                served_source = below;
            } else {
                // Step 4: the floor binds, so the served set is not a prefix of `{id < P_d}` and
                // the route must widen until some band holds `m` visible rows of the tile. A band
                // is an identity-space prefix, so the `m` smallest visible identities lie inside
                // the first band that holds `m` of them, and taking the `m` smallest of that band
                // is exactly taking the `m` smallest of the tile.
                //
                // **Through the lists, not through `lz.u8`.** Every widening step is one slice of
                // a `top-J.bin` located by binary search, and its entries carry the identities and
                // the codes — so a settled step costs no identity-column read at all. The `lz`
                // column would cost a byte a visible row of the tile and would still leave every
                // identity to be read.
                out.floor_widened_tiles += 1;
                let mut settled: Option<Vec<(u64, u32, Option<u32>)>> = None;
                // The band already in hand comes first: it held fewer than `m` rows *below the
                // cut*, which does not mean it holds fewer than `m` rows at all.
                if candidates.len() >= m {
                    out.floor_settled_by_band += 1;
                    let mut kept: Vec<(u64, u32, Option<u32>)> = candidates
                        .iter()
                        .map(|&(row, id, code)| (id, row, code))
                        .collect();
                    kept.sort_unstable();
                    kept.truncate(m);
                    settled = Some(kept);
                }
                // Then the wider lists, narrowest first. A list whose `J` is at or above `j`
                // addresses a subset of the band just rejected and cannot hold more than it did.
                if settled.is_none() {
                    for (list_j, list) in inputs.bands.lists.iter().rev() {
                        if *list_j >= j {
                            continue;
                        }
                        let before = out.list_entries_walked;
                        list_candidates(
                            list,
                            range,
                            inputs.mask,
                            &mut out.list_entries_walked,
                            &mut wider,
                        );
                        let bytes = (out.list_entries_walked - before) * ENTRY as u64;
                        out.list_bytes += bytes;
                        out.floor_list_bytes += bytes;
                        if wider.len() >= m {
                            out.floor_settled_by_list += 1;
                            let mut kept: Vec<(u64, u32, Option<u32>)> = wider
                                .iter()
                                .map(|&(row, id, code)| (id, row, code))
                                .collect();
                            kept.sort_unstable();
                            kept.truncate(m);
                            settled = Some(kept);
                            break;
                        }
                    }
                }
                served_source = match settled {
                    Some(kept) => kept,
                    None => {
                        // Even the widest list holds fewer than `m` of this tile's visible rows,
                        // so the tile has few visible rows — in expectation under `16 · m` of
                        // them, since the widest list holds one row in sixteen — and reading their
                        // identities directly is a page or two.
                        out.floor_settled_by_column += 1;
                        let mut smallest = Smallest::new(inputs.params.cap);
                        inputs.mask.for_each_visible_run(range.clone(), |run| {
                            for row in run {
                                out.column_reads += 1;
                                out.floor_column_rows_read += 1;
                                smallest.offer(ids[row as usize], row);
                            }
                        });
                        smallest
                            .take(m)
                            .into_iter()
                            .map(|(id, row)| (id, row, None))
                            .collect()
                    }
                };
            }
        }

        out.exact_total += exact;
        if !from_scan {
            out.exact_banded += exact;
        }
        let start = served.len();
        served.extend(served_source);
        groups.push(TileGroup {
            tile: tile.prefix,
            depth: tile.depth,
            visible,
            j,
            exact,
            start,
            len: served.len() - start,
        });
    }
    out.search_wall_s = search_started.elapsed().as_secs_f64();
    out.search_cpu_s = process_cpu_s() - search_cpu;
    out.served_total = served.len() as u64;

    // ---- Phase 2: step 5, each served row's position at cell resolution.
    //
    // A row a list offered brought its cell code with it and needs no lookup at all; every other
    // row costs one binary search over `cuts.u32` and one `cell-codes.u32` read. `check_codes`
    // additionally asserts the answer against `morton.u32`, which is the column the route exists
    // to stop reading — so it is off in a run that is measuring rather than checking.
    let position_started = Instant::now();
    let position_cpu = process_cpu_s();
    let codes = if options.check_codes {
        Some(inputs.segment.morton.u32())
    } else {
        None
    };
    for &(_, row, code) in &served {
        let cell_code = match code {
            Some(code) => {
                out.codes_from_list += 1;
                code
            }
            None => {
                out.codes_from_cut_index += 1;
                let cell = starts.partition_point(|&s| s <= row) - 1;
                inputs.bands.cell_code(cell)
            }
        };
        if let Some(codes) = codes {
            assert_eq!(
                cell_code, codes[row as usize],
                "row {row}'s cell code is not its own Morton code, which the cut index's contract \
                 forbids"
            );
        }
    }
    out.position_wall_s = position_started.elapsed().as_secs_f64();
    out.position_cpu_s = process_cpu_s() - position_cpu;

    out.served_rows = served.iter().map(|&(_, row, _)| row).collect();

    // ---- Phase 3, timed as part of neither: the same served identities as the reference arm, or
    // a recorded disagreement. Skipped where no reference arm ran.
    if let Some(reference) = reference {
        let empty = (0u64, Vec::new());
        for group in &groups {
            let mine: Vec<u64> = served[group.start..group.start + group.len]
                .iter()
                .map(|&(id, _, _)| id)
                .collect();
            let (ref_served, ref_ids) = reference.get(&group.tile).unwrap_or(&empty);
            if mine.len() as u64 != *ref_served || mine != *ref_ids {
                let first_diff = mine
                    .iter()
                    .zip(ref_ids.iter())
                    .find(|(a, b)| a != b)
                    .map(|(a, b)| json!({"band": a, "reference": b}));
                out.disagreements.push(json!({
                    "tile": group.tile,
                    "zoom": group.depth,
                    "visible": group.visible,
                    "band_served": mine.len(),
                    "reference_served": ref_served,
                    "j": group.j,
                    "exact_count": group.exact,
                    "first_differing": first_diff,
                }));
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------------------
// The render arm
// ------------------------------------------------------------------------------------------

/// Distinct 4 KiB pages a set of `u32`-indexed rows touches in a flat 4 B/row array.
///
/// **Modelled.** The bytes are `pages × 4096`; the kernel's read-ahead moves more than that and
/// a page already resident costs none of it.
fn pages_of(rows: &[u32], stride: u64) -> usize {
    let mut pages: HashSet<u64> = HashSet::new();
    for &row in rows {
        pages.insert(u64::from(row) * stride / 4096);
    }
    pages.len()
}

/// The pages a binary search over `starts` probes, for each row.
fn cut_index_pages(starts: &[u32], rows: &[u32]) -> usize {
    let mut pages: HashSet<u64> = HashSet::new();
    for &row in rows {
        let (mut lo, mut hi) = (0usize, starts.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            pages.insert(mid as u64 * 4 / 4096);
            if starts[mid] <= row {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
    }
    pages.len()
}

// ------------------------------------------------------------------------------------------
// Request construction
// ------------------------------------------------------------------------------------------

fn auth_json(terms: &[String]) -> String {
    let list = terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"terms\":[{list}]}}")
}

/// A tile's own bbox, inset by a tenth of a leaf cell on each side so that `tiles_for_bbox`
/// returns this tile at its own depth and exactly its descendants below it.
fn tile_bbox(tile: &Tile, e: &Bounds) -> [f64; 4] {
    let (mut tx, mut ty) = (0u32, 0u32);
    for i in 0..tile.depth {
        tx |= (((tile.prefix >> (2 * i)) & 1) as u32) << i;
        ty |= (((tile.prefix >> (2 * i + 1)) & 1) as u32) << i;
    }
    let shift = 16 - u32::from(tile.depth);
    let (cx0, cy0) = (u64::from(tx) << shift, u64::from(ty) << shift);
    let (cx1, cy1) = (cx0 + (1u64 << shift), cy0 + (1u64 << shift));
    let wx = (e.x_max - e.x_min) / GRID;
    let wy = (e.y_max - e.y_min) / GRID;
    [
        e.x_min + cx0 as f64 * wx + wx * 0.1,
        e.y_min + cy0 as f64 * wy + wy * 0.1,
        e.x_min + cx1 as f64 * wx - wx * 0.1,
        e.y_min + cy1 as f64 * wy - wy * 0.1,
    ]
}

/// How many pixels one leaf cell occupies when `bbox` fills a [`VIEWPORT_PIXELS`]-wide viewport.
///
/// The cut index answers a served row's position to the cell it is in, so the error of the
/// cell-resolution render is under one cell per axis — 2⁻¹⁶ of the extent — and this is that
/// bound in pixels. `cells_across = (bbox width / extent width) · 65536`, and the figure is
/// `2000 / cells_across`, taken as the larger of the two axes.
fn pixels_per_cell(bbox: [f64; 4], e: &Bounds) -> f64 {
    let across_x = (bbox[2] - bbox[0]) / (e.x_max - e.x_min) * GRID;
    let across_y = (bbox[3] - bbox[1]) / (e.y_max - e.y_min) * GRID;
    let px = VIEWPORT_PIXELS / across_x.max(1e-9);
    let py = VIEWPORT_PIXELS / across_y.max(1e-9);
    px.max(py)
}

/// One case: what to ask for, and under what name.
struct Case {
    name: String,
    bbox: [f64; 4],
    zoom: u8,
    k: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let ticks = clock_ticks();
    if args.principals.is_empty() {
        return Err("at least one --principal NAME=TERM,TERM is required".into());
    }
    for arm in &args.arms {
        if !["R", "B", "G"].iter().any(|k| arm.eq_ignore_ascii_case(k)) {
            return Err(format!("--arms takes R, B and G; got '{arm}'").into());
        }
    }
    let named = |name: &str| args.arms.iter().any(|a| a.eq_ignore_ascii_case(name));
    let (run_r, run_b, run_g) = (named("R"), named("B"), named("G"));
    if !(run_r || run_b || run_g) {
        return Err("--arms names no arm".into());
    }
    if run_g && !run_b {
        return Err("the render arm reads the band arm's served rows, so --arms G needs B".into());
    }
    let band_options = BandOptions {
        fp16: args.fp16,
        check_codes: !args.no_code_check,
    };

    // ---- The view, its frame, and the single-segment refusal.
    let (view_id, quantisation, segment_count) = {
        let bundle = open_bundle(&args.bundle)?;
        let quantisation = bundle
            .manifest
            .views
            .first()
            .ok_or("a built bundle declares a view")?
            .quantisation;
        let (view_id, view_data) = bundle
            .partitions
            .values()
            .next()
            .and_then(|p| p.views.iter().next())
            .map(|(id, data)| (id.clone(), data))
            .ok_or("the bundle carries no view")?;
        (view_id, quantisation, view_data.segments.len())
    };
    if segment_count != 1 {
        return Err(format!(
            "view '{view_id}' holds {segment_count} segments; this probe evaluates one segment \
             and has no multi-segment union (§7.2's merge rule), so it refuses rather than \
             measure a fraction of the answer"
        )
        .into());
    }
    let extent = Bounds {
        x_min: quantisation.x_min,
        x_max: quantisation.x_max,
        y_min: quantisation.y_min,
        y_max: quantisation.y_max,
    };
    let full = [extent.x_min, extent.y_min, extent.x_max, extent.y_max];

    let bands = Bands::open(&args.bands)?;

    let tmp = std::env::temp_dir().join(format!("tessera-identity-bands-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let engine = Engine::open(
        &args.bundle,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: MAX_K,
            k_min: K_MIN,
            k_max_marks: K_MAX_MARKS,
            theta_target_marks: THETA_TARGET,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: MAX_TILES_PER_REQUEST,
            compute_threads: tessera_engine::default_compute_threads(),
            flush_max_age_secs: 90,
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )?;

    // ---- Every principal authorised and warmed once, before anything is timed inside an arm.
    struct Principal {
        name: String,
        terms: Vec<String>,
        session: tessera_engine::Session,
        mask: EffectiveMask,
        visible_total: u64,
        ladder: Vec<u64>,
        d_star: u8,
        projection: Value,
        ladder_cost: Value,
    }
    let mut principals: Vec<Principal> = Vec::new();
    let mut generation = None;
    for spec in &args.principals {
        let (name, terms) = spec
            .split_once('=')
            .ok_or_else(|| format!("--principal wants NAME=TERM,TERM, got '{spec}'"))?;
        let terms: Vec<String> = terms.split(',').map(|t| t.trim().to_string()).collect();
        let session = engine.authorise(auth_json(&terms).as_bytes())?;
        // The row-projection build crosses entity space into row space over the whole fragment
        // and belongs to no arm. Taken once, at `k = 1`, and reported on its own.
        let before = Counters::now();
        let warm = engine.viewport(&session, ViewportRequest::new(&view_id, 0, full, 1))?;
        let mut projection = before.since(ticks);
        projection["row_projection_built"] = json!(warm.timings.row_projection_built);
        drop(warm);
        // The background occupancy fill this request spawned runs on the pool to depth 12; let it
        // land rather than have it overlap the first timed arm.
        std::thread::sleep(std::time::Duration::from_millis(500));

        let (gen, mask) = engine.composed_mask(&session, &view_id)?;
        let generation = generation.get_or_insert(gen);
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(&view_id))
            .ok_or("the view vanished between open and compose")?;
        let segments = segments_with_row_bases(&view_id, view_data)?;
        let before = Counters::now();
        let rungs = occupied_tiles_ladder(&mask, &segments, LADDER_DEPTH);
        let ladder_cost = before.since(ticks);
        let ladder: Vec<u64> = (0..=LADDER_DEPTH).map(|d| rungs.at(d)).collect();
        // The depth whose expected mark count is nearest the client's budget: `16 · N_occ(d)`
        // marks at depth `d`, since the mean occupied tile draws `m_target` (§7.2).
        let d_star = (0..=MAX_WHOLE_EXTENT_DEPTH)
            .min_by_key(|&d| (THETA_TARGET * ladder[d as usize]).abs_diff(args.budget))
            .expect("the depth range is not empty");
        let visible_total = mask.visible_total();
        principals.push(Principal {
            name: name.to_string(),
            terms,
            session,
            mask,
            visible_total,
            ladder,
            d_star,
            projection,
            ladder_cost,
        });
    }
    let generation = generation.expect("at least one principal");
    let view_data = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(&view_id))
        .ok_or("the view vanished")?;
    let segments = segments_with_row_bases(&view_id, view_data)?;
    let (segment, row_base) = segments[0];
    if row_base != 0 {
        return Err(
            "the one segment does not begin at row 0; this probe assumes a build segment".into(),
        );
    }

    // The two columns every arm reads scattered. `MADV_RANDOM` applies to the mapping, so this
    // reaches the reference arm's reads of the identity column as well as the band arm's.
    let madv = if args.madv_random {
        let ids = segment.columns.tessera_id();
        let identity = madvise_random(ids.as_ptr() as *const u8, std::mem::size_of_val(ids));
        let lz = madvise_random(bands.lz.as_ptr(), bands.lz.len());
        json!({"applied": true, "identity_bytes": identity, "lz_bytes": lz})
    } else {
        json!({"applied": false})
    };

    // ---- The zoom-`z` locations, chosen once under the widest principal so that every principal
    // is measured at the same places.
    let widest = principals
        .iter()
        .enumerate()
        .max_by_key(|(_, p)| p.visible_total)
        .map(|(i, _)| i)
        .expect("at least one principal");
    let mut locations: Vec<(u8, Tile, u64)> = Vec::new();
    for z in [2u8, 4, 6, 8] {
        let tiles = tiles_for_bbox(full, z, &extent);
        let ranges = tile_ranges_all(segment, &tiles);
        let mut best = (0u64, tiles[0]);
        for (tile, range) in tiles.iter().zip(&ranges) {
            if range.start >= range.end {
                continue;
            }
            let visible = principals[widest].mask.count_range(range.clone());
            if visible > best.0 {
                best = (visible, *tile);
            }
        }
        locations.push((z, best.1, best.0));
    }

    // ---- Run. The two roots a cold arm advises away: the bundle the engine reads and the band
    // files this probe reads.
    let roots = [args.bundle.as_path(), args.bands.as_path()];
    let mut results: Vec<Value> = Vec::new();
    let mut disagreeing_tiles = 0usize;
    for principal in &principals {
        let mut cases: Vec<Case> = vec![
            Case {
                name: "whole_k30".to_string(),
                bbox: full,
                zoom: 0,
                k: args.k_small,
            },
            Case {
                name: "whole_budget".to_string(),
                bbox: full,
                zoom: principal.d_star,
                k: MAX_K,
            },
        ];
        for &(z, tile, _) in &locations {
            let bbox = tile_bbox(&tile, &extent);
            cases.push(Case {
                name: format!("zoom_{z}"),
                bbox,
                zoom: (u32::from(principal.d_star) + u32::from(z)).min(16) as u8,
                k: MAX_K,
            });
            cases.push(Case {
                name: format!("zoom_{z}_k30"),
                bbox,
                zoom: z,
                k: args.k_small,
            });
        }
        if !args.cases.is_empty() {
            cases.retain(|case| args.cases.iter().any(|name| name == &case.name));
            if cases.is_empty() {
                return Err(format!("--cases {:?} names no case", args.cases).into());
            }
        }

        let mut case_values: Vec<Value> = Vec::new();
        for case in &cases {
            let tiles = tiles_for_bbox(case.bbox, case.zoom, &extent);
            if tiles.len() > MAX_TILES_PER_REQUEST {
                case_values.push(json!({
                    "case": case.name,
                    "refused": "tile count above max_tiles_per_request",
                    "tiles": tiles.len(),
                }));
                continue;
            }
            let ranges = tile_ranges_all(segment, &tiles);
            // The rung this principal's one walk already produced — see [`LADDER_DEPTH`] for why
            // it is the same number the request path computes for `N_occ(case.zoom)`.
            let n_occ = principal.ladder[case.zoom as usize];
            let threshold = Threshold::at_depth(principal.visible_total, THETA_TARGET, n_occ);
            let params = SelectParams {
                k_min: K_MIN,
                cap: case.k.min(K_MAX_MARKS),
                threshold,
            };
            let inputs = Inputs {
                mask: &principal.mask,
                segment,
                bands: &bands,
                params: &params,
                tiles: &tiles,
                ranges: &ranges,
            };

            let mut arms = serde_json::Map::new();
            // `None` where the reference arm did not run: the band arm then makes no comparison
            // and records that it made none.
            let mut reference: Option<HashMap<u64, (u64, Vec<u64>)>> = run_r.then(HashMap::new);
            let mut served_rows: Vec<u32> = Vec::new();
            let mut case_meta = json!({
                "case": case.name,
                "zoom": case.zoom,
                "k": case.k,
                "cap": params.cap,
                "bbox": case.bbox,
                "tiles_requested": tiles.len(),
                "n_occ": n_occ,
                "threshold": match threshold {
                    Threshold::Cut(cut) => json!({"cut": cut, "j": cut.leading_zeros()}),
                    Threshold::Saturated => json!("saturated"),
                },
                "pixels_per_cell": pixels_per_cell(case.bbox, &extent),
            });

            for condition in &args.conditions {
                let cold = condition == "cold";

                // ---- R, the shipped path. Cold is one run after an eviction pass; hot is the
                // second of two, so the first has already faulted the pages in.
                if run_r {
                    if cold {
                        evict(&roots);
                    } else {
                        drop(engine.viewport(
                            &principal.session,
                            ViewportRequest::new(&view_id, case.zoom, case.bbox, case.k),
                        )?);
                    }
                    let before = Counters::now();
                    let out = engine.viewport(
                        &principal.session,
                        ViewportRequest::new(&view_id, case.zoom, case.bbox, case.k),
                    )?;
                    arms.insert(
                        format!("R.{condition}"),
                        merge(before.since(ticks), r_detail(&out)),
                    );
                    // The served identities per tile, split out of the flat points stream by each
                    // tile's own `served` — the only grouping the response carries
                    // (`TileCount::served`). Taken from the first condition's run; the request is
                    // the same one, so the second condition's answer is the same set.
                    let reference = reference.as_mut().expect("a map, since the arm ran");
                    if reference.is_empty() {
                        let mut at = 0usize;
                        let mut per_tile = String::new();
                        for tile in &out.tiles {
                            let take = tile.served as usize;
                            let ids = out.points.tessera_ids[at..at + take].to_vec();
                            at += take;
                            if args.per_tile.is_some() {
                                per_tile.push_str(
                                    &json!({
                                        "tile": tile.tile,
                                        "visible": tile.visible,
                                        "served": tile.served,
                                        "ids": ids,
                                    })
                                    .to_string(),
                                );
                                per_tile.push('\n');
                            }
                            reference.insert(tile.tile, (tile.served, ids));
                        }
                        if let Some(dir) = &args.per_tile {
                            std::fs::create_dir_all(dir)?;
                            std::fs::write(
                                dir.join(format!("{}.{}.ndjson", principal.name, case.name)),
                                per_tile,
                            )?;
                        }
                    }
                }

                // ---- B, the band route over the same mask, cut, ranges and parameters.
                if run_b {
                    if cold {
                        evict(&roots);
                    } else {
                        drop(band_route(&inputs, reference.as_ref(), band_options));
                    }
                    let before = Counters::now();
                    let outcome = band_route(&inputs, reference.as_ref(), band_options);
                    arms.insert(
                        format!("B.{condition}"),
                        merge(before.since(ticks), outcome.to_json()),
                    );
                    disagreeing_tiles += outcome.disagreements.len();
                    if served_rows.is_empty() {
                        served_rows.clone_from(&outcome.served_rows);
                    }
                }

                // ---- G, the render, over the rows B served.
                if run_g {
                    let codes = segment.morton.u32();
                    let residual = segment.columns.residual();
                    let starts = segment.cuts.starts();
                    let rows = &served_rows;
                    let run_columns = || {
                        let mut acc = 0u64;
                        for &row in rows {
                            acc ^= (u64::from(codes[row as usize]) << 32)
                                | u64::from(residual[row as usize]);
                        }
                        black_box(acc);
                    };
                    let run_cells = || {
                        let mut acc = 0u64;
                        for &row in rows {
                            let cell = starts.partition_point(|&s| s <= row) - 1;
                            acc ^= u64::from(bands.cell_code(cell));
                        }
                        black_box(acc);
                    };
                    if cold {
                        evict(&roots);
                    } else {
                        run_columns();
                    }
                    let before = Counters::now();
                    run_columns();
                    let columns_metrics = before.since(ticks);
                    if !cold {
                        run_cells();
                    }
                    let before = Counters::now();
                    run_cells();
                    let cells_metrics = before.since(ticks);

                    let cell_indices: Vec<u32> = rows
                        .iter()
                        .map(|&row| (starts.partition_point(|&s| s <= row) - 1) as u32)
                        .collect();
                    let morton_pages = pages_of(rows, 4);
                    let residual_pages = pages_of(rows, 4);
                    let cell_code_pages = pages_of(&cell_indices, 4);
                    let cut_pages = cut_index_pages(starts, rows);
                    arms.insert(
                        format!("G.{condition}"),
                        json!({
                            "rows": rows.len(),
                            "columns": merge(columns_metrics, json!({
                                "morton_pages": morton_pages,
                                "residual_pages": residual_pages,
                                "modelled_bytes": (morton_pages + residual_pages) as u64 * 4096,
                            })),
                            "cells": merge(cells_metrics, json!({
                                "cell_code_pages": cell_code_pages,
                                "cut_index_pages": cut_pages,
                                "modelled_bytes": (cell_code_pages + cut_pages) as u64 * 4096,
                            })),
                        }),
                    );
                }
            }
            case_meta["arms"] = Value::Object(arms);
            case_values.push(case_meta);
        }

        results.push(json!({
            "principal": principal.name,
            "terms": principal.terms,
            "visible_total": principal.visible_total,
            "occupancy_ladder": principal.ladder,
            "occupancy_cost": principal.ladder_cost,
            "d_star": principal.d_star,
            "projection_build": principal.projection,
            "cases": case_values,
        }));
    }

    let report = json!({
        "bundle": args.bundle,
        "bands": args.bands,
        "bands_json": bands.bands_json,
        "budget": args.budget,
        "k_small": args.k_small,
        "conditions": args.conditions,
        "arms": args.arms,
        "cases_filter": args.cases,
        "fp16_measured": args.fp16,
        "code_check": !args.no_code_check,
        "madv_random": madv,
        "engine_config": {
            "max_k": MAX_K,
            "k_min": K_MIN,
            "k_max_marks": K_MAX_MARKS,
            "theta_target_marks": THETA_TARGET,
            "max_tiles_per_request": MAX_TILES_PER_REQUEST,
            "compute_threads": tessera_engine::default_compute_threads(),
        },
        "view": view_id,
        "row_count": segment.row_count,
        "cell_count": segment.cuts.len(),
        "locations": locations
            .iter()
            .map(|(z, tile, visible)| json!({"z": z, "tile": tile.prefix, "visible_widest": visible}))
            .collect::<Vec<_>>(),
        "widest_principal": principals[widest].name,
        "commit": std::env::var("TESSERA_PROBE_COMMIT").unwrap_or_default(),
        "box": std::env::var("TESSERA_PROBE_BOX").unwrap_or_default(),
        "disagreeing_tiles": disagreeing_tiles,
        "principals": results,
    });
    std::fs::write(
        &args.out,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    if run_r && run_b {
        eprintln!(
            "wrote {} — {} tile(s) disagreed between the band route and the reference",
            args.out.display(),
            disagreeing_tiles
        );
    } else {
        eprintln!(
            "wrote {} — arms {}, so no equality comparison was made",
            args.out.display(),
            args.arms.join(",")
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// Two JSON objects as one.
fn merge(a: Value, b: Value) -> Value {
    let mut out = match a {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    if let Value::Object(map) = b {
        for (key, value) in map {
            out.insert(key, value);
        }
    }
    Value::Object(out)
}

/// The reference arm's own figures: what it returned and what its stages cost.
fn r_detail(out: &tessera_engine::ViewportOut) -> Value {
    let t = &out.timings;
    json!({
        "points": out.points.len(),
        "tiles_nonempty": out.tiles.len(),
        "sigma_visible": t.sigma_visible,
        "stage_timings": {
            "enabled": t.enabled,
            "total_ns": t.total_ns,
            "row_projection_ns": t.row_projection_ns,
            "compose_ns": t.compose_ns,
            "theta_anchor_ns": t.theta_anchor_ns,
            "theta_occupancy_ns": t.theta_occupancy_ns,
            "tiles_for_bbox_ns": t.tiles_for_bbox_ns,
            "tile_ranges_ns": t.tile_ranges_ns,
            "count_ns": t.count_ns,
            "select_ns": t.select_ns,
            "gather_ns": t.gather_ns,
            "select_rows_visited": t.select_rows_visited,
            "points_gathered": t.points_gathered,
            "rows_in_ranges": t.rows_in_ranges,
        },
    })
}
