//! The Stage 7 campaign's fixture arithmetic: the two things a driver cannot ask the CLI for.
//!
//! `tessera corpus materialise` writes the campaign's fixture, and `tessera corpus artifact-census`
//! answers it — between them they cover every arm except two facts a measurement driver needs and
//! neither verb prints:
//!
//! - **The boundary arm's geometry.** `tessera-corpus`'s spatial arm carries a roster of authored
//!   depth-*d* tile prefixes and no boxes, because it was written while nothing read one
//!   (`boundary.rs`'s header). A `membership = "spatial"` layer declares `{ key, bbox }` per
//!   artifact, so the campaign authors the boxes here — **the middle half of each authored tile**,
//!   with the assertion `tiles_for_bbox` covers exactly that one prefix, which is what makes the
//!   served membership and `boundary_artifact_census` the same set rather than two nearby ones.
//!   That is `artifact_spatial.rs`'s fixture rule at campaign scale, not a new one.
//! - **What a grant is worth.** At `--terms-per-level 65536` the corpus carries ~10⁶ terms and a
//!   single term covers a ten-thousandth of a percent of it, so a principal at a stated breadth is
//!   a term *set* whose size is itself the finding. The ladder is constructed analytically —
//!   whole levels, then a prefix of the next level's slots — and then **measured** by an O(*n*)
//!   pass, because the analytic figure is a probability and the campaign reports what the corpus
//!   actually holds.
//!
//! Nothing here measures the engine. It writes a declaration fragment and a JSON sidecar the
//! probe's Python driver reads; the measurement is over there.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use rayon::prelude::*;
use tessera_corpus::materialise::{BOUNDARY_LAYER, FIXTURE_LEVEL, PARTITION_LAYER, TREED_LAYER};
use tessera_corpus::{Corpus, Grant};
use tessera_spatial::Bounds;

/// The generator's own quantisation extent — the cell grid's coordinates, which is what
/// `tessera corpus materialise` writes into its declaration and what every census is stated in.
const GRID: f64 = 65536.0;

/// The term levels the generator emits (`tessera_corpus`'s `TERM_LEVELS`). Not importable — it is
/// private to that crate — and restated here only to bound the ladder's search; the **measured**
/// breadth beside every rung is what the campaign reports, so a drift in this constant would show
/// up as a ladder that stops early rather than as a wrong number.
const TERM_LEVELS: u32 = 16;

#[derive(Parser)]
#[command(about = "The artifact scale campaign's fixture arithmetic: boundary boxes and the \
                   principal ladder")]
struct Args {
    #[arg(long)]
    seed: u64,
    #[arg(long)]
    n: u64,
    /// Slots per term level — the campaign's `materialise --terms-per-level`, and the same value
    /// or the grants below name terms this corpus cannot emit.
    #[arg(long, default_value_t = 65_536)]
    terms_per_level: u32,
    /// Where to write `boundary-layer.toml` and `fixture.json`.
    #[arg(long)]
    out: PathBuf,
    /// The principal breadths to construct, as fractions of the corpus.
    ///
    /// **The broadest rung is 93.75% and not 100%, and that is a measured ceiling rather than a
    /// choice.** At this term width a whole-corpus principal holds all 1 048 576 terms, and
    /// `/session/authorise` buffers `auth_data` under axum's 2 MB default body limit — about
    /// 150 000 seven-digit descriptors. Two whole term levels (131 072 terms, 93.75% of the
    /// corpus) is the broadest grant that fits; the campaign's README records the refusal.
    #[arg(long, value_delimiter = ',', default_values_t = vec![0.9375, 0.75, 0.5, 0.25, 0.094, 0.031])]
    breadths: Vec<f64>,
    /// Skip the O(*n*) measuring pass and report the analytic breadth alone. For a driver that
    /// only wants the boundary geometry at a tier where the pass would cost minutes.
    #[arg(long)]
    no_measure: bool,
}

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: GRID,
        y_min: 0.0,
        y_max: GRID,
    }
}

/// The `(tx, ty)` a depth-`depth` prefix interleaves from, by inverting the interleave one bit at
/// a time — the arithmetic inverse of `interleave_bits`, not a search, because at depth 6 a search
/// is fine and at depth 16 it is not, and this binary should not carry a bound it does not need.
fn deinterleave(prefix: u64, depth: u8) -> (u32, u32) {
    let mut tx = 0u32;
    let mut ty = 0u32;
    for bit in 0..u32::from(depth) {
        tx |= (((prefix >> (2 * bit)) & 1) as u32) << bit;
        ty |= (((prefix >> (2 * bit + 1)) & 1) as u32) << bit;
    }
    (tx, ty)
}

/// The middle half of tile `prefix`, in extent coordinates.
///
/// **The middle half rather than the whole tile**, for `artifact_spatial.rs`'s reason: a tile's
/// bounds are half-open and a box reaching them quantises into the neighbour beyond. The assertion
/// below is what makes "covers exactly this tile" a fact rather than an intention — and it has to
/// be a fact, because the census counts every point in the tile and the engine counts every point
/// in the covering ranges.
fn box_of(prefix: u64, depth: u8) -> [f64; 4] {
    let (tx, ty) = deinterleave(prefix, depth);
    let span = 65_536u32 >> depth;
    let quarter = span / 4;
    let at = |cells: u32| f64::from(cells) * GRID / 65_536.0;
    let bbox = [
        at(tx * span + quarter),
        at(ty * span + quarter),
        at(tx * span + 3 * quarter),
        at(ty * span + 3 * quarter),
    ];
    let covering = tessera_spatial::tiles_for_bbox(bbox, depth, &extent());
    assert_eq!(
        covering.len(),
        1,
        "the campaign's box for prefix {prefix} covers {} tiles, not one",
        covering.len()
    );
    assert_eq!(
        covering[0].prefix, prefix,
        "the campaign's box for prefix {prefix} covers the wrong tile"
    );
    bbox
}

/// One rung of the principal ladder: whole term levels `0..levels`, plus the first `slots` slots of
/// level `levels`.
///
/// A draw lands at level *L* with probability `2^-(L+1)` and in a uniform slot within it, and an
/// item takes two independent draws — so this set's per-draw probability is
/// `(1 - 2^-levels) + 2^-(levels+1) * slots/tpl` and its coverage is `1 - (1 - p)^2`. That is the
/// **construction**; the number the campaign quotes is the measured one beside it.
fn rung(levels: u32, slots: u32, tpl: u32) -> Vec<u32> {
    let mut terms: Vec<u32> = Vec::with_capacity((levels * tpl + slots) as usize);
    for level in 0..levels {
        for slot in 0..tpl {
            terms.push(level * tpl + slot);
        }
    }
    for slot in 0..slots {
        terms.push(levels * tpl + slot);
    }
    terms
}

/// The `(levels, slots)` whose analytic coverage is closest to `target` from below, capped at the
/// generator's own level count — so a target of 1.0 returns the whole term space and is reported
/// as whatever that actually covers.
fn ladder_rung(target: f64, tpl: u32) -> (u32, u32) {
    let p_target = 1.0 - (1.0 - target.clamp(0.0, 1.0)).sqrt();
    let mut levels = 0u32;
    while levels < TERM_LEVELS && 1.0 - 2f64.powi(-((levels + 1) as i32)) <= p_target {
        levels += 1;
    }
    if levels >= TERM_LEVELS {
        return (TERM_LEVELS, 0);
    }
    let base = 1.0 - 2f64.powi(-(levels as i32));
    let step = 2f64.powi(-((levels + 1) as i32));
    let slots = (((p_target - base) / step) * f64::from(tpl)).round().max(0.0) as u32;
    (levels, slots.min(tpl))
}

fn main() {
    let args = Args::parse();
    let corpus = Corpus::with_terms_per_level(args.seed, args.n, extent(), args.terms_per_level)
        .expect("the campaign's extent and term width");
    fs::create_dir_all(&args.out).expect("the output directory");

    // ---- the boundary arm's geometry -------------------------------------------------------
    let depth = corpus.boundary_depth(BOUNDARY_LAYER, FIXTURE_LEVEL);
    let roster = corpus.boundary_artifacts(BOUNDARY_LAYER, FIXTURE_LEVEL);
    let mut toml = String::new();
    toml.push_str(&format!(
        "# The campaign's spatial arm: `tessera-corpus`'s authored depth-{depth} tile prefixes,\n\
         # given the geometry the generator's roster does not carry. One box per authored tile,\n\
         # its middle half, asserted at emission to cover exactly that tile — so this layer's\n\
         # membership and `artifact-census --layer boundary` are the same set.\n\
         [[layer]]\n\
         name                      = \"campaign/boundary\"\n\
         views                     = [\"s0\"]\n\
         membership                = \"spatial\"\n\
         hierarchy                 = {{ kind = \"flat\" }}\n\
         visibility                = \"public\"\n\
         artifact_visibility       = {{ default = \"inherited\" }}\n\
         require_member_visibility = \"none\"\n\
         artifacts = [\n"
    ));
    for &prefix in &roster {
        let b = box_of(prefix, depth);
        toml.push_str(&format!(
            "  {{ key = \"{prefix}\", bbox = [{}, {}, {}, {}] }},\n",
            b[0], b[1], b[2], b[3]
        ));
    }
    toml.push_str("]\n\n  [layer.shape]\n  kind  = \"bbox\"\n");
    toml.push_str(&format!("  depth = {depth}\n"));
    fs::write(args.out.join("boundary-layer.toml"), &toml).expect("the boundary declaration");

    // ---- the principal ladder ---------------------------------------------------------------
    let mut grants = Vec::new();
    for &target in &args.breadths {
        let (levels, slots) = ladder_rung(target, args.terms_per_level);
        let terms = rung(levels, slots, args.terms_per_level);
        let encoded = terms
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let analytic = {
            let p = (1.0 - 2f64.powi(-(levels as i32)))
                + 2f64.powi(-((levels + 1) as i32)) * f64::from(slots) / f64::from(args.terms_per_level);
            1.0 - (1.0 - p).powi(2)
        };
        let visible = if args.no_measure {
            None
        } else {
            let grant = Grant::parse_bounded(&encoded, corpus.term_space())
                .expect("the ladder names terms inside this corpus's space");
            Some(
                (0..args.n)
                    .into_par_iter()
                    .filter(|&e| corpus.visible(e, &grant))
                    .count() as u64,
            )
        };
        grants.push(BTreeMap::from([
            ("target".to_string(), serde_json::json!(target)),
            ("levels".to_string(), serde_json::json!(levels)),
            ("slots".to_string(), serde_json::json!(slots)),
            ("terms".to_string(), serde_json::json!(terms.len())),
            ("analytic_fraction".to_string(), serde_json::json!(analytic)),
            ("visible".to_string(), serde_json::json!(visible)),
            (
                "measured_fraction".to_string(),
                serde_json::json!(visible.map(|v| v as f64 / args.n as f64)),
            ),
            ("grant".to_string(), serde_json::json!(encoded)),
        ]));
    }

    // The single-term principal the campaign's census spread needs: one level-0 slot, which at this
    // width is a ten-thousandth of a percent of the corpus and the narrowest non-empty mask the
    // generator can state.
    let single = "0".to_string();
    let single_visible = if args.no_measure {
        None
    } else {
        let grant = Grant::parse_bounded(&single, corpus.term_space()).expect("term 0 exists");
        Some(
            (0..args.n)
                .into_par_iter()
                .filter(|&e| corpus.visible(e, &grant))
                .count() as u64,
        )
    };
    grants.push(BTreeMap::from([
        ("target".to_string(), serde_json::json!("single-term")),
        ("levels".to_string(), serde_json::json!(0)),
        ("slots".to_string(), serde_json::json!(1)),
        ("terms".to_string(), serde_json::json!(1)),
        (
            "analytic_fraction".to_string(),
            serde_json::json!(1.0 - (1.0 - 0.5 / f64::from(args.terms_per_level)).powi(2)),
        ),
        ("visible".to_string(), serde_json::json!(single_visible)),
        (
            "measured_fraction".to_string(),
            serde_json::json!(single_visible.map(|v| v as f64 / args.n as f64)),
        ),
        ("grant".to_string(), serde_json::json!(single)),
    ]));

    let sidecar = serde_json::json!({
        "seed": args.seed,
        "n": args.n,
        "terms_per_level": args.terms_per_level,
        "term_space": corpus.term_space(),
        "boundary_depth": depth,
        "boundary_artifacts": roster.len(),
        "flat_artifacts": corpus.artifacts_in(tessera_corpus::materialise::FLAT_LAYER, FIXTURE_LEVEL),
        "partition_artifacts": corpus.partition_count(PARTITION_LAYER),
        "treed_artifacts": corpus.treed_count(TREED_LAYER),
        "grants": grants,
    });
    fs::write(
        args.out.join("fixture.json"),
        serde_json::to_string_pretty(&sidecar).expect("the sidecar serialises"),
    )
    .expect("the fixture sidecar");
    println!(
        "campaign fixture: boundary depth {depth}, {} authored tile(s); {} grant rung(s) written \
         to {}",
        roster.len(),
        grants.len(),
        args.out.display()
    );
}
