# Test audit R5 — `tessera-authz`, `tessera-filter`, `tessera-filter-write`

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests,
not the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect
in shipped code was found. Track R5 of the test-quality campaign; Wave 0's
[reachability memo](2026-08-30-test-reachability.md) is the input for which tests run.
`conformance.md` §4.6 is untouched — a row moves only when a test moves with it, and no test moved.

## Results

**193 tests assessed**, all of them executed by the per-PR gate: `tessera-authz` 77 (33 in `src/`
across ten harnesses, the rest in the nine integration files), `tessera-filter` 79 (46 in `src/`,
26 in `tests/dict.rs`, 7 in `tests/values_writer.rs`), `tessera-filter-write` 37 (23 in `src/`, 14
in `crates/tessera-filter-write/tests/record_blob.rs`). **No `#[ignore]` anywhere in the three crates**, no feature-gated test,
no test reachable only under `--workspace`. L1 is clean and took one command; the budget went to L2
and L4.

**The surface is strong, and where it is weak the weakness is absence rather than a bad test.** The
one finding that matters was found by enumerating a design claim and asking what would go red if it
were violated — not by reading a test and doubting it. Nothing on this surface was found that cannot
fail *and* carries a claim nobody else carries; the one test that provably cannot fail (F2) names a
claim covered properly elsewhere.

**What L2 checked and cleared, since this is the mask machinery.** A fixture whose principal sees
everything or nothing frequently cannot fail, so every masking-shaped fixture was checked
individually. None is vacuous. The credential here is a term-id list, and the tests that matter use a
*narrow* one against a fixture holding terms it does not carry: `crates/tessera-authz/tests/fragment_deltas.rs:55` grants
term 0 against a base and a tier that both also carry term 1 and asserts entities `2` and `11` are
**absent**; `crates/tessera-filter/src/values.rs:1347` puts the value under test on five entities, admits three to the
candidate, and asserts exactly one hit. Every fragment-cache fixture unions five to ten terms whose
postings cover at most 5,000 of a 100,000-entity universe, so full coverage never arises. The
`column.rs` narrowing tests and `crates/tessera-authz/src/term_sweep.rs` carry `tests/fold.rs`-style **"mutations this
kills"** paragraphs naming the specific defect each body catches.

**Three findings, in three different classes.** F1 is **missing** and is the reason this memo exists:
a function whose own doc calls ordinal preservation "the whole of its correctness argument" has no
test anywhere in the workspace, and the renumbering it forbids is a cross-compartment disclosure.
F2 is **vacuous**: both sides of its equality are the same cached object. F3 is
**under-discriminating** in the bare-`is_err()` form: a fail-closed refusal whose test cannot tell it
from a truncation. All three were proven by mutation; nothing was reported on suspicion.

## Findings

### F1 — `coalesce_dict_extents` has no test at all, and its ordinal-preservation claim is unasserted anywhere in the workspace

**Claim:** a dictionary coalesce writes the same records in the same order, so no term ordinal
moves. Its own doc states what a violation costs, and nothing checks it.

**Evidence:** `crates/tessera-authz/src/dict.rs:186`

> **Ordinal-preserving by construction, which is the whole of its correctness argument.** … A
> caller that reordered the list, or coalesced a non-contiguous selection, would renumber every
> ordinal after the gap, and a session's granted terms are resolved once at authorise and never
> re-resolved: it would evaluate against a different term than the one it was granted.

`coalesce_dict_extents` (`crates/tessera-authz/src/dict.rs:204`) appears in exactly two places
outside its own file: the re-export at `crates/tessera-authz/src/lib.rs:9` and the call site at
`crates/tessera-engine/src/coalesce.rs:585`. `crates/tessera-authz/src/dict.rs`'s test module has
four cases and none of them names it. No test in any crate resolves a descriptor to an ordinal, or
authorises a credential, after a coalesce has run.

**Class:** missing. **Severity: S1** — the failure mode the function's doc describes is a session
served another compartment's postings, which is I1 and I2 at once. It is also S2-shaped: term ids
determine entity ids and a published ordinal is not recoverable by a rerun.

**What a defect would let through:** any change that reorders inputs, drops a trailing record, or
skips an empty descriptor renumbers every ordinal after that point. The postings were written under
the old numbering, so a session granted descriptor *A* evaluates against *B*'s posting — silently,
with no error at any layer, and permanently, because the coalesced extent replaces its inputs in the
manifest.

**Confidence: high, and proven by mutation.** In a throwaway worktree, `for path in inputs` became
`for path in inputs.iter().rev()` — precisely the reordering the doc forbids, and a real one here
because the engine coalesces eight single-record extents (`tests/coalesce.rs:23`, `WIDTH = 8`). Every
binary that could plausibly see it stayed **green**: `-p tessera-authz` 77/77, and `tessera-engine`'s
`--test coalesce` 3/3, `--test soak` 2/2, `--test promotion` 4/4, `--test dict_generation` 5/5,
`--test staleness_hint` 3/3. The soak's visibility assertion cannot see it because the session's
grant descriptor lives in the build's base extent, which a coalesce does not touch; the coalesce
test asserts extent *counts* and that extent 0 keeps its path, neither of which a renumbering moves.

**Why the gap is conspicuous rather than systematic:** the other three functions in the same family
are pinned exactly, and one of them is pinned *because* it once had this bug.
`crates/tessera-authz/src/dict.rs:488`'s
`reload_equals_in_memory_extension_even_when_an_extent_repeats_a_descriptor` documents the historical
fail-open in as many words — "after a restart a session granted `a` was served `b`'s items —
silently, and across a compartment boundary" — and asserts `a → 0, b → 1` by name so the equality
cannot be satisfied by moving both. `extending_from_memory_equals_extending_from_the_extent_it_wrote`
(`:509`) and `extending_from_memory_preserves_every_existing_ordinal` (`:527`) do the same for the
in-memory paths. The coalesce is the one member of the family with nothing.

**Disposition:**

### F2 — `coalescing_tiers_leaves_the_fragment_unchanged` cannot fail: both sides of its equality are the same cached object

**Claim:** a fragment built over coalesced tiers equals one built over the separate tiers, which is
why a merge needs no cache key of its own. The second build never happens.

**Evidence:** `crates/tessera-authz/tests/fragment_cache_key.rs:72`. The two calls differ only in
their `auth_data_hash` (`[3u8; 32]` then `[4u8; 32]`) and in the tiers passed; the terms
(`[TermId::new(0)]`) and the watermark (`20`) are identical. `auth_data_hash` is **not** part of the
canonical key — `canonical_key` (`crates/tessera-authz/src/fragment.rs:137`) hashes
`bundle_identity ‖ auth_plugin_hash ‖ watermark ‖ sorted term ids` and nothing else, and
`get_or_build` uses `auth_data_hash` only as a memo key for that computation
(`crates/tessera-authz/src/fragment.rs:846`). So the second call takes a `Ready` hit on the slot the
first call published, returns the first fragment, and the `deltas` argument is never read.

**Class:** vacuous. **Severity: S3.** No disclosure surface is left bare — the content-preservation
half of the claim is covered directly and well at `crates/tessera-authz/tests/tier_coalesce.rs`
(dedup, every term surviving in term order, the Roaring arm, the one-input identity, and
`coalescing_retires_nothing`). What is genuinely untested is the *composition*: nothing in the
workspace builds a fragment over the output of `coalesce_delta_tiers`.

**What a defect would let through:** a coalesce whose output a fragment build reads differently from
the separate tiers it replaced — a tag-boundary disagreement between `coalesce_delta_tiers`'s encode
and `build_fragment_with_deltas`'s decode, say. Since the cache deliberately reuses the pre-merge
entry, the divergence would appear only after a restart, as a session's visible set changing without
a flush.

**Confidence: high, proven by mutation.** In a throwaway worktree the coalesced tier's entities were
changed from `&[7, 9]` to `&[7]`: green. The tier list was then replaced with `&[]` — no coalesced
tier at all: still green, 1/1. The assertion compares the first fragment with itself.

**Disposition:**

### F3 — the nested-list refusal is asserted by `is_err()`, which cannot tell it from running out of bytes

**Claim:** `a_nested_or_overrunning_list_refuses` covers records §5's "the multi model is one level
deep". Deleting the depth guard leaves it green.

**Evidence:** `crates/tessera-filter/src/record.rs:1063`, `assert!(decode(&nested, 1).is_err());`
over the crafted payload `[0, 0, KIND_LIST, KIND_LIST, 1, 0, 0, 0]`. The guard under test is
`crates/tessera-filter/src/record.rs:375`, `if !lists_allowed { return Err(malformed("a list element
is itself a list; …")) }`. With the guard bypassed the decoder reads the inner list's element kind
from a payload that has none and refuses on truncation instead — a different error, same
`is_err()`. The test's own sibling in the same body (the overrun case, `:1071`) and its neighbour
`a_bool_byte_past_one_refuses` (`:975`) both check the message; this one does not.

**Class:** under-discriminating. **Severity: S4.** The list family is unbuilt and refuses at encode
(`encoding_a_list_refuses`, `:1008`), so no writer in the repository can produce either shape; the
guard is a decode-side bound on recursion depth, and a defect there is a crash on a crafted bundle
rather than a disclosure.

**What a defect would let through:** unbounded recursion in `decode_value` over a bundle artefact
that nests lists deeply enough — a stack overflow rather than a refusal.

**Confidence: high, proven by mutation.** `decode_value(payload, cursor, elem_kind, false)` became
`… , true)` and all eleven `record::tests` stayed green.

**Disposition:**

## The `is_err()` seam — which of them matter

Measured across the three crates: **fifteen** assertion sites on `is_err()`/`is_ok()` in
`tessera-filter` (nine in `tests/dict.rs`, six in `crates/tessera-filter/src/record.rs`), two in `tessera-authz`
(`tests/delta_tier.rs:77`, `:80`), and five in `tessera-filter-write`. (The brief's count of twelve
for `tessera-filter` differs from mine; I report what I measured and did not reconcile the two, since
the disposition is per site.) The repository's good form is on this surface —
`crates/tessera-authz/tests/fragment.rs:437` matches `Err(FragmentCacheError::Io(_))` and follows it
with two state assertions — and the comparison holds up: **one of the twenty-two matters, and it is
F3.**

The rest do not, for four reasons that are worth recording so they are not re-argued:

- **The kind is checked three lines away, on the same fault.**
  `tests/dict.rs:804`'s `assert!(dict.self_check().is_err())` closes
  `walk_ordinals_refuses_a_damaged_block_and_no_other`, whose preceding loop already asserts
  `.contains("restart shares nothing")` on the same doctored block.
  `crates/tessera-filter-write/tests/record_blob.rs:289`, `:303`, `:355` are the same shape — each follows a
  `matches!(err, RecordError::Malformed(_))` plus a message check on the identical corruption.
- **Only one error is reachable.** `tests/dict.rs:453` sweeps *every* prefix of a good file and
  `:461` appends one byte; `tests/delta_tier.rs:77`/`:80` feed the writer descending and duplicated
  term ids, which its one precondition check rejects. `crates/tessera-filter/src/record.rs:986`'s invalid-UTF-8 payload is
  byte-exact — tag, kind, a length of 2 and exactly two bytes — so the UTF-8 validation is the only
  thing that can fail. These are non-discriminating in *form* and discriminating in *fact*.
- **The test says why a kind check would over-specify.** `tests/dict.rs:528`'s comment is explicit:
  "The block no longer tiles its extent, or the suffix is empty — either way it refuses." Pinning one
  of two legitimate refusals would be a worse test.
- **The load-bearing arm is kind-checked and the bare one is the trailing extra.**
  `tests/dict.rs:648` is the zero-length file in `a_short_file_refuses_before_it_is_mapped`; the
  two-byte case three lines above asserts `.contains("minimum")` for both access modes, which is the
  half where a missing length check would mean an out-of-bounds footer read. A zero-length mapping
  fails at `Mmap::map` regardless, so `is_err()` there could not distinguish the routes and does not
  need to.

`tessera-authz`'s own error-kind discipline is better than either: `FragmentCacheError` has three
variants and the tests match on them — `Building` at `tests/fragment.rs:338` inside the
single-flight retry loop, `Io(_)` at `:437`, and `SingleFlightError::Build("boom")` by value at
`crates/tessera-authz/src/single_flight.rs:706`.

## I5 — the honest position is unchanged, and nothing here can be even a partial oracle

**Verdict: `conformance.md` §4.6's "not covered" is still exactly right, and the reason is
structural rather than a testing gap on this surface.**

I5 is the agreement of an authorisation plugin's *two* functions — the data function normalising an
item's expression, and the auth function deciding which normalised terms a credential satisfies.
Neither function is on this surface and neither is reachable from it. `tessera-authz` receives
`satisfied: &[TermId]` already resolved and never sees a credential, a descriptor's semantics, or an
item's label expression; `build_fragment`, `FragmentCache::get_or_build` and `sweep_term_postings`
all take the term list as given. The only plugin that exists is the passthrough, which lives in
`tessera-corpus` and `tessera-plugin`, and for which both functions are the same string comparison —
so there is nothing here two implementations could disagree about.

The nearest thing to an oracle on this surface is
`crates/tessera-authz/tests/fragment.rs:57`'s `build_fragment_matches_brute_force_union`, which
compares the compressed-postings union against a `HashSet<u32>` union over twenty random grants. That
is a genuine independent oracle and worth keeping in view — but it is an oracle for **I1's mask
derivation** (the same property the Python suite's pair-relation differential covers), not for I5.
It takes the term set as input, so it is blind to the question I5 asks.

`core-access-expressions.md` is **draft r1 and nothing in it is built** (its own status line), so it
imposes no test obligation today. If its expr §6 ruling lands, the evaluator moves into the core and
I5's semantic half becomes differentially checkable — at which point this surface acquires the
obligation it does not have now. **Nothing moved and nothing should move.**

## I13a — the incidental coverage would catch a fail-open here

**Verdict: on this surface I13a's coverage is by design rather than incidental, and every
fail-open route I could name is pinned by a named test.** §4.6's "covered incidentally, not by the
designed test" is true of the *conformance suite's* missing asymmetry test; read as a statement
about the authorisation crate it understates what is there. I did not move the row.

The two clauses, each with the test that carries it:

- **A waiter must never observe a half-built artefact as complete.**
  `crates/tessera-authz/src/single_flight.rs:636`'s `concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild`
  parks a build on a channel handshake, asserts the concurrent arrival gets
  `Err(SingleFlightError::Building)` and not a value, and — the part that makes it bite — passes a
  build closure that `panic!`s, so a losing arrival that built anything fails loudly.
  `tests/fragment.rs:308` drives the same claim through eight racing OS threads at the cache level
  and asserts `Arc::ptr_eq` across all eight results plus `rebuild_count() == 1`.
- **No failure is ever cached, and no slot is left wedged.** Both forms:
  `crates/tessera-authz/src/single_flight.rs:701` (returned `Err`) and `:721` (unwinding panic), each asserting
  `cache.len() == 0` afterwards **and** that the next call succeeds — so a permanently fail-closed
  wedge fails the test as surely as a cached success would.
  `tests/fragment.rs:422` is the same at the `FragmentCache` level through a real `ENOTDIR`, with
  the kind-checked `matches!(result, Err(FragmentCacheError::Io(_)))`, `slot_count() == 0`,
  `rebuild_count() == 0`, and a repair-and-retry that must succeed.

**Would a stale or empty fragment served instead of a refusal be caught? Yes, by four independent
tests.** A cache entry keyed under a superseded bundle identity: `tests/fragment.rs:167`. Under a
superseded auth plugin hash: `:201`. Under a superseded tier set: `tests/fragment_cache_key.rs:35`,
which asserts both that the two disk paths differ *and* that the two fragments genuinely differ by
entity `9`, so the collision would have mattered. Carried across a fold: `tests/fragment.rs:476`
asserts a rotated cache has no slots, that the memo does not survive either (the case with no lookup
that could miss), and that the byte bound *does* survive. A corrupt-but-right-length entry:
`tests/fragment.rs:241` flips a byte and asserts the rebuild happens and the rebuilt set is correct.
An *empty* fragment served for a non-empty grant would fail
`build_fragment_matches_brute_force_union` on the first of its twenty grants.

The one thing on this surface that a fail-closed reading leaves open is not a test gap:
`FragmentCacheError::Building` is returned to the engine, and whether the engine turns it into a
refusal or into something weaker is the engine's own obligation, in another track's surface.

## What was checked and found sound

Kept so the attacks are not re-run.

- **The mask fixtures are not vacuous.** Checked one by one.
  `crates/tessera-authz/tests/fragment_deltas.rs:55` grants a strict subset and asserts the unauthorised term's entities
  absent (I2 in its most direct form). `crates/tessera-filter/src/values.rs:1347` admits three of five entities and asserts
  the two carrying the value but outside the candidate do not appear.
  `crates/tessera-filter/src/column.rs:332`'s narrowing loop asserts `narrowed.andnot(candidate).is_empty()` for every
  value against every candidate — the answer never names an entity the candidate did not — and its
  candidate set includes the empty bitmap, a singleton, a scattered set and one naming an entity no
  posting holds. No fragment fixture uses a credential covering the whole universe.
- **Zero-assertion bodies: there are none.** Every test body in the three crates asserts directly or
  through a helper that asserts (`both`, `assert_spool_matches_buffered`, `run_sweep`'s callers,
  `malformed`).
- **The fold's byte-identity claim (`filter-index` §6.2) is asserted on bytes, not readbacks**, in
  four independent places: `crates/tessera-filter-write/src/lib.rs:615`,
  `crates/tessera-filter-write/src/keyword.rs:825` and
  `crates/tessera-filter-write/src/record.rs:571`,
  and `tests/values_writer.rs` for the streaming/whole-column writer pair
  across every fixed-width family, five chunkings, the empty column and the partial one.
  `tests/streaming_writers.rs` and `tests/keyed_spool.rs` do the same for the authorisation side,
  and `crates/tessera-authz/tests/keyed_spool.rs:33` sweeps **every** partition of the key space into three consecutive bands
  so the writer cannot tell where a band ended.
- **The term sweep is the best-documented code in this surface.** `tests/term_sweep.rs` carries a
  per-test *mutation* paragraph naming the exact defect the body kills — "delete `union -=
  tombstones` and every one of 2, 12, 21 survives somewhere", "loop `0..base.term_count()` instead
  of `0..dict_len` and this produces 2 records, not 4". It covers the base-only, tier-only and
  both-arms cases separately because they take different code, includes a tombstoned entity present
  in no posting, and pins `dict_len` written through verbatim so an emptied term keeps its ordinal.
- **`tessera-filter`'s scan is checked against a literal per-entity definition, not another route
  through the same code.** `crates/tessera-filter/src/values.rs:1724` compares the packed 2¹⁶-block path with
  `candidate.iter().filter(…)` across three densities and six candidate shapes chosen for the
  seams; `:1775` pins the trailing partial block from both directions; `:1668` walks past `CHUNK`
  and `RUN_MIN` in three shapes because "the small fixtures reach only one of the three paths",
  which is a non-vacuity guard in the engine's own idiom.
- **`tests/dict.rs` re-derives the footer independently of the reader** (`Layout::of`), stated at the
  file's top as deliberate redundancy: a test that asked the reader where the restart table is could
  not tell a moved table from a moved reader. Every fail-closed case damages a named byte.
- **`crates/tessera-filter-write/tests/record_blob.rs` covers review B6 at the level the digest cannot.** A directory offset
  redirected at a neighbour's row refuses on the discriminant with the message checked, and the
  *victim* row is asserted to answer as itself or refuse — never as anything else.
- **I4's entity-space rule is enforced mechanically, not by a test, and correctly so.**
  `scripts/check-layers.sh:12`–`:32` denies all three crates a direct edge to `tessera-store` and
  `tessera-spatial`, with the reason stated inline (`filter-index` §9 — a crate that can see a
  `RowId` can relate the two ID spaces). The check is direct-edge-only by design, documented at
  `scripts/check-layers.sh:3`. Nothing in the crates duplicates it and nothing should.
- **Four modules have no in-crate tests and three of them are covered from outside.**
  `tessera-filter`'s `extent.rs` is exercised by `tessera-engine`'s `flush.rs` unit tests and
  `tests/filtering.rs`, including the claim that matters — an extent's presence bitmap is written
  unconditionally so it can never be read positionally: `write_extent` passes `Some(presence)` as a
  literal and `crates/tessera-engine/src/flush.rs:1607` reads the reopened extent back by entity id
  for a sparse, non-zero-based set (3, 5, 9), which a positional read would answer wrongly; `record_stack.rs` by `crates/tessera-filter-write/tests/record_blob.rs:386` (a base plus two interleaved
  extents, plus a truncated extent refusing the whole stack rather than downgrading) and by
  `tessera-engine`'s `tests/attribute_tail.rs`; `tessera-filter-write`'s `text.rs` by
  `tests/fold_text.rs` and `tests/coalesce_text.rs`. The fourth is F1.
- **Two doc claims are broader than their bodies, and neither is worth a finding.**
  `crates/tessera-filter/src/values.rs:1345` says the excluded entity "never contributes work either" and the body asserts
  only the result; `crates/tessera-filter-write/tests/record_blob.rs:443` says the set read "decompresses each block once" and
  the body asserts only that it agrees with the single reads it replaces. Both are performance
  claims beside a correctness claim that *is* asserted, and neither names an Appendix C row.

## Appendix R — review trail

- **r1 (2026-08-30)** — first pass. Base `2bde89a6`. Method: every test read against
  `filter-index.md` §2, §5–§6, §9, `filter-surface.md` §4–§5, `records-and-search.md` §3–§5, §7,
  `compaction.md` §3, `write-path.md` §5.4, §7, architecture §4 (I1, I2, I4, I5, I13a) and Appendix C
  (C8, C11, C24–C26), plus decisions 0042, 0048, 0063, 0066. Three mutations, all in one throwaway
  git worktree removed on completion; nothing committed and no file in the repository edited but this
  memo. Nothing was escalated mid-audit: **no live defect in shipped code was found**, and no route
  by which a mask could be widened or a fragment served fail-open. F1 is a missing test, not a bug —
  `coalesce_dict_extents` is correct as written and its caller replaces a contiguous range in place.
  `core-access-expressions.md` was read and found to bind nothing (draft r1, nothing built), so it
  raised no ambiguity question.
