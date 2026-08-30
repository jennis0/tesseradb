# Test audit R6 — `tessera-lifecycle`, `tessera-types`, `tessera-wire`

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests,
not the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect
in shipped code was found. Track R6 of the test-quality campaign; Wave 0's
[reachability memo](2026-08-30-test-reachability.md) is the input for which tests run.
`conformance.md` §4.6 is untouched — a row moves only when a test moves with it, and no test moved.

## Results

**The empty I10 test should not be a green `#[test]`, and its stated reason is false.**
`crates/tessera-wire/tests/wire.rs:210` `fn payload_bytes_never_contain_the_identity_key() {}`
argues at `:204` that the property holds structurally because "`tessera-wire` has no dependency on
the module that defines `IdentityKey` (`tessera_types::identity`) and therefore cannot construct,
hold, or serialise one in the first place". That is not so. `tessera-wire` depends on
`tessera-types` (`crates/tessera-wire/Cargo.toml`), `identity` is `pub use`d from the crate root
(`crates/tessera-types/src/lib.rs:2`), and `IdentityKey::from_hex` is public. **Measured:** a
function added to `crates/tessera-wire/src/lib.rs` that parses a key, calls `forward` on an
`EntityId` and returns the resulting bytes **compiles clean** in a throwaway worktree. The
enforcement that actually exists is `scripts/check-layers.sh:69`'s grep, which lives in CI's
`checks` job — a different job from the one that runs the test. Full verdict and recommendation in
§ *Handed item 1*.

**Durability ordering is still not falsifiable, and it is worse than the recorded non-row says.**
`conformance.md` §5's note is that a no-op `sync_data()` passes both crash tests. On this surface
the stronger statement holds: **an ack-before-fsync WAL passes all 122 `tessera-lifecycle` tests.**
Three mutations, each run alone against `cargo test -p tessera-lifecycle`, each 122/122 green:
`sync_data()` in `sync_and_publish` replaced by `Ok(())`; every `sync_data`/`sync_all`/`fsync_dir`
in `wal.rs` removed; and — the sharp one — the sidecar offset **published before** `sync_data` is
called, so a failed sync leaves an advanced offset on disk. That last is a real defect of the class
`write-path` exists to forbid, and nothing goes red. § *Handed item 2* names why no existing test
can be changed to catch it and what seam would have to move.

**The `trybuild` rows are sound in the direction that matters.** A rustc release can only turn them
**red**; it cannot turn them green while the forbidden conversion becomes possible. § *Handed
item 3*.

**Otherwise the surface is strong.** 183 tests assessed — `tessera-lifecycle` 122 executed plus one
reasoned `#[ignore]` (`crates/tessera-lifecycle/src/membership.rs:2139`), `tessera-types` 39, `tessera-wire` 22. Exactly one
empty test body in the three crates (the I10 one above); nine bare `assert!(…is_err())`, every one
checked and every one either single-variant or standing beside an exact-error sibling. The WAL and
rotation files are among the best-instrumented in the repository: seven fail-closed cases delegate
to `expect_corruption` (`tests/wal.rs:57`), which matches the error *kind* and panics on any other;
several tests carry their own non-vacuity guard in the assertion message.

Five findings. Four are **missing** or **unreachable**; one is **mis-named**. None is a claim that
shipped behaviour is wrong.

## Findings

### F1 — the I10 marker test cannot fail, and the argument it stands on is not true

**Claim:** the framed viewer-plane payload never carries the identity key (I10; `contracts §2.6`),
recorded as held structurally rather than by assertion.

**Evidence:** `crates/tessera-wire/tests/wire.rs:210`

```rust
#[test]
fn payload_bytes_never_contain_the_identity_key() {}
```

and its doc at `:204`: *"`tessera-wire` has no dependency on the module that defines `IdentityKey`
… and therefore cannot construct, hold, or serialise one in the first place"*.

**Class:** vacuous. **Severity: S1** — a disclosure surface whose Rust evidence is a body that
executes nothing, resting on a premise the compiler refutes.

**What a defect would let through:** nothing today. The grep at `scripts/check-layers.sh:69` is
real and is tight, because the only route to an `IdentityKey` is `from_hex`, which cannot be called
without naming the type. What the finding is about is where the assurance lives: the test reports
green in the I10 column while checking nothing, and if `check-layers.sh` were dropped from CI —
it runs in the `checks` job, not the `rust` job that runs this test — nothing on this surface would
notice.

**Confidence:** high. **Mutation used:** yes — a probe function constructing an `IdentityKey` and
calling `forward` was added to `crates/tessera-wire/src/lib.rs` in a throwaway worktree and
`cargo build -p tessera-wire` succeeded. Reverted; `check-layers.sh` exits 0 on the clean tree.

**Disposition:**

### F2 — an ack-before-fsync WAL passes every test in the crate

**Claim:** `write-path` and the WAL's own module doc (`src/wal.rs:37`–`:48`) hold that the sidecar
offset is advanced only by a **complete** `sync_data` + publish, so `[durable_len, len)` is exactly
the bytes no caller was told about. `conformance.md` §5 already records the crash-test half as a
non-row; this is the same property one level down.

**Evidence:** `crates/tessera-lifecycle/src/wal.rs:1471` `fn sync_and_publish`. Mutated so the
offset is written first:

```rust
let published = write_sync_offset(&self.active.sync_path, self.active.len);
if let Err(e) = self.active.file.sync_data() { self.state = WalState::Unsynced; return Err(...); }
match published { … }
```

**122 of 122 green**, across all seven binaries. The two weaker mutations (`sync_data` → `Ok(())`;
every fsync in the file removed) are also 122/122 green.

**Class:** missing. **Severity: S2** — WAL durability is on the campaign's irreversible list.

**What a defect would let through:** a suppression or a delete acknowledged to a caller, and lost
on power failure. No in-process test can observe an fsync, so the *crash* half stays with the
Python suite and issue #71. But one half **is** falsifiable in Rust and is not falsified: that a
failure of the sync half leaves the sidecar naming the old offset. **No existing test can be
changed to cover it.** The four "real fsync failure" tests (`tests/wal.rs:323`, `:429`, `:481`,
`:533`) provoke the *sidecar publish* half via a read-only directory, and each says so in its own
doc; the source states at `src/wal.rs:1977` that the `sync_data` half cannot be provoked from a
test in this tree. The fault switchboard does not reach it either: injection is at the
`ExecutorWal` wrapper (`src/wal.rs:1639`), which returns before `Wal::fsync` is called, so the real
`sync_and_publish` never runs under a fault. The one change that would make the property testable
is moving the fsync fault **inside** `Wal::sync_and_publish`, at the `sync_data` call, so a test
can assert the on-disk sidecar after a sync-half failure. That is a seam decision, not a test fix.

**Confidence:** high. **Mutation used:** yes, three.

**Disposition:**

### F3 — the frame header's byte layout is pinned by nothing

**Claim:** `contracts §3.2` r26 fixes the framing as `u8 kind ‖ u32 LE payload length ‖ payload`,
repeated. It is a cross-language contract: a second reader decodes it independently at
`clients/ts/core/src/frame.ts:94`.

**Evidence:** the writer is `crates/tessera-wire/src/payload.rs:157` `begin_frame` / `:172`
`patch_frame_len` (`len.to_le_bytes()`); the reader is `:700` `split_frames`
(`u32::from_le_bytes`). Every assertion in `tests/wire.rs` and in `payload.rs`'s own test module
reaches the frames through `split_frames` — the reader half of the same pair. Flipping both halves
to big-endian leaves **22 of 22 green** in `tessera-wire`. The one hand-built header in the crate,
`payload.rs:964` `body.extend_from_slice(&0u32.to_le_bytes())`, encodes a zero length and is
therefore byte-order-invariant.

**Class:** missing. **Severity: S3** — a real defect could pass. Not S2: nothing here is
irreversible, and no mask or gate is involved.

**What a defect would let through:** a change of endianness, or of the kind/length order, that
every Rust test accepts and that breaks the shipped TypeScript client at runtime. The two decoders
agree today by construction rather than by assertion. A five-line test asserting
`frame[0] == FRAME_TILES` and `u32::from_le_bytes(frame[1..5]) == payload.len()` closes it.

**Confidence:** high. **Mutation used:** yes.

**Disposition:**

### F4 — 22 of `tessera-types`' 39 tests do not exist under the crate's own selection

**Claim:** Wave 0 established that resolver-2 unification decides which tests a per-crate selection
runs, and reported the workspace-only set as two names in `tessera-engine`. It is larger.

**Evidence:** `crates/tessera-types/src/lib.rs:10`–`:11` gates the whole `layer` module on
`#[cfg(feature = "serde")]`, and the crate's `serde` feature is off by default. Measured:
`cargo test -p tessera-types` runs **17**; `cargo test -p tessera-types --features serde` runs
**39**. Under `--workspace` the feature is unified on by
`crates/tessera-lifecycle/Cargo.toml:23` (`tessera-types = { …, features = ["serde"] }`), a
*normal* edge, so CI runs all 39. `bash scripts/check-test-reachability.sh tessera-types` lists the
22 by name, `layer::tests::levels_must_be_dense_from_zero` among them.

**Class:** unreachable, under the per-crate selection only. **Severity: S3.**

**What a defect would let through:** nothing in CI today — the gate runs `--workspace`. What it
lets through is a developer editing `layer.rs` and running `cargo test -p tessera-types` on the
crate they are changing, who sees 17 of 39 pass and is told nothing. That is Wave 0's finding at a
larger ratio (22 of 39, against 2 of 805), and unlike `bench-timing` it has the repository's own
remedy already available: a self dev-dependency naming `features = ["serde"]`, the pattern
`crates/tessera-lifecycle/Cargo.toml` documents for `fault-injection`. Worth correcting Wave 0's
memo alongside, which reads as though the sweep had been taken over every member.

**Confidence:** high — measured twice, and reproduced by the campaign's own script. **Mutation
used:** no.

**Disposition:**

### F5 — a rotation test's doc claims a durability property the body does not check and the code does not have

**Claim:** the doc at `crates/tessera-lifecycle/tests/rotation.rs:336` states *"Records appended
but never fsynced are discarded by the rotation's own sync, exactly as a restart would discard
them — a rotation must not sweep unacked bytes into the durable prefix."*

**Evidence:** the body at `:339` `fn the_first_member_starts_its_records_at_the_header` appends one
record, fsyncs it, and asserts `end - HEADER_LEN == wal.position()` — a header-offset identity. No
rotation, no unsynced tail. And the claim is inverted with respect to the code:
`crates/tessera-lifecycle/src/wal.rs:1205` calls `sync_and_publish()` when
`self.active.len != self.active.durable_len`, so a rotation makes an unsynced tail **durable**
rather than discarding it. Born mismatched in one commit (`6f3410bb`), so this is an orphaned doc
rather than drift.

**Class:** mis-named. **Severity: S4** — weak, and only mildly misleading, because the *name*
matches the body and a reviewer greps for names. A reader of the doc, however, is told that a
durability discard is tested and true; neither holds.

**What a defect would let through:** nothing — the property described does not exist, so nothing
can violate it. The cost is a false entry in the record. The code is correct: syncing a pending
tail at a rotation is group commit, and the caller of the rotation is the same executor that owns
those appends.

**Confidence:** high. **Mutation used:** no — reading the doc against `Wal::rotate` settles it.

**Disposition:**

## Handed item 1 — the empty I10 test

**Verdict: (a), with a correction the brief did not anticipate.** It is a redundant marker that
should not be a `#[test]`, *and* the eight-line argument it carries is factually wrong and must be
rewritten rather than moved verbatim into a comment.

Three things decide it.

**The structural argument does not hold as written.** `tessera-wire` depends on `tessera-types`;
`IdentityKey` is re-exported from the crate root; `from_hex` is public. A function in
`tessera-wire/src` that constructs a key and derives a `tessera_id` from an `EntityId` compiles.
The crate *can* hold one. What stops it is the grep, not the dependency graph — and the doc names
the grep as a belt-and-braces backstop ("so a future edge cannot reintroduce the possibility
silently") when it is in fact the only enforcement. This is the same correction
`crates/tessera-types/tests/compile_fail.rs` made about `rust-toolchain.toml`: a comment that
attributes protection to the wrong mechanism makes maintaining the real one feel optional.

**A byte assertion here would buy nothing.** Option (b) fails on its own terms. To assert that a
frame contains no key material, the test must supply the key — and the frame builders take
`u64` columns and `&str` layer names, so the only way key bytes could enter is if the caller passed
them. The assertion would be about the fixture, not about the code. That is the shape
`compile_fail.rs`'s I8 paragraph already refused: *"a placeholder asserting that some stand-in type
is immutable would report green while checking nothing"*. The repository has ruled on this once,
and consistency points the same way here.

**Real byte-level I10 evidence exists and is not in this crate.** `conformance.md` records the
Python byte-scanner as built, exceeding its design on reach, and calls it the most thorough test in
the repository. I10's coverage does not depend on this `#[test]`.

**Recommended:** delete the `#[test]` attribute and the empty `fn`; keep the prose as a module- or
section-level comment in `tests/wire.rs`, rewritten so it says what is true — that
`tessera-wire`'s public API exposes no `IdentityKey`-bearing type, that nothing in
`crates/tessera-wire/src/` names the type, and that this is enforced by
`scripts/check-layers.sh:69` and by nothing else — and add the pointer to the Python scanner as
where the byte-level evidence lives. Leave `conformance.md` §4.6's I10 row alone: it does not cite
this test, and no test moves.

## Handed item 2 — durability ordering

**Verdict: still true, and the WAL tests are weaker against it than `conformance.md` §5's wording
suggests.** §5's non-row is scoped to the two restart-replay tests in the Python suite. On this
surface, all three mutations below leave `cargo test -p tessera-lifecycle` at 122/122:

| Mutation | Result |
|---|---|
| `sync_data()` in `sync_and_publish` → `Ok(())`, offset still published | 122/122 green |
| every `sync_data` / `sync_all` / `fsync_dir` in `wal.rs` removed | 122/122 green |
| **the sidecar offset published *before* `sync_data` is called** | 122/122 green |

The third is not a stub — it is a genuine ack-before-fsync defect, in-process, at the one function
whose contract is that the two happen in that order and only in that order.

**Which test would have to change: none, as the seam stands.** The four "real fsync failure" tests
(`tests/wal.rs:323`, `:429`, `:481`, `:533`) each provoke the **sidecar-publish** half through a
read-only directory and each records that distinction in its own doc; `src/wal.rs:1977` states
plainly that the `sync_data` half cannot be provoked from a test in this tree. The fault
switchboard does not reach it: `ExecutorWal::fsync` (`src/wal.rs:1639`) returns the injected error
*before* calling `Wal::fsync`, so the real `sync_and_publish` is never entered under injection.
`a_lost_writeback_is_repaired_by_rewriting_the_records` (`src/wal.rs:1984`) comes closest, but it
assembles `WalState::Unsynced` by hand and asserts the *repair*, not the ordering that produced the
state.

**What would make it falsifiable in Rust**, stated as a seam question for the owner rather than as
a fix: a `#[cfg(feature = "fault-injection")]` hook on the `sync_data` call *inside*
`Wal::sync_and_publish`, so a test can fail the sync half and then read the sidecar file off disk
and assert it still names the old offset. That is one assertion and it kills all three mutations
above. It does not close the crash half, which stays with the Python suite and issue #71 — a
process that never crashes cannot tell a written page from a synced one, and no amount of Rust
changes that.

## Handed item 3 — the `trybuild` rows

**Verdict: sound. A rustc change can turn them red; it cannot turn them green while the conversion
becomes possible.** `trybuild`'s `compile_fail` fails a case that *compiles*, regardless of any
`.stderr`, so `impl From<EntityId> for RowId` makes `entity_space_and_row_space_do_not_convert` go
red — which is what `crates/tessera-types/src/lib.rs:120`'s comment records as measured.
`TRYBUILD=overwrite` cannot launder that either: there is no stderr to record for a successful
compile, so the regeneration route reports the same failure.

The sharper question has a different answer than the one the brief was reaching for. The
green-while-permissive path is not a rustc reword; it is a `.stderr` regenerated after a case began
failing for an **unrelated** reason. Rename `AttrLocalId`, and
`tests/ui/attr_local_id_into_term_id.rs` fails at `E0432` on the import; `TRYBUILD=overwrite` will
happily record that as the expectation, and the row then passes forever while the conversion it
forbids could be added freely. Nothing mechanical prevents that. The module doc names it exactly —
*"a changed error code or a row that stopped failing is a finding … only changed phrasing is
noise"* — and that instruction is the whole control. No finding: the harness is doing what it can,
and the residual risk is documented at the site where the judgement is made.

Two smaller checks, both clean. The reverse directions (`impl From<RowId> for EntityId`, and the
`TermId`/`AttrLocalId` pair the other way) have no `trybuild` row, but `scripts/check-layers.sh`'s
I4 grep is direction-agnostic over all six newtype names, and Rust's orphan rule confines such an
impl to `tessera-types`, so the script is a complete check for that spelling. And the one genuinely
uncovered spelling — `RowId::new(e.raw() as u32)`, which the type system cannot see — is stated
honestly at `tests/ui/row_id_from_entity_id_raw.rs` and in the harness's module doc, with code
review named as its only control. Note that these four rows are among the 22 counted in **F4** only
in the sense that they share the crate; `tests/compile_fail.rs` itself is not `serde`-gated and
runs under `-p tessera-types`.

## What was checked and found sound

Kept so these attacks are not re-run.

**The WAL's recovery contract (L4).** Every clause of `src/wal.rs`'s "durable prefix" doc has a
test that would go red: corruption below the sync point (`tests/wal.rs:112`), a log shorter than
its own sync point (`:136`), a missing / zero-byte / four-byte sidecar (`:172`, `:185`, `:203`), a
corrupted length prefix on either side of the boundary (`:225`, `:247`), a record straddling the
boundary (`:359`), a bad header (`:390`), a gap in the member sequence
(`tests/rotation.rs:219`), a broken position chain (`:233`), a header number disagreeing with its
filename (`:255`), and a zero-length newest member being re-headered rather than refused (`:277`).
Seven of these delegate to `expect_corruption` (`tests/wal.rs:57`), which matches
`WalError::WalCorruption` and panics on any other error — the helper pattern the campaign's brief
names as the model, and it holds.

**The version gate carries a positive control.**
`a_log_at_the_version_before_the_coordinates_widened_is_refused`
(`tests/wal_format.rs`) refuses version 15 *and then accepts the same header at version 16*, so the
refusal is a statement about the version rather than about the rest of the header. Its sibling
`a_wal_row_round_trips_a_coordinate_no_f32_holds` asserts up front that its fixture coordinate is
one `f32` cannot hold — "or this test discriminates nothing" — before using it.

**Rotation's ordering obligations.**
`a_suppression_survives_the_reclamation_of_the_record_that_carried_it` asserts the record's own
member was actually deleted before checking the suppression survived, so "it survived" cannot pass
because nothing was reclaimed. `recovery_walks_every_surviving_member_in_order` asserts the whole
record sequence including the snapshot's position, which is what distinguishes a left-to-right walk
from a resume-at-the-snapshot optimisation.

**The overlay snapshot.** `a_snapshot_replays_in_position_and_never_displaces_what_precedes_it`
places a `Delete` *below* the snapshot precisely so the two candidate recoveries disagree; without
that record both would pass. `an_unsuppressed_entity_is_untouched_and_absent_from_the_snapshot`
pins `lifecycle §3.1`'s Rule S at the representation.

**I9, the allocator (L4).** Append-only, no reuse across a crash, and no reuse across a fold that
*lowers* the bundle's high-water are three separate property tests
(`tests/alloc_props.rs`), the third modelling all three durable homes and documenting
which half of obligation 10 it does not carry. `src/alloc.rs:412`–`:507` covers the point/row-less
ceiling, block alignment and two-mark exhaustion. The three bare `assert!(…is_err())` at
`src/alloc.rs:435`–`:437` are sound: `Allocator::try_new` has exactly one reachable error variant,
and a positive control at `ROWLESS_CEILING - 1` sits on the next line.

**Commit-window properties.** Both fragmentation tests in `tests/window_props.rs` carry explicit
non-vacuity paragraphs about the corpus they use —
`the_emitted_run_ratio_rises_with_the_window_and_stays_under_the_full_sort_ceiling` states that a
one-term-per-row corpus would make every arm report exactly `1.0` and every assertion pass while
measuring nothing, and uses two terms per row for that reason. That is the convention Wave 1 named
as worth preserving, present here independently.

**Fault-injection fidelity.** `an_injected_failure_is_indistinguishable_from_a_real_one`
(`tests/wal.rs:585`) exists to stop the engine's WAL-failure tests measuring the harness rather than
the engine, and `an_injected_append_failure_follows_the_real_sequence` (`:728`) covers the arm
nothing else reaches. Both assert the *sequence* (`Io` then `Poisoned`), both operations refusing,
and the meter not counting a failed call. This is the right shape; F2 is about a third arm neither
of them reaches, not about these.

**`tessera-types::identity` (I10's construction).** Known-answer vectors are read from
`reference/vectors/tessera_id.json`, the file the Python oracle also tests against, and asserted in
**both** directions including the `inverse_only` block; the file's `construction` and `rounds`
fields are asserted against the constants, so swapping the file to a different construction is
caught. `forward_refuses_an_entity_above_u32_max_rather_than_truncating`,
`degenerate_keys_are_refused` and `the_hex_form_is_lowercase_only_and_is_not_case_folded` each
match an exact error variant with a positive control beside it.
`debug_does_not_print_key_material` checks a hex needle *and* the presence of "redacted"; the
second assertion is what carries it, since a derived `Debug` would print decimal and slip past the
first.

**Four tests early-return under uid 0** (`tests/wal.rs:324`, `:430`, `:534`; `tests/rotation.rs:304`), because a read-only directory does not bind root. CI runs on
`ubuntu-latest` as `runner`, not in a root container, so they are not silently skipped in the gate.
Each prints a skip line rather than passing quietly.

**`HandleTable`'s eight tests exercise code nothing calls.** `crates/tessera-wire/src/handles.rs`
is retained-but-unallocated by decision 0032, and both the module doc and
`crates/tessera-server/src/state.rs:24` say so and say why. Deliberate, documented, and not a
finding — recorded only so a future auditor does not spend the same twenty minutes on it.

**`frame_bytes_never_contain_a_raw_entity_id_encoding`** (`tests/wire.rs:145`) is a real byte scan
over a real body and can fail. It builds its identity column from `HandleTable` handles rather than
from `tessera_id` values, so it exercises a shape production no longer emits — but the property it
scans for is unchanged, its own doc says exactly this, and the production-shaped scan is the Python
suite's. Not a finding.

**The nine bare `assert!(…is_err())` on this surface** were each read against the function under
test. Three (`alloc.rs`) are single-variant with a positive control; three (`layer.rs:1885`,
`:1894`, `:1904`) are `serde_json` missing-field refusals where no second failure mode is
reachable from the fixture; one (`identity.rs:481`) sits beside an exact-variant sibling in
`degenerate_keys_are_refused`; and two (`registry.rs:1723`, `:1726`) are in
`a_refused_registration_moves_nothing`, whose claim is about the *after-effects* — marks, version
and length unchanged — and not about which refusal fired. None is the R3-shaped under-discrimination
the campaign is hunting.

## Not investigated

`crates/tessera-lifecycle/src/registry.rs` declares 22 `RegistryError` variants and its own test
module exercises a handful by name. The rest are exercised from `tessera-engine`'s artifact tests,
which are track R2's surface and were audited there; enumerating that crossing properly would mean
re-reading R2's ground, so it is recorded as unexamined rather than as clean.
