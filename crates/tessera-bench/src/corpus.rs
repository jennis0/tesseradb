//! Grant construction: turning a bundle's dictionary into principals with chosen properties.
//!
//! **This is the cheap variant axis, and the reason the label-set axis stays small.** Within one
//! bundle you can synthesise principals of any width, coverage and scatter for free by choosing
//! which terms to grant. What you cannot synthesise is signature-sorted entity contiguity — that
//! is decided at build time by `tessera_build::signature_sort_key` and is permanent under I9 — so
//! the label set is the only way to vary it, and varying it costs a rebuild. Hence: a handful of
//! label sets spanning the contiguity spectrum, and everything else swept here.
//!
//! **Fixed-w, not head-coverage, for scaling sweeps.** `probes/results.md` §4.3b is explicit that
//! head grants select terms until a coverage target is hit, so `w` changes with scale (2 -> 4 -> 9
//! for categories) and those rows conflate scale with grant composition. The clean sweep holds `w`
//! fixed and lets coverage emerge.
//!
//! **Determinism is a gate, not a nicety.** Every selection here is seeded, because gate G0
//! requires container counts and Sigma-visible to be bit-identical across runs of the same
//! `(scale, label_set, seed)`. If they move, the corpus or the grant construction changed and
//! every latency comparison in that run is void.

use std::path::Path;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use tessera_authz::PostingsReader;
use tessera_types::TermId;

/// How a principal's granted term set is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantShape {
    /// `w` terms drawn uniformly at random from the dictionary. Postings land wherever the label
    /// set put them, so at equal coverage this is the *scattered* arm.
    Random,
    /// The `w` widest postings. Reaches high coverage with few terms and exercises the dense
    /// container regime (~1.54 us/container vs ~114 ns for sparse).
    Head,
    /// The `w` narrowest non-empty postings. The tail principal — the case direct evaluation
    /// exists for, and the one `probes/results.md` §5 found sits below the crossover for
    /// essentially every tile.
    Tail,
    /// Terms whose postings are most entity-contiguous, i.e. the *clustered* arm at equal
    /// coverage. Paired with `Random`, this isolates the contiguity effect Phase 0 measured at up
    /// to ~130x on union cost with cardinality held constant.
    Clustered,
}

impl GrantShape {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "random" | "scattered" => Some(GrantShape::Random),
            "head" => Some(GrantShape::Head),
            "tail" => Some(GrantShape::Tail),
            "clustered" => Some(GrantShape::Clustered),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            GrantShape::Random => "random",
            GrantShape::Head => "head",
            GrantShape::Tail => "tail",
            GrantShape::Clustered => "clustered",
        }
    }
}

/// A constructed principal: the terms, and the descriptors needed to authorise as it over HTTP.
// `shape`/`seed` are carried for the record's `params` block and for the concurrency arm, which
// needs to build N *distinct* principals and prove they really are distinct.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Grant {
    pub terms: Vec<TermId>,
    pub shape: GrantShape,
    pub seed: u64,
}

impl Grant {
    /// The `Passthrough` plugin's auth-data form: `{"terms": ["<descriptor>", ...]}`.
    ///
    /// Descriptors must come from the bundle's own dictionary — `Dict::lookup` silently drops
    /// unknown ones, so a guessed descriptor yields a *smaller* mask than intended and a
    /// meaningless measurement. `scripts/bench_p99.py` learned this the same way and reads the
    /// dictionary file for the same reason.
    pub fn auth_json(&self, dict: &Dictionary) -> String {
        let quoted: Vec<String> = self
            .terms
            .iter()
            .filter_map(|t| dict.descriptor(*t))
            .map(|d| format!("{:?}", d))
            .collect();
        format!("{{\"terms\": [{}]}}", quoted.join(", "))
    }
}

/// The bundle's term dictionary, read straight off disk.
///
/// On-disk form (`<prefix>/dictionary/terms-0.dict`): repeated `u32` little-endian length
/// followed by that many bytes; a term's id is its ordinal. Same parse as
/// `scripts/bench_k_sweep.py`'s reader and `benches/viewport.rs`'s
/// `first_dictionary_descriptor`, so all three agree on what term 0 is.
pub struct Dictionary {
    descriptors: Vec<String>,
}

impl Dictionary {
    pub fn open(bundle_root: &Path, prefix: &str) -> std::io::Result<Self> {
        let path = bundle_root.join(prefix).join("dictionary/terms-0.dict");
        let data = std::fs::read(path)?;
        let mut descriptors = Vec::new();
        let mut offset = 0usize;
        while offset + 4 <= data.len() {
            let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + len > data.len() {
                break;
            }
            descriptors.push(String::from_utf8_lossy(&data[offset..offset + len]).into_owned());
            offset += len;
        }
        Ok(Dictionary { descriptors })
    }

    /// Vocabulary size — reported per fixture so a clamped `w` is interpretable.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    pub fn descriptor(&self, term: TermId) -> Option<&str> {
        self.descriptors
            .get(term.raw() as usize)
            .map(|s| s.as_str())
    }
}

/// Per-term statistics, computed once per bundle and reused across every grant.
///
/// Reading every posting is O(vocabulary) and is paid once; the alternative — choosing terms
/// without knowing their shape — makes `Head`, `Tail` and `Clustered` unimplementable and leaves
/// only `Random`, which is the arm that tells you least.
pub struct TermStats {
    /// `(term, cardinality, containers)` for every non-empty term, in term order.
    pub entries: Vec<(TermId, u64, u64)>,
}

impl TermStats {
    pub fn compute(postings: &PostingsReader) -> std::io::Result<Self> {
        let mut entries = Vec::new();
        for raw in 0..postings.term_count() {
            let term = TermId::new(raw);
            let bitmap = crate::postings::to_bitmap(postings, term)?;
            let cardinality = bitmap.cardinality();
            if cardinality == 0 {
                continue;
            }
            entries.push((term, cardinality, crate::metrics::containers(&bitmap)));
        }
        Ok(TermStats { entries })
    }

    /// Terms ordered widest first.
    fn by_width_desc(&self) -> Vec<(TermId, u64, u64)> {
        let mut v = self.entries.clone();
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.raw().cmp(&b.0.raw())));
        v
    }

    /// Terms ordered by *density within their containers* — cardinality per container, highest
    /// first. A term whose postings pack tightly into few containers is entity-contiguous; one
    /// whose postings scatter across many touches more containers for the same cardinality, and
    /// containers are what union cost is linear in.
    fn by_contiguity_desc(&self) -> Vec<(TermId, u64, u64)> {
        let mut v = self.entries.clone();
        v.sort_unstable_by(|a, b| {
            let da = a.1 as f64 / a.2.max(1) as f64;
            let db = b.1 as f64 / b.2.max(1) as f64;
            db.partial_cmp(&da)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.raw().cmp(&b.0.raw()))
        });
        v
    }
}

/// Build a grant of `w` terms with the requested shape.
///
/// `w` is clamped to the number of non-empty terms available, and the clamp is visible in the
/// returned grant's length rather than silently pretended away — the existing criterion bench hit
/// exactly this at 2.4M, where `fragment_build/10000` really measures w=176 because that is the
/// whole vocabulary. A cell that clamped must say so.
pub fn build_grant(stats: &TermStats, shape: GrantShape, w: usize, seed: u64) -> Grant {
    let terms: Vec<TermId> = match shape {
        GrantShape::Random => {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut all: Vec<TermId> = stats.entries.iter().map(|e| e.0).collect();
            all.shuffle(&mut rng);
            all.truncate(w);
            all.sort_unstable_by_key(|t| t.raw());
            all
        }
        GrantShape::Head => stats.by_width_desc().iter().take(w).map(|e| e.0).collect(),
        GrantShape::Tail => stats
            .by_width_desc()
            .iter()
            .rev()
            .take(w)
            .map(|e| e.0)
            .collect(),
        GrantShape::Clustered => stats
            .by_contiguity_desc()
            .iter()
            .take(w)
            .map(|e| e.0)
            .collect(),
    };
    Grant { terms, shape, seed }
}

/// Grow a grant until it covers at least `target` of `universe`, then stop.
///
/// For the *equal-coverage* comparisons — the only honest way to compare two mask shapes, since
/// `probes/results.md` §4.2 flags that comparing a 26%-coverage principal against a 100% one
/// measures the absence of masking rather than masking. Returns the grant and its achieved
/// coverage, which will overshoot the target by at most one term's worth.
pub fn build_grant_to_coverage(
    stats: &TermStats,
    postings: &PostingsReader,
    shape: GrantShape,
    target: f64,
    universe: u64,
    seed: u64,
) -> std::io::Result<(Grant, f64)> {
    let ordered: Vec<TermId> = match shape {
        GrantShape::Random => {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut all: Vec<TermId> = stats.entries.iter().map(|e| e.0).collect();
            all.shuffle(&mut rng);
            all
        }
        GrantShape::Head => stats.by_width_desc().iter().map(|e| e.0).collect(),
        GrantShape::Tail => stats.by_width_desc().iter().rev().map(|e| e.0).collect(),
        GrantShape::Clustered => stats.by_contiguity_desc().iter().map(|e| e.0).collect(),
    };

    let mut accumulated = croaring::Bitmap::new();
    let mut chosen = Vec::new();
    for term in ordered {
        if universe > 0 && (accumulated.cardinality() as f64 / universe as f64) >= target {
            break;
        }
        accumulated.or_inplace(&crate::postings::to_bitmap(postings, term)?);
        chosen.push(term);
    }
    let coverage = if universe == 0 {
        0.0
    } else {
        accumulated.cardinality() as f64 / universe as f64
    };
    chosen.sort_unstable_by_key(|t| t.raw());
    Ok((
        Grant {
            terms: chosen,
            shape,
            seed,
        },
        coverage,
    ))
}

/// A deterministic viewport generator: random bboxes at mixed zooms.
///
/// Same recipe as `scripts/bench_k_sweep.py::gen_viewports` — `span = extent / 2^max(zoom-4, 1)`,
/// zooms cycling 4..=12 — so a Rust-side random sweep and the existing Python one draw comparable
/// geometry. Seeded, because the tail-attribution memo showed the *input distribution* drives the
/// random-sweep p99 far more than the system does; two runs that drew different viewports are not
/// comparable at all.
pub fn gen_viewports(n: usize, extent: f64, seed: u64, zooms: &[u8]) -> Vec<(u8, [f64; 4])> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let zoom = zooms[i % zooms.len()];
        let span = extent / 2f64.powi(zoom.saturating_sub(4).max(1) as i32);
        let x0 = rng.gen_range(0.0..(extent - span).max(f64::MIN_POSITIVE));
        let y0 = rng.gen_range(0.0..(extent - span).max(f64::MIN_POSITIVE));
        out.push((zoom, [x0, y0, x0 + span, y0 + span]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_of(entries: &[(u32, u64, u64)]) -> TermStats {
        TermStats {
            entries: entries
                .iter()
                .map(|(t, c, k)| (TermId::new(*t), *c, *k))
                .collect(),
        }
    }

    #[test]
    fn head_takes_the_widest_tail_takes_the_narrowest() {
        let stats = stats_of(&[(0, 10, 1), (1, 1000, 5), (2, 50, 2)]);
        let head = build_grant(&stats, GrantShape::Head, 1, 0);
        assert_eq!(head.terms, vec![TermId::new(1)]);
        let tail = build_grant(&stats, GrantShape::Tail, 1, 0);
        assert_eq!(tail.terms, vec![TermId::new(0)]);
    }

    #[test]
    fn clustered_prefers_high_cardinality_per_container() {
        // term 1: 1000 values in 1 container (dense). term 2: 1000 values in 500 (scattered).
        let stats = stats_of(&[(1, 1000, 1), (2, 1000, 500)]);
        let clustered = build_grant(&stats, GrantShape::Clustered, 1, 0);
        assert_eq!(clustered.terms, vec![TermId::new(1)]);
    }

    #[test]
    fn random_grants_are_deterministic_across_calls() {
        // Gate G0 depends on this: same seed, same terms, therefore same containers and
        // Sigma-visible, therefore latencies that can be compared at all.
        let stats = stats_of(&(0..100).map(|i| (i, 10 + i as u64, 2)).collect::<Vec<_>>());
        let a = build_grant(&stats, GrantShape::Random, 10, 42);
        let b = build_grant(&stats, GrantShape::Random, 10, 42);
        assert_eq!(a.terms, b.terms);
        let c = build_grant(&stats, GrantShape::Random, 10, 43);
        assert_ne!(a.terms, c.terms, "a different seed must draw differently");
    }

    #[test]
    fn w_is_clamped_to_the_vocabulary_and_the_clamp_is_visible() {
        let stats = stats_of(&[(0, 10, 1), (1, 20, 1)]);
        let g = build_grant(&stats, GrantShape::Random, 10_000, 0);
        assert_eq!(
            g.terms.len(),
            2,
            "clamped to what exists, and observably so"
        );
    }

    #[test]
    fn viewport_generation_is_deterministic_and_inside_the_extent() {
        let a = gen_viewports(50, 65536.0, 7, &[4, 6, 8]);
        let b = gen_viewports(50, 65536.0, 7, &[4, 6, 8]);
        assert_eq!(a.len(), 50);
        for ((za, ba), (zb, bb)) in a.iter().zip(b.iter()) {
            assert_eq!(za, zb);
            assert_eq!(ba, bb);
            assert!(
                ba[0] >= 0.0 && ba[2] <= 65536.0,
                "bbox inside extent: {ba:?}"
            );
            assert!(
                ba[0] < ba[2] && ba[1] < ba[3],
                "non-degenerate bbox: {ba:?}"
            );
        }
    }
}
