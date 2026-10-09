//! **What the member files cost a bundle that already has its columns**: the writer's time, the
//! stored bytes and the covering index in memory, per row-major column.
//!
//! Reads label or list columns straight from a prefix (`row-column/*.tslb`, `*.tsll`), whose format
//! does not move with the bundle's, so a bundle built at the previous format can be measured without
//! rebuilding it. Each column is handed to `mosaica_store::derived::project_row_members`, the one
//! writer the build and the fold call, and its file is opened and indexed as the engine opens it.
//!
//! ```text
//! row_members_size --scratch <dir> <column file>...
//! ```
//!
//! The scratch directory takes the writer's partition, 8 B a member entry, and the member file;
//! both are removed once each column is measured. Run a large bundle under a memory cap.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use mosaica_store::derived::{project_row_members, ColumnLabels};
use mosaica_store::membership::{LabelColumnPack, ListColumnPack};
use mosaica_store::row_members::RowMembersPack;

#[derive(Parser)]
struct Args {
    /// Where the writer's partition and the member file go.
    #[arg(long)]
    scratch: PathBuf,
    /// Row-major column files: `.tslb` for the label form, `.tsll` for the list form.
    columns: Vec<PathBuf>,
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 20) as f64
}

fn main() {
    let args = Args::parse();
    std::fs::create_dir_all(&args.scratch).expect("the scratch directory");
    for column in &args.columns {
        let list = column.extension().is_some_and(|e| e == "tsll");
        let label_pack;
        let list_pack;
        let labels = if list {
            list_pack = ListColumnPack::open(column).expect("a list column");
            ColumnLabels::List(&list_pack)
        } else {
            label_pack = LabelColumnPack::open(column).expect("a label column");
            ColumnLabels::Label(&label_pack)
        };
        let started = Instant::now();
        let path = project_row_members(labels, &args.scratch).expect("the writer runs");
        let write_secs = started.elapsed().as_secs_f64();
        let pack = RowMembersPack::open(&path).expect("the member file opens");
        let ordinals = pack.ordinals();
        let ranges = pack.total_ranges();
        let members: u64 = (0..ordinals).map(|o| u64::from(pack.member_count(o))).sum();
        let file_len = pack.file_len() as u64;
        let payload = pack.payload_len() as u64;
        let mut containers = [0u64; 3];
        for ordinal in 0..ordinals {
            if let Some(view) = pack.members(ordinal) {
                let s = view.statistics();
                containers[0] += u64::from(s.n_array_containers);
                containers[1] += u64::from(s.n_run_containers);
                containers[2] += u64::from(s.n_bitset_containers);
            }
        }
        let held = mosaica_engine::row_members::LevelMembers::new(pack);
        let started = Instant::now();
        let probe = held.overlapping(0..=0);
        let index_secs = started.elapsed().as_secs_f64();
        // The index holds each range once as three `u32`s: its ends and its artifact.
        let index_bytes = ranges * 12;
        println!(
            "{}: {ordinals} artifacts, {members} members, rows {}; writer {write_secs:.1} s; file \
             {:.1} MiB = bitmaps {:.1} MiB ({:.2} B a member; containers: array {} run {} bitset {}) + \
             coverings {ranges} ranges {:.1} MiB + tables {:.1} MiB; index built in {index_secs:.2} s, \
             {:.1} MiB in memory ({} at row 0)",
            column.display(),
            held.base_rows(),
            mib(file_len),
            mib(payload),
            payload as f64 / members.max(1) as f64,
            containers[0],
            containers[1],
            containers[2],
            mib(ranges * 8),
            mib(file_len - payload - ranges * 8),
            mib(index_bytes),
            probe.cardinality(),
        );
        drop(held);
        let _ = std::fs::remove_file(&path);
    }
}
