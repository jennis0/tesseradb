# 0140 — The consistency pass rulings

**Date:** 2026-09-11 · **Status:** Settled (owner ruling) · Not built: applied by the pass in
[`../evidence/memos/2026-09-11-duplication-and-consistency-pass.md`](../evidence/memos/2026-09-11-duplication-and-consistency-pass.md),
whose §7 states each option, except C and the drawing half of D, which are capabilities and are
listed in [`../evidence/memos/2026-09-11-capability-gaps.md`](../evidence/memos/2026-09-11-capability-gaps.md). Applies [decision 0139](0139-one-implementation-between-build-and-ingest-and-across-a-type-family.md).

| | Ruling | Reason |
|---|---|---|
| A | `type = "u16", vocabulary = "v"` is refused at both entry points; `category` is the one spelling | nothing in the tree uses the width spelling on the control route, and one spelling removes the engine's rewrite path |
| B | an integer where a float metadata value is declared is accepted and widened at both | TOML and JSON write `0` as an integer; the stored value is a float either way, and a float where an integer is declared stays refused |
| C | a category-typed view metadata value is accepted on the create route as the build accepts it: a closed vocabulary resolves or refuses, an open one mints on the write executor | the handler resolves and the executor mints, serially, as for an ingest cell; a view create is already an executor command |
| D | a supplied content `type` is one of `text`, `polygon`, `extent`, `point`, `circle`, `ellipse`, checked in `LayerDeclaration::validate` and nowhere else | polygon-membership §6.1 (h) names the three shape words and the type recognises them; whether an authored circle or ellipse is drawn end to end is verified by the pass and marked not-built if it is not |
| E | one `SingleFlightCache`, in a leaf crate `tessera-cache` under `tessera-authz` and `tessera-engine` | the twin's only stated reason is crate direction, and `check-layers.sh` denies named edges, not a new leaf |
| F | the engine's examples become `tessera-bench` binaries | they are live measurement tools that duplicate bench's own sweeps; a probe crate stops compiling when an API moves |
| G | test support is one crate, `tessera-testkit`, a dev-dependency; the `tessera-corpus` check in `check-layers.sh` runs over `-e normal,dev` | one implementation of the parquet writers, the `BuildArgs` constructor and the bundle assembler; the widened check keeps the generator off it (correctness-suite §13) |
| H | the pass is tracked in [`../consistency-pass.md`](../consistency-pass.md), a committed file that is the sole authority for its status, and not in GitHub issues | owner direction; the second such exception under `agents/epic-lifecycle.md` |
| I | a layer's gate is a list of labels, any one satisfying, as a view's is; one resolver, the plugin's `terms_of_labels` as `gate.rs` applies it, serves the layer gate, the artifact gate and the view gate | architecture §3 names that derivation as the one run at both entry points; the layer path's literal dictionary lookup agrees with it under `builtin:passthrough` only |
