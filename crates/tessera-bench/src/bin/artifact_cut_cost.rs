//! **What does the cut cost per request, and is it a fraction of the walk it rides on?**
//!
//! The cut derives a treed level's child direction from its parent pointers on **every request**,
//! rather than holding a cached index. That is the deliberate choice — a cache costs an
//! invalidation rule that has to be right whenever the generation moves, and the alternative to it
//! is not "no work" but "work proportional to something the serving path already walks". The
//! serving path tests every artifact of the level against its criterion, so the claim being made is
//! that reading the same level's parent pointers is a fraction of that loop rather than a multiple.
//!
//! **This measures the claim.** Two arms, both timed:
//!
//! - **flat** — a level with no edges at all, which is every layer published before Stage 5. It is
//!   the arm that matters most, because a flat layer must not pay for a mechanism it does not use,
//!   and the whole cost here is one pass reading a field and finding nothing.
//! - **treed** — a balanced tree over the level, with a share of its artifacts passing the
//!   criterion, at several budgets.
//!
//! Reported per scale: the lineage build, the unbudgeted cut, and a budgeted cut (which bisects
//! over depths and so evaluates the plan several times). Against these, the figure to compare is
//! the per-artifact candidacy test the serving loop already performs — a Roaring `and_cardinality`
//! against the viewer's mask, which `annotation-representation.md` §2 prices in the microseconds
//! per artifact range at realistic membership sizes. **A lineage pass in the nanoseconds per
//! artifact range is therefore the answer this probe is looking for**; anything approaching the
//! candidacy test's own cost would say the derivation wants caching after all.
//!
//! # Measured 2026-08-18, WSL2 on this host, release
//!
//! | level | arm | lineage | cut, no budget | cut, budgeted | ns/artifact |
//! |---:|---|---:|---:|---:|---:|
//! | 10³ | flat | 0.000 ms | 0.003 ms | 0.002 ms | 0.4 |
//! | 10³ | treed | 0.001 ms | 0.060 ms | 0.059 ms | 1.4 |
//! | 10⁴ | flat | 0.003 ms | 0.027 ms | 0.026 ms | 0.3 |
//! | 10⁴ | treed | 0.046 ms | 0.620 ms | 0.678 ms | 4.6 |
//! | 10⁵ | flat | 0.037 ms | 0.252 ms | 0.250 ms | 0.4 |
//! | 10⁵ | treed | 0.426 ms | 9.508 ms | 7.975 ms | 4.3 |
//! | 10⁶ | flat | 1.177 ms | 3.725 ms | 3.066 ms | 1.2 |
//! | 10⁶ | treed | 5.634 ms | 100.5 ms | 110.9 ms | 5.6 |
//!
//! # Extended to 10⁷ 2026-08-21, and the top row is the operating point after all
//!
//! | level | arm | lineage | cut, no budget | cut, budgeted | ns/artifact |
//! |---:|---|---:|---:|---:|---:|
//! | 10⁶ | flat | 0.554 ms | 3.053 ms | 2.810 ms | 0.6 |
//! | 10⁶ | treed | 4.563 ms | 80.13 ms | 86.56 ms | 4.6 |
//! | **10⁷** | flat | 4.969 ms | 32.64 ms | 35.98 ms | 0.5 |
//! | **10⁷** | **treed** | 44.90 ms | **1 063 ms** | **1 008 ms** | 4.5 |
//!
//! The line above saying 10⁶ "is not an operating point this system serves" was written before the
//! serving pass was measured, and it is wrong in one case: **a principal who can see the whole
//! corpus passes every artifact**, so at a 10⁷-artifact treed layer and whole-map zoom the cut is
//! handed the entire population and costs **a second**. That is now the largest single term in such
//! a request — larger than the verdict pass it rides on, which
//! [the serving-scale probe](../../../../probes/2026-08-20-artifact-serving-scale/README.md) brings
//! to ~135 ms with the same population.
//!
//! It does not contradict the ~100 ns per *passing* artifact rule; it is that rule at a passing
//! count nothing had measured. Every narrower principal stays cheap for the same reason: at a mask
//! admitting 9.4% of the corpus, ~94 000 artifacts pass and the cut is ~10 ms.
//!
//! # And then the plan was rewritten again — 2026-08-21
//!
//! | level | arm | lineage | cut, no budget | cut, budgeted |
//! |---:|---|---:|---:|---:|
//! | 10⁶ | flat | 0.581 ms | 2.684 ms | 2.641 ms |
//! | 10⁶ | treed | 8.117 ms | 17.03 ms | 12.01 ms |
//! | **10⁷** | flat | 6.051 ms | 28.27 ms | 28.23 ms |
//! | **10⁷** | **treed** | **87.0 ms** | **213.7 ms** | **188.0 ms** |
//!
//! **1 008 ms to 188 ms on the budgeted path, and peak RSS from 1 078 MB to 470 MB.** Four changes,
//! and the first two are where almost all of it is:
//!
//! - **The plan holds one depth interval per servable node, not a lineage per frontier node.** Each
//!   node is the pick over one contiguous range of depths and never again, so the union across the
//!   lineages sharing it is an interval. That replaced 67 million `(depth, ordinal)` entries — half
//!   a gigabyte, to answer for a few thousand — with three arrays, and it made the budget search
//!   free: every depth's served count falls out of a difference array, so the bisection evaluates
//!   counts rather than building a cut per candidate depth.
//! - **Before that, flattening the per-frontier-node `Vec`s into one buffer** was worth 975 → 440 ms
//!   on its own. Four and a half million separate allocations, each growing through several
//!   reallocations to hold a dozen entries.
//! - **`on_chain` is `climbed ∪ passing`**, so the second climb over the spine went away: every
//!   passing node is an ancestor-or-self of a head, and every ancestor of a head is an ancestor of
//!   a passing node.
//! - **The passing set is borrowed where it already arrives ascending**, which the serving path
//!   always produces, rather than copied and sorted.
//!
//! **Depth moved to `Lineage`**, where the remaining ~40 ms of the lineage column now sits: depth is
//! a property of the tree and not of the viewer, so a request that recomputes it is redoing
//! generation work. That is why the lineage column rose as the cut column fell — the work did not
//! grow, it moved to the object that should be held per generation rather than rebuilt per request.
//!
//! **The lineage build is 0.3–5.6 ns per artifact of the level** — memory-bandwidth work against a
//! per-artifact candidacy test that does Roaring arithmetic, so the derivation is comfortably a
//! fraction of the loop it rides on and the cache it replaces would not have paid for itself.
//!
//! **The cut proper is ~100 ns per *passing* artifact**, and that is the figure to watch, because
//! it scales with what the viewer can see rather than with the level. At 10⁴ passing — already
//! more than a client can draw, and past where `annotation-representation.md` §2.0.0 says a
//! request is heading for its artifact ceiling anyway — it is 0.6 ms. At 10⁶ it is 100 ms, which
//! is not an operating point this system serves.
//!
//! **These are the figures after the plan was rewritten**, and the rewrite was worth about 8–10×:
//! the first shape allocated a lineage vector per passing node and answered *is this passing* and
//! *what is its depth* through a tree, measuring 5.4 ms at 10⁴ and 1 070 ms at 10⁶. Ordinal-indexed
//! side tables, a frontier pass that stops climbing at the first node an earlier walk covered, and
//! a memoised depth table are the whole difference.
//!
//! Run: `cargo run --release -p tessera-bench --bin artifact_cut_cost`

use std::time::Instant;

use tessera_engine::cut::{cut, Lineage};

/// Children per internal node in the treed arm — three, so the arithmetic never agrees with a bit
/// shift by accident and the tree gets genuinely deep at scale.
const BRANCH: u32 = 3;

/// Measured on the frontier, which is the more expensive of the two modes: pruning has to work out
/// which passing nodes are covered before it can map any of them, where serving the whole tree maps
/// every one directly. A figure taken unpruned would understate the cost of the mode that does more.
const PRUNE: bool = true;

fn main() {
    println!("# artifact cut cost\n");
    println!(
        "{:>10}  {:>8}  {:>12}  {:>12}  {:>12}  {:>12}",
        "level", "arm", "lineage", "cut(none)", "cut(budget)", "ns/artifact"
    );

    for &n in &[1_000u32, 10_000, 100_000, 1_000_000, 10_000_000] {
        flat_arm(n);
        treed_arm(n);
    }

    // **What a request actually hands the cut.** The arms above pass two thirds of the level, which
    // is the whole-corpus principal at whole-map zoom — the worst case and not the common one. A
    // viewport narrows the passing set long before the cut sees it, so the question this table
    // answers is whether the cut's cost follows the *passing* count or the level's.
    println!("\n# a 10^7 level, by how much of it passes\n");
    println!(
        "{:>12}  {:>10}  {:>12}  {:>12}",
        "passing", "lineage", "cut(none)", "cut(budget)"
    );
    for &passing in &[2_837u32, 44_166, 705_662, 6_666_666] {
        passing_arm(10_000_000, passing);
    }

    println!(
        "\nlineage = building the level's parent map; cut = resolving the frontier and, where a \n\
         budget is given, bisecting over depths. ns/artifact is the lineage build over the level \n\
         size — the quantity that has to stay far below the per-artifact candidacy test the \n\
         serving loop already pays."
    );
}

/// Every layer before Stage 5: no edges anywhere. The cut must short-circuit and the build must
/// allocate nothing.
fn flat_arm(n: u32) {
    let pairs: Vec<(u32, Option<u32>)> = (0..n).map(|ordinal| (ordinal, None)).collect();
    let passing: Vec<u32> = (0..n).collect();

    let start = Instant::now();
    let lineage = Lineage::new(pairs.iter().copied());
    let build = start.elapsed();
    assert!(lineage.is_flat());

    let start = Instant::now();
    let served = cut(&lineage, &passing, None, PRUNE);
    let unbudgeted = start.elapsed();
    assert_eq!(served.len(), n as usize);

    let start = Instant::now();
    let served = cut(&lineage, &passing, Some(100), PRUNE);
    let budgeted = start.elapsed();
    assert_eq!(
        served.len(),
        n as usize,
        "a flat level has no lineage to trade, so a budget takes nothing from it"
    );

    report(n, "flat", build, unbudgeted, budgeted);
}

/// A balanced tree over the whole level, with two artifacts in three clearing their criterion —
/// so the passing set is large enough to be the cost driver and holey enough that the frontier is
/// not simply the leaves.
fn treed_arm(n: u32) {
    let pairs: Vec<(u32, Option<u32>)> = (0..n)
        .map(|ordinal| {
            (
                ordinal,
                (ordinal > 0).then(|| (ordinal - 1) / BRANCH),
            )
        })
        .collect();
    let passing: Vec<u32> = (0..n).filter(|o| !o.is_multiple_of(3)).collect();

    let start = Instant::now();
    let lineage = Lineage::new(pairs.iter().copied());
    let build = start.elapsed();
    assert!(!lineage.is_flat());

    let start = Instant::now();
    let full = cut(&lineage, &passing, None, PRUNE);
    let unbudgeted = start.elapsed();

    let start = Instant::now();
    let narrow = cut(&lineage, &passing, Some(100), PRUNE);
    let budgeted = start.elapsed();
    assert!(
        narrow.len() <= full.len(),
        "a budget never serves more than the full frontier"
    );

    report(n, "treed", build, unbudgeted, budgeted);
}

/// One `(level, passing)` point: the same balanced tree, with only the passing share moved.
fn passing_arm(n: u32, want: u32) {
    let pairs: Vec<(u32, Option<u32>)> = (0..n)
        .map(|ordinal| (ordinal, (ordinal > 0).then(|| (ordinal - 1) / BRANCH)))
        .collect();
    // Spread over the level rather than taken from its head, so the passing set is not one subtree.
    let step = (n / want.max(1)).max(1);
    let passing: Vec<u32> = (0..n).step_by(step as usize).collect();

    let start = Instant::now();
    let lineage = Lineage::new(pairs.iter().copied());
    let build = start.elapsed();

    let start = Instant::now();
    let full = cut(&lineage, &passing, None, PRUNE);
    let unbudgeted = start.elapsed();

    let start = Instant::now();
    let narrow = cut(&lineage, &passing, Some(1_000), PRUNE);
    let budgeted = start.elapsed();
    assert!(narrow.len() <= full.len());

    println!(
        "{:>12}  {:>8.3}ms  {:>10.3}ms  {:>10.3}ms",
        passing.len(),
        build.as_secs_f64() * 1e3,
        unbudgeted.as_secs_f64() * 1e3,
        budgeted.as_secs_f64() * 1e3
    );
}

fn report(
    n: u32,
    arm: &str,
    build: std::time::Duration,
    unbudgeted: std::time::Duration,
    budgeted: std::time::Duration,
) {
    println!(
        "{:>10}  {:>8}  {:>10.3}ms  {:>10.3}ms  {:>10.3}ms  {:>12.1}",
        n,
        arm,
        build.as_secs_f64() * 1e3,
        unbudgeted.as_secs_f64() * 1e3,
        budgeted.as_secs_f64() * 1e3,
        build.as_nanos() as f64 / n as f64,
    );
}
