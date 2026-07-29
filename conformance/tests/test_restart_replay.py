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

**`/control/status`'s actual Phase 1 shape.** The brief describes checking that `/control/status`
"shows the ingested batch's rows as buffered". Phase 1's `/control/status` handler
(`crates/tessera-server/src/control.rs`) exposes exactly one field, `entity_id_high_water`
(Important-1 fix note in that file: it used to expose this unauthenticated, which is why every
call here is bearer-gated). There is no separate "buffered row count" to read. The adaptation
made here: `entity_id_high_water` growing at ingest time and then *staying exactly put* across
the kill/restart (not reset, not re-incremented) is itself the observable proof that the ingested
batch's rows were replayed as buffered items with their *original* allocated entity ids — replay
re-uses IDs already assigned in the WAL rather than re-allocating (`tessera-lifecycle`'s WAL doc),
so a high-water mark that is unchanged after a from-scratch replay is only possible if the ingest
record was found, replayed, and its allocation honoured exactly.

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


def _build_ingest_batch() -> bytes:
    """One small Arrow IPC stream, schema `(external_id: binary, x: float32, y: float32,
    access: utf8)` (R5) — three brand-new items, external ids well outside the fixture's own
    source-id range (which is `< 250_000`, R4's build-with-`--limit`), so there's no collision."""
    external_ids = [
        (900_000_001).to_bytes(8, "little"),
        (900_000_002).to_bytes(8, "little"),
        (900_000_003).to_bytes(8, "little"),
    ]
    xs = [1000.0, 2000.0, 3000.0]
    ys = [1000.0, 2000.0, 3000.0]
    accesses = ["999001", "999001", "999002"]

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
        counts_before = {t: v for t, v, m in tiles}

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
            counts_after_restart = {t: v for t, v, m in tiles2}

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
        finally:
            stop_server(proc2)
    finally:
        if proc is not None:
            stop_server(proc)


def _oracle_counts(bundle, mask, slice_id, zoom, bbox):
    from oracle import viewport as vp

    return vp.counts(bundle, mask, slice_id, zoom, bbox)
