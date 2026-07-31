"""`AckedJournal`'s own rules, against a stub control plane.

The journal is fixture code, not the system under test, and its rules are the ones every I1 and I7
differential built on it inherits: **only a 200 is journalled**, a refused batch has no effect at
all, and **acked is not applied** for an ingest. Those are decisions this file makes, so a stub
server is the right instrument — it makes the states deterministic (a real deployment does not
produce a 409 on demand) and keeps the assertions about the journal rather than about an engine.
`conformance/tests/test_overlay_journal.py` runs the same code against a real server; the two are
complementary, and neither replaces the other.

The stub is deliberately dumb. It records what it was given and returns what it was told to
return; it models no state at all, because a stub that modelled the engine would be a second
implementation of the thing the journal exists to observe.
"""

from __future__ import annotations

import base64

import pytest

from oracle.journal import AckedJournal, IngestOp


class _Response:
    def __init__(self, status_code: int, payload=None, text: str = ""):
        self.status_code = status_code
        self._payload = payload
        self.text = text or (str(payload) if payload is not None else "")

    def json(self):
        return self._payload


class _StubServer:
    """Answers the four calls `AckedJournal` makes, with pre-arranged statuses."""

    def __init__(self, *, high_water: int = 100):
        self.high_water = high_water
        self.change_calls: list = []
        self.batch_calls: list = []
        self.ingest_calls: list = []
        self.next_change = _Response(200, {})
        self.next_batch = _Response(200, {})
        self.next_ingest = _Response(200, {"accepted": 0})

    def status(self):
        return {"entity_id_high_water": self.high_water}

    def change(self, external_id_b64, op, access=None):
        self.change_calls.append((external_id_b64, op, access))
        return self.next_change

    def changes(self, items):
        self.batch_calls.append(items)
        return self.next_batch

    def ingest(self, body, batch_id):
        self.ingest_calls.append((body, batch_id))
        return self.next_ingest


class _StubBundle:
    """Just enough `Bundle` for the journal: external ids and a term dictionary."""

    dictionary = {0: b"term-zero", 1: b"term-one"}

    @staticmethod
    def external_id_of(entity_id: int) -> bytes:
        return f"ext-{entity_id}".encode()


@pytest.fixture
def stub():
    server = _StubServer()
    return server, AckedJournal(server, _StubBundle())


def test_a_refused_batch_journals_nothing_at_all(stub):
    """Contracts §3.4: a duplicate is a 409 and **the batch has no effect**.

    So a non-200 journals nothing — not even the items that would individually have been fine. A
    per-item journal on a refused batch is the exact fail-open the type exists to prevent, and it
    is the arm no test reached: the composed mask would carry denies the service never applied,
    the differential would go red, and the engine would be blamed for it.
    """
    server, journal = stub
    server.next_batch = _Response(409, text='{"duplicate": ["ext-7"]}')

    response = journal.changes([(7, "delete", None), (8, "suppress", None)])

    assert response.status_code == 409
    assert journal.ops == [], "a refused batch put operations in the journal"
    assert len(journal.refused) == 1
    assert journal.refused[0].what == "batch of 2"
    assert journal.resolve({7, 8, 9}, set()) == {7, 8, 9}, (
        "a refused batch changed the composed mask, so the oracle now expects a deny the service "
        "explicitly did not accept"
    )


def test_an_acked_batch_journals_every_item_in_order(stub):
    """The other arm, and the label grammar the batch path builds for a predicate change."""
    server, journal = stub

    journal.changes([(7, "delete", None), (8, "suppress", None), (9, "predicate", {0, 1})])

    assert [(op.entity_id, op.op) for op in journal.ops] == [
        (7, "delete"),
        (8, "suppress"),
        (9, "predicate"),
    ]
    assert [op.sequence for op in journal.ops] == [1, 2, 3]
    assert not journal.refused

    sent = server.batch_calls[0]
    assert sent[0]["external_id"] == base64.b64encode(b"ext-7").decode()
    assert "access" not in sent[0], "only a predicate change carries an access label"
    assert sent[2]["access"] == "term-zero,term-one", (
        "the plugin's label is the comma-joined descriptors from the bundle's own dictionary"
    )

    # `predicate` with an empty term set removes the item from any session's mask via `L`, which is
    # the `\\ L` arm — not a deny, and not the same code path.
    assert journal.resolve({7, 8, 9}, {0}) == {9}


def test_an_acked_ingest_is_not_an_applied_one(stub):
    """**Acked is not applied**, and the journal has to model both facts at once.

    `POST /control/ingest` returns 200 after WAL fsync: the batch is durable and irrevocably
    accepted, and its rows are still in nobody's visible set until flush. Under §11.1's
    group-commit allocation that gap is the normal steady state of an ingesting deployment rather
    than a transient, so a journal that collapsed the two would assert a held batch is visible —
    a differential failure against a *correct* engine.
    """
    server, journal = stub
    server.high_water = 100
    server.next_ingest = _Response(200, {"accepted": 3, "tessera_ids": [11, 12, 13]})

    journal.ingest(b"arrow-bytes", "batch-1")

    assert len(journal.ingests) == 1
    op = journal.ingests[0]
    assert (op.batch_id, op.accepted, op.applied) == ("batch-1", 3, False)
    assert op.required_high_water == 103, "the barrier waits for `before + accepted`"
    assert journal.acked_count == 1
    # Acked-but-unapplied contributes nothing to the composed mask, which is correct twice over:
    # the entities postdate the bundle, and nothing has said they are visible.
    assert journal.resolve({1, 2}, set()) == {1, 2}


def test_the_barrier_waits_for_the_watermark_and_then_marks_applied(stub):
    """`barrier` is the only thing that may set `applied`, and it may only do so on evidence."""
    server, journal = stub
    server.high_water = 100
    server.next_ingest = _Response(200, {"accepted": 3})
    journal.ingest(b"arrow", "batch-1")

    server.high_water = 102  # short by one row
    with pytest.raises(TimeoutError):
        journal.barrier(timeout=0.3, poll=0.05)
    assert journal.ingests[0].applied is False, (
        "a barrier that timed out marked the batch applied anyway — the flag would then mean "
        "'we asked' rather than 'the service showed us'"
    )

    server.high_water = 103
    status = journal.barrier(timeout=1.0, poll=0.05)
    assert status["entity_id_high_water"] == 103
    assert journal.ingests[0].applied is True


def test_the_barrier_is_a_no_op_with_nothing_pending(stub):
    """Overlay changes need no barrier — the 200 follows the generation swap (lifecycle §4).

    Asserted because `barrier()` is called on journals that hold only overlay ops, and a version
    that waited on something there would hide an engine that acked early; a version that became an
    unconditional `return` would silently stop enforcing the ingest half. Both directions matter,
    so the no-op path is pinned rather than assumed.
    """
    server, journal = stub
    journal.changes([(7, "delete", None)])

    assert journal.barrier(timeout=0.0)["entity_id_high_water"] == server.high_water
    assert journal.ingests == []


def test_reflects_is_all_of_them_not_any_of_them(stub):
    """Two acked batches, one reflected: the barrier must not release."""
    pending = [
        IngestOp(sequence=1, batch_id="a", accepted=2, required_high_water=102),
        IngestOp(sequence=2, batch_id="b", accepted=5, required_high_water=107),
    ]
    assert not AckedJournal._reflects({"entity_id_high_water": 102}, pending)
    assert not AckedJournal._reflects({"entity_id_high_water": 106}, pending)
    assert AckedJournal._reflects({"entity_id_high_water": 107}, pending)


def test_a_refused_ingest_journals_a_refusal_and_no_batch(stub):
    server, journal = stub
    server.next_ingest = _Response(503, text="fail closed")

    journal.ingest(b"arrow", "batch-1")

    assert journal.ingests == []
    assert len(journal.refused) == 1
    assert journal.refused[0].status == 503
    assert journal.acked_count == 0
