"""Restart-replay: deny survival and WAL idempotency across a `SIGKILL` (plan §10.3, brief step
2).

Sequence: start a server on its own (private, non-shared) WAL/cache/bundle → ingest one Arrow
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
`.ignore/tessera-system-architecture.md` (§6.2/§6.3: WAL/ack durably records allocator state
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

Also out of scope per the ledger note: the `x-tessera-slice` header (unimplemented; contracts
allow 422 on ambiguity but Phase 1 ships one slice, so this is never exercised).
"""

from __future__ import annotations

import base64
import io
from pathlib import Path

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from oracle import mask as mask_mod
from oracle.bundle import Bundle
from oracle.harness import kill_server, spawn_server, stop_server
from oracle.wire import decode_viewport

SLICE = "s0"
GRID_MAX = 65536.0
BATCH_ID = "conformance-restart-replay-batch-1"


def _build_ingest_batch(*, access: str = "999002") -> bytes:
    """One small Arrow IPC stream, schema `(external_id: binary, x: float32, y: float32,
    access: utf8)` (R5) — three brand-new items, external ids well outside the fixture's own
    source-id range (which is `< 250_000`, R4's build-with-`--limit`), so there's no collision.
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
        ]
    )
    batch = pa.record_batch(
        [
            pa.array(external_ids, type=pa.binary()),
            pa.array(xs, type=pa.float32()),
            pa.array(ys, type=pa.float32()),
            pa.array(accesses, type=pa.utf8()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


@pytest.fixture
def restart_paths(tmp_path):
    """A private tmp dir for this test's cache/WAL, so an actual restart-on-same-state is
    possible (the shared session-scoped `server` fixture's tmp dir is not reusable this way)."""
    return {
        "root": tmp_path,
        "cache": tmp_path / "cache",
        "wal": tmp_path / "wal.log",
    }


def test_deny_ops_and_ingest_survive_a_sigkill_restart(bundle_root: Path, restart_paths):
    oracle_bundle = Bundle(bundle_root)
    cache_dir = restart_paths["cache"]
    wal_path = restart_paths["wal"]
    tmp_dir = restart_paths["root"]

    srv, proc = spawn_server(bundle_root, tmp_dir, cache_dir=cache_dir, wal_path=wal_path)

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
        term0 = 0
        base_mask = mask_mod.mask_of({term0}, oracle_bundle.pairs_path())
        assert len(base_mask) >= 3, "fixture must have >= 3 term-0 members for this test"
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
        resolved_mask_before = changes.resolve(base_mask, {term0})

        dictionary = oracle_bundle.dictionary
        auth = srv.authorise([dictionary[term0].decode("ascii")])
        token = auth["token"]

        bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
        zoom = 4
        raw = srv.viewport(token, SLICE, zoom, bbox, k=200)
        tiles, _points = decode_viewport(raw)
        counts_before = {t: v for t, v, m, _s in tiles}

        from_oracle_before = _oracle_counts(oracle_bundle, resolved_mask_before, SLICE, zoom, bbox)
        assert counts_before == from_oracle_before
        assert sum(counts_before.values()) == len(base_mask) - 3, (
            "three denies (1 delete + 2 suppress) must each remove exactly one member"
        )

        # --- SIGKILL: no graceful shutdown --------------------------------------------------
        kill_server(proc)
        proc = None  # already reaped by kill_server

        # --- restart on the SAME wal/cache/bundle, nothing re-submitted ---------------------
        srv2, proc2 = spawn_server(bundle_root, tmp_dir, cache_dir=cache_dir, wal_path=wal_path)
        try:
            status_after_restart = srv2.status()
            assert status_after_restart["entity_id_high_water"] == high_water_after_ingest, (
                "allocator high-water mark must be unchanged by a from-scratch WAL replay — see "
                "module doc on why this is the ingested batch's replay evidence"
            )

            auth2 = srv2.authorise([dictionary[term0].decode("ascii")])
            token2 = auth2["token"]
            raw2 = srv2.viewport(token2, SLICE, zoom, bbox, k=200)
            tiles2, _points2 = decode_viewport(raw2)
            counts_after_restart = {t: v for t, v, m, _s in tiles2}

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


def _oracle_counts(bundle, mask, slice_id, zoom, bbox):
    from oracle import viewport as vp

    return vp.counts(bundle, mask, slice_id, zoom, bbox)
