//! **Ask 1: user authorisation performance.**
//!
//! Measures `tessera_authz::build_fragment` — design §2.6's step A4, the posting union that
//! dominates session materialisation — against real mmapped postings.
//!
//! **Not `Engine::authorise`.** `FragmentCache::get_or_build` memoises by (terms, auth hash), so
//! timing `authorise` in a loop measures a cache hit after the first iteration. The existing
//! criterion bench documents the same trap and takes the same way out.
//!
//! **Swept over `w` and shape, not only over point count.** The owner's ask was "authorisation
//! performance as a function of point count", but Phase 0 measured the drivers as grant width and
//! *posting shape*: at 10⁹, equal-coverage principals ranged 21.7 ms to 2,885 ms — ~130x — purely
//! on entity-space contiguity, while dictionary scale was free (117M terms cost the same as 10M).
//! Point count is still an axis (it is the fixture tier), but sweeping it alone would produce a
//! flat line that hides the effect that actually matters.

use crate::arms::{Context, Result};
use crate::corpus::{build_grant, GrantShape, TermStats};
use crate::report::Work;
use tessera_authz::{build_fragment, PostingsReader};

pub fn run(ctx: &Context, widths: &[usize], shapes: &[String], seed: u64) -> Result<()> {
    let mut run = ctx.open("authorise")?;

    let shapes: Vec<GrantShape> = shapes
        .iter()
        .map(|s| GrantShape::parse(s).ok_or_else(|| format!("unknown grant shape {s:?}")))
        .collect::<std::result::Result<_, _>>()?;

    for fixture in &ctx.fixtures {
        let postings = PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let vocabulary = stats.entries.len();
        eprintln!(
            "authorise: scale={} set={} vocabulary={}",
            fixture.scale, fixture.label_set, vocabulary
        );

        for &shape in &shapes {
            for &w in widths {
                let grant = build_grant(&stats, shape, w, seed);
                if grant.terms.is_empty() {
                    continue;
                }

                let cell_id = format!(
                    "authorise/{}/{}/{}/w{}",
                    fixture.scale,
                    fixture.label_set,
                    shape.name(),
                    w
                );
                if run.ledger.is_done(&cell_id) {
                    run.skipped += 1;
                    continue;
                }

                let samples = crate::metrics::repeat(ctx.repeat, || {
                    build_fragment(&grant.terms, &postings).expect("fragment build")
                });

                // Built once more outside the timed loop for the work counters. Container count
                // is the number that makes this latency portable across machines and scales.
                let mask = build_fragment(&grant.terms, &postings)?;
                let coverage = mask.cardinality() as f64 / fixture.scale as f64;

                let mut flags = Vec::new();
                if grant.terms.len() < w {
                    // The existing criterion bench hits this at 2.4M, where `fragment_build/10000`
                    // really measures w=176 because that is the whole vocabulary. Say so in the
                    // record rather than letting the label lie.
                    flags.push(format!("clamped_w={}", grant.terms.len()));
                }

                let work = Work {
                    containers: crate::metrics::containers(&mask),
                    mask_cardinality: mask.cardinality(),
                    coverage,
                    run_ratio: crate::metrics::run_ratio(&mask, fixture.scale),
                    degenerate: coverage >= 0.999,
                    ..Default::default()
                };

                run.emit(
                    cell_id,
                    fixture,
                    serde_json::json!({
                        "w_requested": w,
                        "w_actual": grant.terms.len(),
                        "shape": shape.name(),
                        "seed": seed,
                        "vocabulary": vocabulary,
                    }),
                    work,
                    samples,
                    None,
                    flags,
                )?;
            }
        }
    }

    run.finish();
    Ok(())
}
