//! Build the identity-band structures for one segment, so the probe beside this can price them.
//!
//! A whole-map viewport needs, per tile, how many visible rows carry a `tessera_id` below the
//! depth's cut and which the smallest of them are (architecture §7.2). Every shipped route reads
//! the 8 B/row identity column to answer it. The structures written here are the candidates for
//! answering it without that read, and the measurement they exist for is
//! `probes/2026-09-14-identity-bands/`. **Nothing here ships**: this is a probe input, written
//! beside a bundle and never into one, and no bundle format knows about these files.
//!
//! One sequential pass over `tessera_id` and `morton.u32` in lockstep, through the store's own
//! readers, writing:
//!
//! | file | content |
//! |---|---|
//! | `lz.u8` | one byte a row, `tessera_id.leading_zeros()` (0..=64) |
//! | `fp16.u16` | two bytes a row, the quantised identity prefix — see [`fp16_of`] |
//! | `top-J.bin` | `(row: u32, id: u64, code: u32)`, 16 B, for every row with `lz >= J`, row order |
//! | `cell-codes.u32` | one `u32` a cell, `morton[cuts[i]]` |
//! | `bands.json` | the row and cell counts, the 65-bin `lz` histogram, each file's measured size against its model, wall seconds and bytes read |
//!
//! **Memory is O(1) beyond the mappings and the writers.** Nothing holds a column: the three
//! inputs are read through their mmapped slices and each output goes straight into a
//! `BufWriter`, so the resident set is the histogram, seven buffers and whatever page cache the
//! kernel keeps for the pass. It is meant to run under
//! `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G` at any rung.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin identity_bands_build -- \
//!     --segment <bundle>/v00000/partitions/default/views/geo/segments/seg-0 --out <dir>
//! ```

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde_json::json;

use tessera_store::read::{ColumnsRef, CutIndex, MortonSlice};

/// The bands that get a `(row, id, code)` list. Powers of two in the identity space: band `J`
/// holds the rows whose identity is below `2^(64-J)`, about `2^-J` of a corpus whose identities
/// are uniform, so the four here span 1/16 to 1/1024 of the rows.
const LISTS: [u32; 4] = [4, 6, 8, 10];

/// `(row: u32, id: u64, code: u32)`, little-endian, packed.
const ENTRY_BYTES: u64 = 16;

#[derive(Parser)]
#[command(about = "Write one segment's identity bands, for the identity-bands probe")]
struct Args {
    /// The segment directory: `columns.arrow`, `morton.u32` and `cuts.u32` beside each other.
    #[arg(long)]
    segment: PathBuf,
    /// Where to write the band files. Created if absent.
    #[arg(long)]
    out: PathBuf,
}

/// The quantised identity prefix: `min(lz, 63)` in the top six bits, the ten bits after the
/// leading one bit in the low ten. `0` maps to `63 << 10`.
///
/// **This is not monotone as a `u16` and the comparison that uses it must not treat it as one.**
/// A larger `lz` is a smaller identity while a larger `frac` is a larger one, so two identities
/// are ordered by `lz` descending and then `frac` ascending — which is what the probe's
/// `fp16_cmp` does. Equal values mean the two identities share their leading `lz + 11` bits and
/// nothing shorter than a full read separates them.
fn fp16_of(id: u64) -> u16 {
    let lz = id.leading_zeros();
    if lz >= 64 {
        // No leading one bit to take ten bits after. The smallest identity there is, and it
        // shares its encoding with 1, which is the next smallest.
        return 63 << 10;
    }
    // `lz + 1 <= 63`, so the shift is in range; at `lz == 63` the identity is 1 and there is
    // nothing below the leading bit.
    let frac = if lz >= 63 {
        0
    } else {
        ((id << (lz + 1)) >> 54) as u16
    };
    ((lz.min(63) as u16) << 10) | frac
}

/// `read_bytes` from `/proc/self/io` — what this process has taken from the block layer.
fn read_bytes() -> u64 {
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

fn create(path: &PathBuf) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(1 << 20, File::create(path)?))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;

    let columns = ColumnsRef::load(&args.segment.join("columns.arrow"))?;
    let morton = MortonSlice::load(&args.segment.join("morton.u32"))?;
    let row_count = columns.row_count();
    let cuts = CutIndex::load(&args.segment.join(CutIndex::FILE), row_count)?;
    if morton.len() != row_count as usize {
        return Err(format!(
            "morton.u32 holds {} codes and columns.arrow {row_count} rows",
            morton.len()
        )
        .into());
    }
    // The pass reads both columns front to back once and never returns to a page, which is what
    // this advice says. It is a hint: a kernel that ignores it leaves the pass correct.
    columns.advise_sequential();
    morton.advise_sequential();

    let started = Instant::now();
    let read_before = read_bytes();

    let mut lz_out = create(&args.out.join("lz.u8"))?;
    let mut fp16_out = create(&args.out.join("fp16.u16"))?;
    let mut lists: Vec<(u32, BufWriter<File>)> = LISTS
        .iter()
        .map(|&j| Ok::<_, std::io::Error>((j, create(&args.out.join(format!("top-{j}.bin")))?)))
        .collect::<Result<_, _>>()?;

    let mut histogram = [0u64; 65];
    let ids = columns.tessera_id();
    let codes = morton.u32();
    for row in 0..row_count as usize {
        let id = ids[row];
        let code = codes[row];
        let lz = id.leading_zeros();
        histogram[lz as usize] += 1;
        lz_out.write_all(&[lz as u8])?;
        fp16_out.write_all(&fp16_of(id).to_le_bytes())?;
        for (j, out) in lists.iter_mut() {
            if lz >= *j {
                let mut entry = [0u8; ENTRY_BYTES as usize];
                entry[0..4].copy_from_slice(&(row as u32).to_le_bytes());
                entry[4..12].copy_from_slice(&id.to_le_bytes());
                entry[12..16].copy_from_slice(&code.to_le_bytes());
                out.write_all(&entry)?;
            }
        }
    }

    // The cell's code is every row of the cell's code, so the first row's is the cell's — which
    // is why `cuts.u32` stores no code of its own (contracts §2.6).
    let mut cells_out = create(&args.out.join("cell-codes.u32"))?;
    for &start in cuts.starts() {
        cells_out.write_all(&codes[start as usize].to_le_bytes())?;
    }

    lz_out.flush()?;
    fp16_out.flush()?;
    cells_out.flush()?;
    for (_, out) in lists.iter_mut() {
        out.flush()?;
    }
    drop(lz_out);
    drop(fp16_out);
    drop(cells_out);
    drop(lists);

    let wall_s = started.elapsed().as_secs_f64();
    let read_delta = read_bytes().saturating_sub(read_before);

    let n = u64::from(row_count);
    let cells = cuts.len() as u64;
    let mut files = Vec::new();
    let mut push =
        |name: String, model: u64, note: &str| -> Result<(), Box<dyn std::error::Error>> {
            let bytes = std::fs::metadata(args.out.join(&name))?.len();
            files.push(json!({
                "file": name,
                "bytes": bytes,
                "model_bytes": model,
                "model_note": note,
                "bytes_over_model": if model == 0 { 0.0 } else { bytes as f64 / model as f64 },
            }));
            Ok(())
        };
    push(
        "lz.u8".to_string(),
        (3 * n).div_ceil(8),
        "ceil(3n/8), the three-bit packed form; the file is one byte a row for random access",
    )?;
    push("fp16.u16".to_string(), 2 * n, "2n")?;
    for &j in &LISTS {
        push(
            format!("top-{j}.bin"),
            ENTRY_BYTES * (n >> j),
            "16·n/2^J, the expected entry count under a uniform identity",
        )?;
    }
    push("cell-codes.u32".to_string(), 4 * cells, "4·cells")?;

    let report = json!({
        "segment": args.segment,
        "out": args.out,
        "row_count": n,
        "cell_count": cells,
        "lz_histogram": histogram.to_vec(),
        "files": files,
        "wall_s": wall_s,
        "read_bytes": read_delta,
    });
    let text = serde_json::to_string_pretty(&report)?;
    std::fs::write(args.out.join("bands.json"), format!("{text}\n"))?;
    println!("{text}");
    Ok(())
}
