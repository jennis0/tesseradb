//! Commit-window properties: what a window must be true of however the submissions that fill it
//! are shaped, and the compression figure the window exists to buy.
//!
//! The engine-level cases (one fsync per window, each waiter's own ids, the partially-acked window)
//! live in `tessera-engine/tests/write.rs`, because they are statements about the executor. These
//! are statements about the allocation, which is entity-space and needs no `Engine`, no `Bundle` and
//! no `TempDir` — that separation is why `CommitWindow` lives in this crate at all.

use std::collections::HashSet;

use proptest::prelude::*;

use tessera_lifecycle::alloc::Allocator;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalRecord;
use tessera_lifecycle::window::{CommitWindow, FragmentationTally, WindowEntry};
use tessera_types::TermId;

fn row(key: &str, terms: &[u32]) -> UnallocatedRow {
    UnallocatedRow {
        external_id: Some(key.as_bytes().to_vec()),
        view: "default".to_string(),
        join: None,
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: Vec::new(),
        terms: terms.iter().map(|t| TermId::new(*t)).collect(),
        scoped: Vec::new(),
    }
}

/// The ids in a closed entry's framed WAL record — "in the WAL" for the property below, one step
/// short of the append itself, which the engine performs on exactly this record.
fn framed_ids(record: &WalRecord) -> Vec<u64> {
    match record {
        WalRecord::IngestBatch { rows, .. } => rows.iter().map(|r| r.entity_id.raw()).collect(),
        _ => panic!("a window frames IngestBatch records only"),
    }
}

proptest! {
    /// **The three id properties, across a sequence of windows on one allocator.**
    ///
    /// Strictly monotone across windows; no id issued twice; and — the one that would otherwise be
    /// invisible — **no id issued that is not in a record**. An implementation that allocated per
    /// window and then framed a subset (or framed the rows of one entry against another entry's
    /// ids) satisfies the first two and fails the third; replay would then either re-derive the
    /// missing rows' placement or lose them, which is what the WAL carrying allocated ids exists to
    /// prevent (lifecycle §5.1, SA §6.2).
    #[test]
    fn ids_are_monotone_unique_and_all_framed(
        windows in prop::collection::vec(
            prop::collection::vec(prop::collection::vec(0u32..6, 0..4), 1..8),
            1..8,
        ),
    ) {
        let mut alloc = Allocator::new(17);
        let mut seen: HashSet<u64> = HashSet::new();
        let mut previous_max: Option<u64> = None;

        for (w, entries) in windows.iter().enumerate() {
            let mut window: CommitWindow<()> = CommitWindow::new(w as u64);
            for (e, entry_rows) in entries.iter().enumerate() {
                // One term per row: the id properties below do not depend on signature *shape*,
                // which `assign_sorted_groups_identical_signatures_contiguously` covers already.
                let rows: Vec<UnallocatedRow> = entry_rows
                    .iter()
                    .enumerate()
                    .map(|(i, t)| row(&format!("w{w}-e{e}-r{i}"), &[*t]))
                    .collect();
                window.push(WindowEntry {
                    rows,
                    batch_id: format!("w{w}-e{e}"),
                    body_hash: [0u8; 32],
                    memberships: Vec::new(),
                edges: Vec::new(),
                    waiters: vec![()],
                });
            }
            let rows_in_window = window.rows();
            let (closed, _) = window.allocate(&mut alloc).expect("the id space is not exhausted");

            let mut issued: Vec<u64> = Vec::new();
            for entry in &closed {
                // The acked ids and the framed ids are the same set, entry by entry: a window that
                // returned the right multiset against the wrong record is a silent misattribution.
                prop_assert_eq!(
                    entry.entity_ids.iter().map(|i| i.raw()).collect::<Vec<_>>(),
                    framed_ids(&entry.record)
                );
                issued.extend(framed_ids(&entry.record));
            }

            prop_assert_eq!(issued.len(), rows_in_window, "one id per row, no more and no fewer");
            for id in &issued {
                prop_assert!(seen.insert(*id), "id {} issued twice (I9)", id);
                if let Some(prev) = previous_max {
                    prop_assert!(*id > prev, "id {} is not above the previous window's max {}", id, prev);
                }
            }
            previous_max = issued.iter().copied().max().or(previous_max);
            prop_assert_eq!(alloc.high_water(), 17 + seen.len() as u64);
        }
    }

    /// **The sort scope is the window.** However the same rows are chunked into submissions, the
    /// assignment is the assignment of one submission carrying all of them.
    ///
    /// This is the headline property at unit scope: if it does not hold, group commit is decoration.
    #[test]
    fn the_assignment_does_not_depend_on_how_the_rows_were_chunked(
        sigs in prop::collection::vec(prop::collection::vec(0u32..5, 0..3), 1..40),
        chunk in 1usize..7,
    ) {
        let rows: Vec<UnallocatedRow> = sigs
            .iter()
            .enumerate()
            .map(|(i, sig)| row(&format!("r{i:04}"), sig))
            .collect();

        let mut chunked: CommitWindow<()> = CommitWindow::new(0);
        for (c, part) in rows.chunks(chunk).enumerate() {
            chunked.push(WindowEntry {
                rows: part.to_vec(),
                batch_id: format!("c{c}"),
                body_hash: [0u8; 32],
                memberships: Vec::new(),
                edges: Vec::new(),
                waiters: vec![()],
            });
        }
        let chunked_ids: Vec<u64> = chunked
            .allocate(&mut Allocator::new(9))
            .unwrap()
            .0
            .iter()
            .flat_map(framed_ids_of)
            .collect();

        let mut whole: CommitWindow<()> = CommitWindow::new(0);
        whole.push(WindowEntry {
            rows,
            batch_id: "one".into(),
            body_hash: [0u8; 32],
            memberships: Vec::new(),
                edges: Vec::new(),
            waiters: vec![()],
        });
        let whole_ids: Vec<u64> = whole
            .allocate(&mut Allocator::new(9))
            .unwrap()
            .0
            .iter()
            .flat_map(framed_ids_of)
            .collect();

        prop_assert_eq!(chunked_ids, whole_ids);
    }
}

fn framed_ids_of<W>(entry: &tessera_lifecycle::window::ClosedEntry<W>) -> Vec<u64> {
    framed_ids(&entry.record)
}

/// **The compression figure, reported against the full-sort ceiling.**
///
/// Posting *runs* in entity space, for one corpus assigned three ways: request-scoped (what a
/// window-less executor gets), window-scoped, and one full-corpus sort (the ceiling the probes'
/// 8.9–36.7× was measured under). The quantity is `postings / runs` per term, averaged over terms —
/// higher is better, `1.0` is fully scattered.
///
/// It is arithmetic over the assignment, not a measurement of the engine: no I/O, no timing, and no
/// claim about latency. It exists so the sort scope is *pinned* — a later change that quietly
/// narrows it shows up here.
///
/// **What this corpus cannot tell you.** Every row carries exactly one term, so an
/// item's signature **is** its term and `run = chunk × density` holds by construction. That makes
/// the ordering property below robust and the *magnitudes* meaningless as a forecast: on a real
/// corpus `assign_sorted` sorts on the whole term signature, so a term's postings split across every
/// signature carrying it and the runs are far shorter (module docs of `tessera_lifecycle::window`
/// have the measured distribution). For the same reason the "window achieves X% of the ceiling"
/// printed below is `chunk / corpus` — 2 000/4 000 — and carries no information about a deployment.
#[test]
fn the_window_run_ratio_against_the_full_sort_ceiling() {
    // 4 000 rows, 20 terms, each row carrying one term: a 5% density per term, in the region the
    // corpus's 2% sits beside. Deterministic, no rng.
    const ROWS: usize = 4_000;
    const TERMS: u32 = 20;
    let corpus: Vec<UnallocatedRow> = (0..ROWS)
        .map(|i| row(&format!("r{i:05}"), &[(i as u32 * 7) % TERMS]))
        .collect();

    let assign = |chunk: usize| -> f64 {
        let mut alloc = Allocator::new(0);
        let mut postings: Vec<Vec<u64>> = vec![Vec::new(); TERMS as usize];
        for group in corpus.chunks(chunk) {
            // One window per `chunk` rows; the window's scope is the only thing that differs
            // between the three arms.
            let mut window: CommitWindow<()> = CommitWindow::new(0);
            window.push(WindowEntry {
                rows: group.to_vec(),
                batch_id: "b".into(),
                body_hash: [0u8; 32],
                memberships: Vec::new(),
                edges: Vec::new(),
                waiters: vec![()],
            });
            for entry in window.allocate(&mut alloc).unwrap().0 {
                for (row, id) in group.iter().zip(entry.entity_ids.iter()) {
                    postings[row.terms[0].raw() as usize].push(id.raw());
                }
            }
        }
        // Runs: consecutive ids in one term's (sorted) posting list.
        let mut ratios = Vec::new();
        for list in postings.iter_mut() {
            if list.is_empty() {
                continue;
            }
            list.sort_unstable();
            let runs = 1 + list.windows(2).filter(|w| w[1] != w[0] + 1).count();
            ratios.push(list.len() as f64 / runs as f64);
        }
        ratios.iter().sum::<f64>() / ratios.len() as f64
    };

    let request_scoped = assign(100); // a client posting 100 rows at a time
    let window_scoped = assign(2_000); // two windows over the same corpus
    let full_sort = assign(ROWS); // the ceiling

    // The ordering is the property; the numbers are printed for the record and asserted only
    // loosely, since they are a function of the constructed corpus rather than of the corpus.
    println!(
        "run ratio (postings per run, mean over terms): request-scoped {request_scoped:.1}, \
         window-scoped {window_scoped:.1}, full-sort ceiling {full_sort:.1}. The last ratio is \
         {:.0}% — which is the ratio of the two SORT SCOPES (2000/4000) and not a result: in a \
         one-term-per-row corpus run length is chunk x density by construction.",
        100.0 * window_scoped / full_sort
    );
    assert!(
        window_scoped > request_scoped * 4.0,
        "a window must collect materially more run length than request scope: {window_scoped} vs \
         {request_scoped}"
    );
    assert!(
        window_scoped < full_sort,
        "and it must stay openly below the full-sort ceiling: {window_scoped} vs {full_sort}"
    );
}

/// **The figure `/control/status` publishes, over one corpus assigned at three scopes.**
///
/// This is what the fragmentation emitter exists for: group commit disabled, group commit at a
/// window, and one full-corpus sort — the ceiling the probes' 8.9–36.7× posting compression was
/// measured under. The quantity is the emitted one (`Σbaseline / Σruns` over
/// `FragmentationTally`), not a second implementation of it in the test, so a change to the
/// emitter's arithmetic shows up here.
///
/// **The corpus carries two terms per row and `k < W`, and that is load-bearing.** With one term
/// per row a term's `k` equals its window's `W`, where the baseline `k·(W − k + 1)/W` is `1` and a
/// perfect run is `1` — so every arm reports exactly `1.0` and every assertion below passes while
/// measuring nothing. `the_window_run_ratio_against_the_full_sort_ceiling` above uses exactly that
/// one-term corpus for a different purpose and says so.
///
/// **Its numbers are not comparable with that test's.** This is the pooled, posting-weighted
/// estimator the endpoint publishes; that one is an unweighted mean over terms of `postings/runs`.
///
/// And the ratio at `chunk = 1` is `1.0` **identically**, for every corpus — see
/// `FragmentationTally`'s doc. That is the correct reading of "group commit disabled collects
/// nothing", not a measurement, and the assertion below says so rather than treating it as one.
#[test]
fn the_emitted_run_ratio_rises_with_the_window_and_stays_under_the_full_sort_ceiling() {
    const ROWS: usize = 4_000;
    const SIGNATURES: u32 = 20;

    // Two terms per row drawn from a small signature vocabulary: a term's postings therefore split
    // across every signature carrying it, exactly as they do on a real corpus, and no term reaches
    // k = W in any window.
    let corpus: Vec<UnallocatedRow> = (0..ROWS)
        .map(|i| {
            let s = (i as u32 * 7) % SIGNATURES;
            row(&format!("r{i:05}"), &[s, (s + 1) % SIGNATURES])
        })
        .collect();

    let ratio_at = |chunk: usize| -> (f64, u64, u64) {
        let mut alloc = Allocator::new(0);
        let mut total = FragmentationTally::default();
        for group in corpus.chunks(chunk) {
            let mut window: CommitWindow<()> = CommitWindow::new(0);
            window.push(WindowEntry {
                rows: group.to_vec(),
                batch_id: "b".into(),
                body_hash: [0u8; 32],
                memberships: Vec::new(),
                edges: Vec::new(),
                waiters: vec![()],
            });
            let (_, tally) = window.allocate(&mut alloc).unwrap();
            total.merge(tally);
        }
        (
            total.baseline_runs_milli as f64 / 1000.0 / total.runs as f64,
            total.postings,
            total.runs,
        )
    };

    let (disabled, _, disabled_runs) = ratio_at(1); // commit_window_max_items = 1
    let (windowed, _, windowed_runs) = ratio_at(500);
    let (ceiling, _, ceiling_runs) = ratio_at(ROWS); // one full-corpus sort

    println!(
        "emitted run_ratio (Sum baseline / Sum runs): grouping disabled {disabled:.2} \
         ({disabled_runs} runs), 500-row window {windowed:.2} ({windowed_runs} runs), \
         full-sort ceiling {ceiling:.2} ({ceiling_runs} runs). The window reaches \
         {:.0}% of the ceiling. Both figures are WITHIN-WINDOW sort quality against a \
         within-window random baseline (see FragmentationTally); neither is a stream-scope \
         fragmentation figure and neither is comparable with the probes' full-corpus numbers.",
        100.0 * windowed / ceiling
    );

    assert_eq!(
        disabled, 1.0,
        "a one-row window has baseline 1 and one run, for every corpus — identically 1.0, which \
         is the reading 'group commit disabled collects nothing', not a measurement"
    );
    assert!(
        windowed > disabled * 4.0,
        "a window must collect materially more run length than no grouping at all: {windowed} vs \
         {disabled}"
    );
    assert!(
        windowed < ceiling,
        "and must stay openly below the full-sort ceiling: {windowed} vs {ceiling}"
    );
    assert!(
        windowed_runs < disabled_runs,
        "the raw run count is the un-normalised half of the same statement and must move with it: \
         {windowed_runs} vs {disabled_runs}"
    );
}
