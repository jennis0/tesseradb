"""Restart-replay: deny survival, WAL idempotency, and replay across a simulated power loss
(plan §10.3; conformance design §5).

**Two tests, and what neither of them proves.** A SIGKILL loses nothing: the page cache survives
process death, so the bytes a process wrote are still there for the next process to read whether or
not anyone fsynced them. A kill-and-restart test therefore verifies *replay logic* — and **an engine
that acked before it fsynced would pass it**. Conformance design §5 says so in those words.

`test_no_acked_operation_is_lost_when_the_unsynced_tail_is_discarded` is §5's truncating variant.
After the kill it **truncates the WAL to its last-synced offset** before restarting, which is what a
power loss would have done, and then asks the same survival questions. The offset comes from the
WAL's own sidecar — one per sequence member, `wal-000001.log` → `wal-000001.sync`, an 8-byte
little-endian offset into that member written write-tmp-then-rename and fsynced together with its
directory entry after every WAL fsync, because replay already needs it
(`tessera-lifecycle/src/wal.rs`, "the durable prefix"; decision 0038). The configured `wal` path
names the family, not a file, so the test derives the active member rather than opening it. §5
specifies an `fsync_offset()` introspection command behind a `conformance` cargo feature to supply
this number; none of that is needed, because the number is already on disk.

# What this module does NOT establish, stated first because it was claimed and is false

**Neither test falsifies ack-before-fsync, and `discarded == 0` in particular does not.** That
assertion compares the engine's own published offset against the file size. It catches an engine
that forgets to advance the sidecar. It does **not** catch one that advances the sidecar without
syncing — which is the same code shape an early-acking implementation naturally has.

Measured, not reasoned: replacing `self.file.sync_data()` with `Ok(())` in `Wal::sync_and_publish`,
so the WAL is **never fsynced** while the offset is still published and acks still return 200,
leaves both tests in this module passing. The sidecar is the same component's bookkeeping, merely
persisted, and a test that reads it is taking the engine's word for the very thing under test.
Decision 0038 originally argued the opposite and has been corrected.

**Nothing black-box can close this**, because fsync ordering is not observable through the API.
Durability ordering is held where it always was: by the write path's `Published` token type, which
makes the ack path unwritable in the wrong order, and by the fault-injection pause site inside the
ack function (lifecycle §4, §7.3). Issue #71 tracks whether an end-to-end check is worth building.

# What this module does establish

- **Replay is correct when the unsynced tail is discarded.** On a green run `discarded == 0`, so the
  truncation removes zero bytes and the remainder is byte-for-byte the SIGKILL path — but the
  assertion is what makes that a *finding* rather than an assumption, and the test would catch a
  regression that left acked operations beyond the published prefix.
- **The sidecar is live.** The offset must advance across the acked denies. A sidecar written once
  at open and never updated would make the first assertion vacuously true for ever; stubbing
  `Wal::fsync` to leave `durable_len` alone fires it (`assert 6 > 6`).
- **The truncated bytes are load-bearing.** Truncating 40 bytes *below* the sync point destroys
  bytes a caller was told were durable, and the server refuses to start: `refused to start: wal
  error: wal corruption before the last-fsynced offset — acked state may be damaged`. So the
  survival assertions cannot pass against a log that quietly lost part of its durable prefix.

Because power loss is simulated by truncation rather than depended on, this may run on any
filesystem including tmpfs — recorded per §5, so the first flake does not relitigate it.

**`discarded == 0` is scoped to the whole file, and that is a workload assumption.** Nothing else
writes this WAL between the last ack and the measurement, so on this workload the assertion is
sound. It is *not* robust to group commit or any background WAL writer: either would leave
legitimate unsynced bytes past the sync point, the assertion would fire, and it would read as a
spurious failure whose natural "fix" is to weaken it. If that day comes, the right change is to
scope the check to the acked records' own extent, not to delete it.

The first test's sequence: start a server on its own (private, non-shared) WAL/cache/bundle → ingest one Arrow
batch of new items over the control plane → suppress two built-in fixture items and delete a
third → assert all three are now invisible → `SIGKILL` (not a graceful terminate) the process →
restart it pointed at the SAME WAL/cache/bundle, with no re-submission of anything → assert,
purely from what replay restored:

- all three items are still invisible;
- the entity-id allocator high-water mark is unchanged from its value right before the kill (this
  is also the ingested batch's replay evidence — see the note below on `/control/status`);
- replaying the *same* ingest batch id with the *same* body is still idempotent (200, and the
  high-water mark does not move again — no double-buffering).

**`/control/status`'s actual Phase 1 shape, and why "high-water unchanged" alone is weaker
evidence than it first looks (code review Important-2 on an earlier draft of this file).** The
brief describes checking that `/control/status` "shows the ingested batch's rows as buffered".
Phase 1's `/control/status` handler (`crates/tessera-server/src/control.rs`) exposes exactly one
field, `entity_id_high_water` — there is no separate buffered-row-count field, no batch-id list,
and no flag anywhere distinguishing "this response reflects a WAL replay" from "this response
reflects a freshly recorded batch" (confirmed by reading every route `control.rs` registers).

**Why a *direct* viewport-visibility check (does the ingested item show up on a subsequent
`/v1/viewport`?) is impossible in Phase 1, not merely inconvenient.** Investigated before choosing
the evidence below, per the review's instruction. `crates/tessera-engine/src/compose.rs`'s mask
composition walks the ingest buffer looking for a row via `Permutation::row_of(entity)`, and its
own comment states Phase 1 buffered entities have no row anywhere (no flush/build has happened
yet) — the "no row" branch is always taken, so a buffered item contributes nothing to any
viewport's mask, however it is authorised. `tessera-lifecycle/src/buffer.rs`'s module doc says the
same thing directly: "a buffered item simply has no `Permutation::row_of` entry anywhere, so it
can never contribute [to a viewport]." This is stated as intentional design in
`docs/design/system-architecture.md` (§6.2/§6.3: WAL/ack durably records allocator state
immediately; spatial/viewport visibility only arrives once a flush/build produces row geometry —
"the catch-up window is the build duration; visibility latency during it degrades gracefully
rather than data being lost"). So there is no HTTP-observable surface where "does this ingested
item now render" is even a coherent question to ask in Phase 1 — testing it would be testing a
capability the design deliberately doesn't have yet, not testing replay.

**The strongest evidence actually available, and what it does and doesn't prove.** Two
behavioural checks, both HTTP-only:
1. `entity_id_high_water` growing at ingest time and then staying *exactly* put across the
   kill/restart (not reset lower, not re-incremented) — entity-id allocation is monotonic and
   append-only (I9: ids are never reused), so if the ingest record had been dropped by replay, the
   high-water mark after restart would revert to its pre-ingest value, strictly lower than what
   was observed right after the original ingest. Exact numeric equality across a from-scratch
   restart is therefore real (if partial) evidence the record survived, not just "no crash".
2. **New in this fix:** re-posting the *same* batch id with a **different** body after restart
   must return `409 Conflict`, not `200`. `control.rs`'s idempotency check is keyed on
   `(batch_id, body_hash)` (`session.rs`'s `accepted_batches` map); a `409` is only possible if the
   *original* body's hash specifically was recovered by replay, not merely a high-water checkpoint
   number. This closes exactly the gap the review named: an implementation that persists
   `entity_id_high_water` as an independent durable counter but drops the itemised WAL ingest
   record (and therefore the batch-id → body-hash map) would pass check 1 by coincidence but fail
   check 2, because a dropped record makes the batch id look unseen, and an unseen batch id with a
   *different* body is accepted fresh (`200`), not rejected.

This remains inferential — a maximally adversarial implementation could theoretically special-case
`accepted_batches` durability separately from everything else the ingest record should have
restored (e.g. still failing to re-derive the correct entity ids for the *original*, matching-body
repost) — but no such gap is observable through any HTTP surface Phase 1 exposes; this is the
ceiling of what black-box conformance testing can assert here, and it substantially narrows the
review's "high-water alone" gap.

**Out of scope (Task 13 note, brief step 2):** deny-op WAL-append-failure fault injection (what
happens if the fsync for a `suppress`/`delete` genuinely fails) is explicitly out of scope for
Phase 1 conformance — `crates/tessera-server/src/control.rs`'s module doc describes the intended
fail-open-vs-fail-closed handling, but injecting a real fsync failure requires fault-injection
plumbing this phase doesn't have. Not tested here.

Also out of scope per the ledger note: the `x-tessera-view` header (unimplemented; contracts
allow 422 on ambiguity but Phase 1 ships one view, so this is never exercised).
"""

from __future__ import annotations

import base64
import io
from pathlib import Path

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from oracle import catalogue
from oracle import mask as mask_mod
from oracle.catalogue import catalogue_points_path, ingest_fx_keys
from oracle.harness import (
    kill_server,
    open_bundle_with_source,
    spawn_server,
    stop_server,
)
from oracle.wire import decode_viewport

VIEW = "s0"
GRID_MAX = 65536.0
BATCH_ID = "conformance-restart-replay-batch-1"


def _build_ingest_batch(*, access: str = "999002") -> bytes:
    """One small Arrow IPC stream, schema `(external_id: binary, x: float32, y: float32,
    access: utf8)` (R5) — three brand-new items, external ids far outside the fixture's own
    source-id range (the catalogue's is `< 150,000`), so there's no collision.
    `access` (the third item's access string) is a parameter so a caller can build a body that
    differs from the original under the SAME batch id, for the conflict/replay-evidence check."""
    external_ids = [
        (900_000_001).to_bytes(8, "little"),
        (900_000_002).to_bytes(8, "little"),
        (900_000_003).to_bytes(8, "little"),
    ]
    xs = [1000.0, 2000.0, 3000.0]
    ys = [1000.0, 2000.0, 3000.0]
    accesses = ["999001", "999001", access]

    schema = pa.schema(
        [
            pa.field("external_id", pa.binary()),
            pa.field("x", pa.float32()),
            pa.field("y", pa.float32()),
            pa.field("access", pa.utf8()),
            # The catalogue declares `fx_key`, and a declared column must be present in every
            # batch (contracts §2.2) — the tail is read back positionally, so an omission shifts
            # every later scalar rather than defaulting. The fixture chooses the values.
            pa.field("fx_key", pa.uint64()),
            # The catalogue's filter columns, present for the same reason. "alpha" is a declared
            # key; the values are inert here — nothing in this module filters, and attribute
            # ingest writes no artefact today (filter-index §5 ⊘).
            pa.field("department", pa.utf8()),
            pa.field("archive", pa.utf8()),
            pa.field("title", pa.utf8()),
            # The keyword column, supplied as its value: the ordinal is the flush's to assign
            # against its own extent's dictionary (records §4.3). Inert here — nothing in this
            # module filters — and required for the same positional reason as the rest.
            pa.field("submitter", pa.utf8()),
            # The render-only category and the blob-resident pair, declared by the catalogue
            # since 2026-08-12 — required for the same positional reason, inert for the same
            # reason (the flush-side blob extent is unbuilt, records §7 ⊘).
            pa.field("shelf", pa.utf8()),
            # A text column arrives at its wire type, `utf8`, like the other two string families.
            pa.field("abstract", pa.utf8()),
            pa.field("note", pa.utf8()),
            pa.field("pages", pa.uint32()),
        ]
    )
    batch = pa.record_batch(
        [
            pa.array(external_ids, type=pa.binary()),
            pa.array(xs, type=pa.float32()),
            pa.array(ys, type=pa.float32()),
            pa.array(accesses, type=pa.utf8()),
            pa.array(ingest_fx_keys(len(external_ids)), type=pa.uint64()),
            pa.array(["alpha"] * len(external_ids), type=pa.utf8()),
            pa.array(["red"] * len(external_ids), type=pa.utf8()),
            pa.array([f"ingested-{i}" for i in range(len(external_ids))], type=pa.utf8()),
            pa.array([f"relay-restart-{i}" for i in range(len(external_ids))], type=pa.utf8()),
            pa.array(["north"] * len(external_ids), type=pa.utf8()),
            pa.array(
                [f"an ingested abstract ref{i}" for i in range(len(external_ids))],
                type=pa.utf8(),
            ),
            pa.array(
                [f"ingested-note-{i}" for i in range(len(external_ids))], type=pa.utf8()
            ),
            pa.array([100 + i for i in range(len(external_ids))], type=pa.uint32()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


@pytest.fixture
def restart_paths(tmp_path, private_catalogue_bundle):
    """A private tmp dir for this test's cache/WAL **and a private copy of the bundle**, so an
    actual restart-on-same-state is possible and nothing this test denies escapes it.

    The cache and WAL are private because a restart has to be pointed at the same durable state
    twice. The *bundle* is private for a different reason: the three denies below are published
    into the bundle prefix (contracts §2.3), so a test run against the shared cached fixture leaves
    entities 0..2 suppressed and deleted for every module that reads it afterwards — including the
    next run on this machine. See `conftest.private_catalogue_bundle`."""
    return {
        "root": tmp_path,
        "cache": tmp_path / "cache",
        "wal": tmp_path / "wal.log",
        "bundle": private_catalogue_bundle(f"restart-{tmp_path.name}"),
    }


def active_member_of(wal_path: Path) -> Path:
    """The sequence member currently being appended to — `wal.log` → `wal-000002.log` if two exist.

    The configured path names a **family**, never a file (`wal.rs`, "the log is a sequence, not a
    file"; decision 0038): members are `<stem>-<n:06><ext>`, positions are sequence-global and
    offsets are per member. The active one is the highest-numbered, which is where an append lands
    and which the sidecar this module reads belongs to.

    Transcribed rather than obtained from the server, for the same reason the offset derivation was:
    it keeps the *path* out of the engine's hands. It does not keep the *offset* out of them — see
    the module doc.
    """
    members = sorted(wal_path.parent.glob(f"{wal_path.stem}-[0-9]*{wal_path.suffix}"))
    assert members, f"no member of the WAL sequence {wal_path} exists"
    return members[-1]


def sync_sidecar_of(member_path: Path) -> Path:
    """`wal-000001.log` → `wal-000001.sync`, beside it. Per member, never one for the sequence."""
    return member_path.with_suffix(".sync")


def read_sync_offset(wal_path: Path) -> int:
    """The active member's last-fsynced offset: 8 bytes, little-endian.

    An offset into that member's file, header included — so it is directly comparable with the
    member's size, and the truncation below is a truncation of that file.
    """
    raw = sync_sidecar_of(active_member_of(wal_path)).read_bytes()
    assert len(raw) == 8, f"sync sidecar is {len(raw)} bytes, not 8 — cannot be an offset"
    return int.from_bytes(raw, "little")


def test_no_acked_operation_is_lost_when_the_unsynced_tail_is_discarded(
    catalogue_bundle_root: Path, restart_paths
):
    """Conformance §5's crash-realism variant: kill, **truncate to the last-synced offset**,
    restart, and assert every acked operation is still there.

    Read the module doc first for what this does not prove. In short: it does not falsify
    ack-before-fsync, and it was claimed to. An engine whose `sync_data()` is a no-op, while it
    still publishes the offset, passes this test.

    What the two assertions do establish:

    **Nothing acked lies beyond the published prefix.** After the last 200 has been received, the
    offset the engine published must already cover every byte it wrote for the operations it acked,
    so truncating to that prefix discards nothing. Checked *before* the truncation rather than
    inferred from what survived it, because "the deny is still there" has more than one possible
    cause and "the log was already published to its end" has exactly one. This catches a regression
    that leaves acked operations past the prefix; it does not catch one that publishes a prefix it
    never synced.

    **The published offset advanced across the acked operations.** A sidecar written once at open
    and never updated would make the first assertion vacuously true for ever. Recording the offset
    before and after the denies and requiring it to move is what keeps the first assertion about
    the engine rather than about a dead file.

    Then the survival questions, against a WAL truncated to what the engine claims is durable.
    """
    oracle_bundle = open_bundle_with_source(
        catalogue_bundle_root, catalogue_points_path()
    )
    cache_dir = restart_paths["cache"]
    wal_path = restart_paths["wal"]
    tmp_dir = restart_paths["root"]
    # The engine serves the copy; the oracle keeps reading the pristine root it was opened from.
    # The two are byte-identical until the first deny lands, which is the point.
    served_root = restart_paths["bundle"]

    srv, proc = spawn_server(served_root, tmp_dir, cache_dir=cache_dir, wal_path=wal_path)

    try:
        resp = srv.ingest(_build_ingest_batch(), BATCH_ID)
        assert resp.status_code == 200, resp.text
        high_water_after_ingest = srv.status()["entity_id_high_water"]
        member_after_ingest = active_member_of(wal_path)
        sync_after_ingest = read_sync_offset(wal_path)

        # **The corpus's first block, resolved through the dictionary**, not term id `0`: that is
        # `public` now (`per-point-attributes.md` §3.8), which this corpus's points do not carry, so
        # a hard-coded `0` selects nothing at all.
        (base_term,) = catalogue.dict_terms(oracle_bundle, ["filler_head"])
        base_mask = mask_mod.mask_of({base_term}, oracle_bundle.pairs_path())
        assert len(base_mask) >= 3, "fixture must have >= 3 members in its first block"
        delete_entity, suppress_a, suppress_b = sorted(base_mask)[:3]

        def ext_b64(entity_id: int) -> str:
            return base64.b64encode(oracle_bundle.external_id_of(entity_id)).decode()

        resp = srv.changes(
            [
                {"external_id": ext_b64(delete_entity), "op": "delete"},
                {"external_id": ext_b64(suppress_a), "op": "suppress"},
                {"external_id": ext_b64(suppress_b), "op": "suppress"},
            ]
        )
        assert resp.status_code == 200, resp.text

        changes = mask_mod.ChangeSet()
        changes.apply(delete_entity, "delete")
        changes.apply(suppress_a, "suppress")
        changes.apply(suppress_b, "suppress")
        resolved_mask = changes.resolve(base_mask, {base_term})

        dictionary = oracle_bundle.dictionary
        bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
        zoom = 4
        expected_counts = _oracle_counts(oracle_bundle, resolved_mask, VIEW, zoom, bbox)

        # --- the sync point advanced, so the sidecar is live ---------------------------------
        # Both offsets must come from the same member for the comparison to mean anything: an
        # offset is per file, so a rotation between the two reads would compare two files' lengths.
        # Nothing here flushes, so nothing rotates; the assertion is what says so.
        active = active_member_of(wal_path)
        assert active == member_after_ingest, (
            f"the WAL rotated between the ingest and the denies ({member_after_ingest.name} → "
            f"{active.name}); offsets are per member, so the comparison below would be across files"
        )
        sync_point = read_sync_offset(wal_path)
        assert sync_point > sync_after_ingest, (
            "the WAL's last-synced offset did not move across three acked deny operations — the "
            "sidecar is not tracking fsyncs, so truncating to it would prove nothing about "
            "durability ordering"
        )

        # --- nothing acked lies beyond it --------------------------------------------------
        wal_len = active.stat().st_size
        discarded = wal_len - sync_point
        assert discarded == 0, (
            f"{discarded} bytes lie past the published durable prefix while every operation that "
            f"produced them had already been acked — a power loss here would lose an operation a "
            f"caller was told was durable. (Note this is scoped to the whole file: a background "
            f"WAL writer or an open group-commit window would trip it legitimately. See the "
            f"module doc before weakening it.)"
        )

        kill_server(proc)
        proc = None

        # --- what a power loss would have done ----------------------------------------------
        # `discarded == 0` above means this removes nothing on a green run, so what follows is
        # byte-for-byte the SIGKILL path. The truncation is here for the run where that assertion
        # is about to become false. (Deliberately no `st_size == sync_point` assertion after it:
        # `truncate(n)` sets the size to `n` whether it shrinks or grows the file, so it could
        # never fail.)
        with active.open("r+b") as fh:
            fh.truncate(sync_point)

        srv2, proc2 = spawn_server(
            served_root, tmp_dir, cache_dir=cache_dir, wal_path=wal_path
        )
        try:
            assert srv2.status()["entity_id_high_water"] == high_water_after_ingest, (
                "the acked ingest's allocator state did not survive the discard of the unsynced "
                "tail — it was acked out of a buffer that was never made durable"
            )

            token = srv2.authorise([dictionary[base_term].decode("ascii")])["token"]
            tiles, _points = decode_viewport(srv2.viewport(token, VIEW, zoom, bbox, k=200))
            assert {t: v for t, v, m, _s, _h in tiles} == expected_counts, (
                "an acked delete or suppression did not survive the discard of the unsynced tail"
            )
        finally:
            stop_server(proc2)
    finally:
        if proc is not None:
            stop_server(proc)


def test_deny_ops_and_ingest_survive_a_sigkill_restart(catalogue_bundle_root: Path, restart_paths):
    oracle_bundle = open_bundle_with_source(
        catalogue_bundle_root, catalogue_points_path()
    )
    cache_dir = restart_paths["cache"]
    wal_path = restart_paths["wal"]
    tmp_dir = restart_paths["root"]
    # The engine serves the copy; the oracle keeps reading the pristine root it was opened from.
    # The two are byte-identical until the first deny lands, which is the point.
    served_root = restart_paths["bundle"]

    srv, proc = spawn_server(served_root, tmp_dir, cache_dir=cache_dir, wal_path=wal_path)

    try:
        # --- ingest one batch -------------------------------------------------------------
        batch_body = _build_ingest_batch()
        resp = srv.ingest(batch_body, BATCH_ID)
        assert resp.status_code == 200, resp.text
        ingest_body = resp.json()
        assert ingest_body["accepted"] == 3

        status_after_ingest = srv.status()
        high_water_after_ingest = status_after_ingest["entity_id_high_water"]

        # --- suppress two fixture items, delete a third -----------------------------------
        # **The corpus's first block, resolved through the dictionary**, not term id `0`: that is
        # `public` now (`per-point-attributes.md` §3.8), which this corpus's points do not carry, so
        # a hard-coded `0` selects nothing at all.
        (base_term,) = catalogue.dict_terms(oracle_bundle, ["filler_head"])
        base_mask = mask_mod.mask_of({base_term}, oracle_bundle.pairs_path())
        assert len(base_mask) >= 3, "fixture must have >= 3 members in its first block"
        ordered = sorted(base_mask)
        delete_entity, suppress_entity_a, suppress_entity_b = ordered[0], ordered[1], ordered[2]

        def ext_b64(entity_id: int) -> str:
            return base64.b64encode(oracle_bundle.external_id_of(entity_id)).decode()

        resp = srv.changes(
            [
                {"external_id": ext_b64(delete_entity), "op": "delete"},
                {"external_id": ext_b64(suppress_entity_a), "op": "suppress"},
                {"external_id": ext_b64(suppress_entity_b), "op": "suppress"},
            ]
        )
        assert resp.status_code == 200, resp.text

        changes = mask_mod.ChangeSet()
        changes.apply(delete_entity, "delete")
        changes.apply(suppress_entity_a, "suppress")
        changes.apply(suppress_entity_b, "suppress")
        resolved_mask_before = changes.resolve(base_mask, {base_term})

        dictionary = oracle_bundle.dictionary
        auth = srv.authorise([dictionary[base_term].decode("ascii")])
        token = auth["token"]

        bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
        zoom = 4
        raw = srv.viewport(token, VIEW, zoom, bbox, k=200)
        tiles, _points = decode_viewport(raw)
        counts_before = {t: v for t, v, m, _s, _h in tiles}

        from_oracle_before = _oracle_counts(oracle_bundle, resolved_mask_before, VIEW, zoom, bbox)
        assert counts_before == from_oracle_before
        assert sum(counts_before.values()) == len(base_mask) - 3, (
            "three denies (1 delete + 2 suppress) must each remove exactly one member"
        )

        # --- SIGKILL: no graceful shutdown --------------------------------------------------
        kill_server(proc)
        proc = None  # already reaped by kill_server

        # --- restart on the SAME wal/cache/bundle, nothing re-submitted ---------------------
        srv2, proc2 = spawn_server(served_root, tmp_dir, cache_dir=cache_dir, wal_path=wal_path)
        try:
            status_after_restart = srv2.status()
            assert status_after_restart["entity_id_high_water"] == high_water_after_ingest, (
                "allocator high-water mark must be unchanged by a from-scratch WAL replay — see "
                "module doc on why this is the ingested batch's replay evidence"
            )

            auth2 = srv2.authorise([dictionary[base_term].decode("ascii")])
            token2 = auth2["token"]
            raw2 = srv2.viewport(token2, VIEW, zoom, bbox, k=200)
            tiles2, _points2 = decode_viewport(raw2)
            counts_after_restart = {t: v for t, v, m, _s, _h in tiles2}

            assert counts_after_restart == from_oracle_before, (
                "the delete/suppress deny ops must survive a SIGKILL + WAL replay unchanged"
            )
            assert sum(counts_after_restart.values()) == len(base_mask) - 3

            # --- idempotent replay of the SAME batch id + body ------------------------------
            resp2 = srv2.ingest(batch_body, BATCH_ID)
            assert resp2.status_code == 200, resp2.text
            assert resp2.json()["accepted"] == 3

            status_after_replay_ingest = srv2.status()
            assert (
                status_after_replay_ingest["entity_id_high_water"] == high_water_after_ingest
            ), "re-posting an already-acked batch id/body must not re-allocate or double-buffer"

            # --- stronger replay evidence: same batch id, DIFFERENT body -> 409, not 200 --------
            # (module doc, Important-2 fix: this is what actually distinguishes "the itemised WAL
            # ingest record was replayed" from "only a high-water checkpoint number was restored".
            # If the original record had been silently dropped by replay, this batch id would look
            # unseen post-restart, and an unseen id with a different body is accepted fresh (200).
            different_body = _build_ingest_batch(access="999003")
            assert different_body != batch_body
            resp3 = srv2.ingest(different_body, BATCH_ID)
            assert resp3.status_code == 409, (
                f"same batch id + different body must be rejected as a conflict — a 200 here "
                f"would mean the original batch's body hash was NOT actually recovered by WAL "
                f"replay, only its high-water side-effect was (got {resp3.status_code}: "
                f"{resp3.text})"
            )

            status_after_conflict = srv2.status()
            assert status_after_conflict["entity_id_high_water"] == high_water_after_ingest, (
                "a rejected (409) conflicting re-post must not allocate anything either"
            )
        finally:
            stop_server(proc2)
    finally:
        if proc is not None:
            stop_server(proc)


def _oracle_counts(bundle, mask, view_id, zoom, bbox):
    from oracle import viewport as vp

    return vp.counts(bundle, mask, view_id, zoom, bbox)
