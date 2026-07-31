"""`AckedJournal` — the fixture-owned journal of **acked** control operations.

Conformance design §1 gives the oracle exactly two inputs, and says the distinction is
load-bearing: the bundle (read only through the contracts spec) for build-time state, and *this*
for runtime state — because predicate changes and unflushed entities live in the overlay and the
WAL, which are out of contract and invisible in any bundle file.

## The one rule

**Only a 200 is journalled.** An operation that was *submitted* is not an operation that was
*acked*, and the I1 differential is only entitled to assume the latter. Everything else — a 409
duplicate, a 422 contract error, a 503 fail-closed, a connection that died mid-flight — is
recorded in [`AckedJournal.refused`] for a test to assert on, and contributes **nothing** to the
composed mask. The asymmetry is deliberate and is the whole point of the type: a journal that
recorded intent would let the oracle expect a deny the service never promised, and the resulting
red test would be blamed on the engine.

`refused` exists rather than the calls simply raising because the conformance suite has to assert
on refusals directly — a 409 with the offending IDs in `detail` and **no effect on the batch** is
a contract clause (contracts §3.4), so the suite must be able to submit something that will be
refused and then check that nothing moved.

## Acked is not the same as applied

For **overlay changes** the two coincide, and the ack contract is what makes them coincide: the
200 follows the generation swap (lifecycle §4), so the caller's next query observes its own change
and no barrier is needed. Conformance design §1 says exactly this, and says barriers cover the
asynchronous operations only.

For **ingest** they do not. `POST /control/ingest` returns 200 after WAL fsync; the rows are
buffered, and stage 2.1's Task 7a puts them in a **commit window** that may be held open. A batch
can therefore be acked — durably, irrevocably accepted — and still not be in anybody's visible
set. [`IngestOp.applied`] models that state, `False` until [`AckedJournal.barrier`] observes the
watermark move past it. Collapsing the two would make the journal assert that a held batch is
visible, which is a differential failure against a correct engine.

Flush and compaction are `202`-async in the same way, and take the same barrier.

## What it does not model

The three retirement rules (lifecycle §3) are the *engine's* obligation, not this journal's:
deletion denies retire by the epoch ledger, suppressions retire only on unsuppress, and
predicate-change entries retire at their compaction fold. This journal records what was acked; it
never expires an entry on its own, because from the viewer's side a retired deny and a live deny
are indistinguishable — both mean "not visible" — and a journal that expired entries by its own
clock would silently start expecting the item back.
"""

from __future__ import annotations

import base64
import time
from dataclasses import dataclass

import requests

from .bundle import Bundle
from .mask import ChangeSet

# The four `/control/changes` operations (contracts §3.4). `predicate` additionally carries an
# `access` label, which the plugin turns back into descriptors.
CHANGE_OPS = ("delete", "suppress", "unsuppress", "predicate")


@dataclass(frozen=True)
class ChangeOp:
    """One acked `/control/changes` item."""

    sequence: int
    entity_id: int
    op: str
    term_ids: frozenset[int] | None = None

    def __str__(self) -> str:
        terms = "" if self.term_ids is None else f" -> {sorted(self.term_ids)}"
        return f"#{self.sequence} {self.op} entity {self.entity_id}{terms}"


@dataclass
class IngestOp:
    """One acked `/control/ingest` batch.

    `applied` is `False` on arrival and only ever set by [`AckedJournal.barrier`]. See the module
    doc: a 200 from ingest is a durability statement, not a visibility one.
    """

    sequence: int
    batch_id: str
    accepted: int
    tessera_ids: tuple[int, ...] = ()
    # The `entity_id_high_water` the barrier waits for: what it was before the call, plus the
    # rows the service said it accepted. Captured per batch because that is the only quantity
    # `/control/status` exposes that moves with an ingest at all (see `barrier`).
    required_high_water: int = 0
    applied: bool = False


@dataclass(frozen=True)
class Refusal:
    """A control call that was **not** acked. Never contributes to the composed mask."""

    sequence: int
    what: str
    status: int
    detail: str


class AckedJournal:
    """Drives the control plane and records only what it acked.

    Every mutating call goes through this object rather than through `Server` directly, so there is
    exactly one place where "did the service accept this?" is decided, and it is decided by the
    status code the service actually returned.
    """

    def __init__(self, server, bundle: Bundle):
        self.server = server
        self.bundle = bundle
        self.ops: list[ChangeOp] = []
        self.ingests: list[IngestOp] = []
        self.refused: list[Refusal] = []
        self._sequence = 0

    # -- submission ------------------------------------------------------------------------------

    def change(
        self,
        entity_id: int,
        op: str,
        *,
        term_ids: set[int] | None = None,
        external_id_b64: str | None = None,
    ) -> requests.Response:
        """Submit one `/control/changes` item, journalling it **iff** the service returns 200.

        `predicate` takes `term_ids`, the item's *new* term set; the `access` label is rebuilt from
        the bundle's own dictionary, because `builtin:passthrough`'s label is the comma-joined
        descriptors and the descriptors are what the dictionary holds. Deriving it here rather than
        at every call site keeps one place that knows the plugin's label grammar.

        `external_id_b64` overrides the bundle lookup, for the one case a test needs it: naming an
        external ID the deployment has never seen, which must be refused. A refusal has no entity
        to journal, so nothing is journalled — but a *200* to such a call would leave this object
        with an operation it cannot attribute, so it raises rather than recording a fiction.
        """
        if op not in CHANGE_OPS:
            raise ValueError(f"unknown change op {op!r}; expected one of {CHANGE_OPS}")
        if op == "predicate" and term_ids is None:
            raise ValueError("a predicate change must state the item's new term set")

        access = None
        if op == "predicate":
            access = ",".join(
                self.bundle.dictionary[t].decode("ascii") for t in sorted(term_ids or set())
            )
        supplied = external_id_b64
        if supplied is None:
            supplied = base64.b64encode(self.bundle.external_id_of(entity_id)).decode()
        response = self.server.change(supplied, op, access)

        if response.status_code == 200 and external_id_b64 is not None:
            raise AssertionError(
                f"the service accepted a {op} against external id {external_id_b64!r}, which this "
                "journal has no entity for; it cannot be composed and must not be silently dropped"
            )

        self._sequence += 1
        if response.status_code == 200:
            self.ops.append(
                ChangeOp(
                    sequence=self._sequence,
                    entity_id=entity_id,
                    op=op,
                    term_ids=None if term_ids is None else frozenset(term_ids),
                )
            )
        else:
            self.refused.append(
                Refusal(
                    sequence=self._sequence,
                    what=(
                        f"{op} external id {external_id_b64}"
                        if external_id_b64 is not None
                        else f"{op} entity {entity_id}"
                    ),
                    status=response.status_code,
                    detail=response.text[:500],
                )
            )
        return response

    def changes(self, items: list[tuple[int, str, set[int] | None]]) -> requests.Response:
        """Submit a whole `/control/changes` batch in one request.

        **All or nothing, on the service's own terms.** Contracts §3.4 makes a duplicate a `409`
        with "the batch has no effect", so a non-200 journals nothing at all here — not even the
        items that would individually have been fine. A per-item journal on a refused batch is the
        exact fail-open this type exists to prevent.
        """
        payload = []
        for entity_id, op, term_ids in items:
            item: dict = {
                "external_id": base64.b64encode(self.bundle.external_id_of(entity_id)).decode(),
                "op": op,
            }
            if op == "predicate":
                item["access"] = ",".join(
                    self.bundle.dictionary[t].decode("ascii") for t in sorted(term_ids or set())
                )
            payload.append(item)

        response = self.server.changes(payload)
        if response.status_code == 200:
            for entity_id, op, term_ids in items:
                self._sequence += 1
                self.ops.append(
                    ChangeOp(
                        sequence=self._sequence,
                        entity_id=entity_id,
                        op=op,
                        term_ids=None if term_ids is None else frozenset(term_ids),
                    )
                )
        else:
            self._sequence += 1
            self.refused.append(
                Refusal(
                    sequence=self._sequence,
                    what=f"batch of {len(items)}",
                    status=response.status_code,
                    detail=response.text[:500],
                )
            )
        return response

    def ingest(self, body: bytes, batch_id: str) -> requests.Response:
        """Submit an Arrow ingest batch, journalling it **iff** the service returns 200.

        Recorded as not-yet-applied. See the module doc — and note that this is the state stage
        2.1's Task 8 makes routine rather than exotic: a batch accepted into a held commit window
        is acked and durable and still invisible, and the I1 differential has to model both facts
        at once.
        """
        before = int(self.server.status()["entity_id_high_water"])
        response = self.server.ingest(body, batch_id)
        self._sequence += 1
        if response.status_code == 200:
            payload = response.json()
            accepted = int(payload.get("accepted", 0))
            self.ingests.append(
                IngestOp(
                    sequence=self._sequence,
                    batch_id=batch_id,
                    accepted=accepted,
                    tessera_ids=tuple(payload.get("tessera_ids", ()) or ()),
                    required_high_water=before + accepted,
                )
            )
        else:
            self.refused.append(
                Refusal(
                    sequence=self._sequence,
                    what=f"ingest batch {batch_id}",
                    status=response.status_code,
                    detail=response.text[:500],
                )
            )
        return response

    # -- barriers --------------------------------------------------------------------------------

    def barrier(self, *, timeout: float = 20.0, poll: float = 0.1) -> dict:
        """Wait until `/control/status` shows the acked ingests reflected, and mark them applied.

        Conformance design §1: the fixture polls `/control/status` until the per-partition
        `segments_version`/`watermark` reflect the operation — fields the contract already exposes
        — before querying. Deliberately **not** applied to overlay changes: those need no barrier
        at all, because the 200 follows the generation swap, and a test that barriered on them
        would hide an engine that acked early.

        Phase 1's `/control/status` exposes exactly one field that moves with an ingest —
        `entity_id_high_water` — so that is what is polled, against the value captured before the
        call plus the rows the service said it accepted. It is a **proxy and is named as one**: it
        establishes that allocation happened, not that a segment was written, and in Phase 1
        allocation precedes the ack, so the wait is usually already over when it starts. That is
        the honest state of affairs rather than a barrier that only appears to do something. When
        the status shape gains a per-window field (Task 7a/8), this is the one place to change.
        """
        pending = [op for op in self.ingests if not op.applied]
        if not pending:
            return self.server.status()

        deadline = time.monotonic() + timeout
        status = self.server.status()
        while time.monotonic() < deadline:
            status = self.server.status()
            if self._reflects(status, pending):
                for op in pending:
                    op.applied = True
                return status
            time.sleep(poll)
        raise TimeoutError(
            f"/control/status did not reflect {len(pending)} acked ingest batch(es) within "
            f"{timeout}s; last status was {status}"
        )

    @staticmethod
    def _reflects(status: dict, pending: list[IngestOp]) -> bool:
        high_water = int(status["entity_id_high_water"])
        return all(high_water >= op.required_high_water for op in pending)

    # -- composition -----------------------------------------------------------------------------

    def change_set(self) -> ChangeSet:
        """The acked `/control/changes` history as a `mask.ChangeSet`, in ack order."""
        changes = ChangeSet()
        for op in self.ops:
            changes.apply(op.entity_id, op.op, None if op.term_ids is None else set(op.term_ids))
        return changes

    def resolve(self, base_mask: set[int], session_terms: set[int]) -> set[int]:
        """`M_auth` for a session holding `session_terms`, per I1's composition.

        `M_auth = (token_mask \\ L) ∪ direct_eval(L)`, then deletes and suppressions removed —
        computed in entity space, where the engine computes it as row-space diffs. Agreement
        between the two is the equivalence the I1 differential proves.

        Acked-but-unapplied ingests contribute nothing here, which is correct twice over: their
        entities are not in `base_mask` (they postdate the bundle), and until a barrier says
        otherwise they are not in anybody's visible set either.
        """
        return self.change_set().resolve(base_mask, session_terms)

    # -- introspection ---------------------------------------------------------------------------

    @property
    def acked_count(self) -> int:
        return len(self.ops) + len(self.ingests)

    def describe(self) -> str:
        """A one-line-per-entry rendering, for an assertion message. The refused entries are
        included deliberately: when a differential fails, "what did we ask for that was turned
        down?" is usually the question."""
        lines = [str(op) for op in self.ops]
        lines += [f"#{op.sequence} ingest {op.batch_id} accepted={op.accepted} applied={op.applied}" for op in self.ingests]
        lines += [f"#{r.sequence} REFUSED {r.what}: {r.status} {r.detail}" for r in self.refused]
        return "\n".join(lines)
