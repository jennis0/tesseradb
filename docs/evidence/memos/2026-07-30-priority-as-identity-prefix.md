# `priority` becomes a prefix of `tessera_id` (2026-07-30)

**Status:** decision, for review. **Touches:** contracts §2.6; design §7.2, §12.3; the
external-id identity plan's Important I-4; `sort_batch`; the selection comparator;
candidate-list construction; the reference oracle; the byte-scanner.

**Deadline:** the identity plan rebuilds the 10⁹ bundle. This changes the storage sort
order, so landing it afterwards means rebuilding twice.

---

## The defect

`priority` is `high16(splitmix64(entity_id))` — u16, so 65,536 distinct values. For a
tile with **V** visible items the k-th lowest priority sits at ≈ k·2¹⁶/V, which is a
resolvable value only when **V ≤ 2¹⁶·k**. At k=30 that threshold is V ≈ 2×10⁶. Above it
every candidate carries the same priority and the *tiebreak* becomes the sampler.

The tiebreak is `entity_id`: storage order is `(morton, priority, entity_id)` and
direct evaluation keeps the k lowest out of a partial sort, so equal priorities resolve
by row order. Entity IDs are **signature-sorted**, permanently, under I9.

**So above V ≈ 2×10⁶ the sample is ordered by permission signature.** A principal whose
visible set spans groups A and B, where A was allocated lower entity IDs, sees mostly A
at coarse zoom even if B is ten times larger.

This keeps the letter of I7 and breaks its purpose. §7.2 rejects global LOD sampling
because "a principal authorised for a small or clustered slice would see a nearly empty
screen while thousands of authorised items sat invisible beneath a sample that selected
around them". This is that failure moved *inside* the visible set, and it correlates
with permissions precisely because the entity allocator was made permission-aware.

It bites **head principals, not tail** — at 0.01% coverage V at depth 0 is 10⁵ and the
cut resolves; at 25% coverage V is 2.5×10⁸ and the sample is tie-dominated from roughly
depth 3 upward. Candidate lists inherit it: they are built as "top c·k by priority",
tie-broken the same way. Large k *helps* (V ≤ 2¹⁶·k), so the binding case is the default
overview at k≈30 — the first screen a user sees.

**Second, independent defect.** D8 made `entity_id` a shard-local `u32`, so
`splitmix64(entity_id)` is no longer the "global per-item property" §12.3's composition
argument names. Item 12,345 carries an identical priority in every shard, and
`(priority, shard_id, entity_id)` as a global order makes shard 0 win every tie.

**Confirmation, when the tree is quiet:** over the existing 10⁹ k-sweep, count the
(tile, principal) pairs with V > 2×10⁶. No new corpus needed.

## The decision

**`priority` is defined as `high16(tessera_id)`.** Same column, same u16 width, same
2 B/row hot read. `splitmix64`-over-entity is deleted as an independent construction;
the storage sort key becomes `(morton, tessera_id)`; the selection comparator falls
through to the full `tessera_id` on prefix ties. **Zero bytes change in the bundle.**

**Why ties stop mattering.** `tessera_id` is a keyed bijection over 2⁶⁴ — globally
unique, uniform, and uncorrelated with signature. Because the u16 is a *prefix* of it,
"k lowest by priority then by `tessera_id`" is identically "k lowest by `tessera_id`".
There is no composite comparator to get subtly wrong, and the sample is correct at any
prefix width. Width becomes a performance knob only: fall-through volume is ≈ V/2^w,
which at 10⁹ is a few thousand scattered 8 B reads against a prefix scan of hundreds of
megabytes.

**A strided read of the wide column is not an alternative.** Reading the high 2 bytes of
a `uint64` array at stride 8 touches every page holding any value (512 u64 per page
against 2,048 u16) and pulls the whole cache line regardless. A cheap prefix must be
physically contiguous, which means a column. This is §10.4's column-major argument one
level down.

## What this retires

**Important I-4 — `priority` forbidden on the viewer plane — no longer applies.** The
prohibition exists solely because `priority` is an *unkeyed* 16-bit residue of the
entity ID: publishing it hands a viewer a 65,536× narrowing of entity space per mark,
combinable across a viewport. A keyed function of a value already on the wire discloses
nothing. A viewer receiving k marks learns only that their priorities fall below some
cut P, and P is fully determined by k and by the exact masked count V, which §7.1
already gives them. Nothing about unseen items is recoverable.

The general rule the identity plan states — *a hot column may be shown only if it is
independent of the entity ID or keyed under the deployment key* — is unchanged, and
`priority` now satisfies its second limb. **I-4 is retired by argument, not weakened by
convenience; the byte-scanner sweep for it can go.**

## What must be verified, not assumed

The high 32 bits of `tessera_id` are the left Feistel half `L`, and the identity plan is
explicit that the construction is "a blinding permutation, not a cipher". Sampling needs
**uniformity, not unpredictability**, and 8 balanced rounds of `splitmix64` deliver it —
but the inputs are highly structured (`shard_id = 0`, `entity_id` dense from 0), and
residual structure in `L` would be inherited by the sample, which is the exact defect
being fixed. **Add a chi-squared check over `high16(tessera_id)` alongside the existing
known-answer vectors in `build_equivalence`.**

## Residuals, recorded not hidden

- **The sample reshuffles on a re-key as well as on a reshard.** Today it reshuffles on
  reshard only, since `entity_id` changes. Re-key is rare and operator-initiated, and a
  reshard already bumps the identity epoch. Accepted.
- **Prefix width may need revisiting at 10¹²⁺.** If the coarse-zoom path remains a live
  scan rather than a session-established summary, fall-through at zoom 0 grows to
  ~1.5×10⁸ rows. The trigger is `w ≈ log₂(V_max/k)` — about 24 bits for a 10⁹ shard at
  head coverage. Record the trigger; do not pay for it now.
- **Not addressed here:** sample stability across resharding. Hashing the durable
  `external_id` would give it, but that key is absent for items whose identity *is* their
  `tessera_id`, so stability would be mixed. More machinery than the rare event warrants.

## Change list

| Where | Change |
|---|---|
| contracts §2.6 | `priority` redefined as `high16(tessera_id)`; the standalone priority function is deleted |
| `crates/tessera-spatial/src/tiler.rs` | `sort_batch` tiebreak `(morton, priority, entity_id)` → `(morton, tessera_id)` |
| selection path | comparator falls through to full `tessera_id` on prefix ties |
| candidate-list build | top c·k by the same order |
| `reference/oracle/` | reproduce `priority` from `tessera_id` |
| byte-scanner | drop the I-4 sweep |
| identity plan | I-4 marked retired, with the argument above |
| `build_equivalence` | chi-squared uniformity check over `high16` |

Provenance: brainstorming session 2026-07-30, arising from a scale-out discussion
(disk residency, signature-aligned sharding, the drawn-mark budget). The width question
was raised by the owner and is what showed the width to be immaterial.
