"""Access expressions, black-box, against `oracle.expression_fixture`.

Items carry labels holding conjunctions, several labels each, and a quoted term. For each principal
the oracle evaluates every item's own labels with its own parser, and this module compares the
running binary with that answer: which items a bulk read returns, the `labels` column beside each,
and the item card of each returned item. A build that indexed a conjunction under the wrong key, or
a card that named a term the principal does not hold, disagrees with the oracle here.
"""

from __future__ import annotations

import io

import pyarrow.ipc as ipc
import pytest

from oracle import expression_fixture as fx
from oracle import wire
from oracle.harness import JOIN_FIELD, spawn_server, stop_server


@pytest.fixture(scope="module")
def expression_server(tmp_path_factory):
    bundle = fx.build_bundle(tmp_path_factory.mktemp("expressions"))
    server, proc = spawn_server(bundle, tmp_path_factory.mktemp("expressions-server"))
    yield server
    stop_server(proc)


def read_all(server, token: str) -> dict[int, tuple[int, list[str]]]:
    """Every item a bulk read returns, as `source id -> (tessera_id, labels)`."""
    body = {
        "view": fx.VIEW_ID,
        "fields": [JOIN_FIELD],
        "system_fields": ["labels"],
        "page_rows": 50,
    }
    out: dict[int, tuple[int, list[str]]] = {}
    while True:
        response = server.items(token, **body)
        assert response.status_code == 200, response.text
        decoded = wire.split_items_frames(response.content)
        for records, _end in decoded.pages:
            with ipc.open_stream(io.BytesIO(records)) as reader:
                for batch in reader:
                    rows = zip(
                        batch.column(JOIN_FIELD).to_pylist(),
                        batch.column("tessera_id").to_pylist(),
                        batch.column("tessera:labels").to_pylist(),
                    )
                    for source_id, tessera_id, labels in rows:
                        assert source_id not in out, f"source id {source_id} returned twice"
                        out[source_id] = (tessera_id, labels)
        cursor = decoded.trailer["next"]
        if cursor is None:
            return out
        body["cursor"] = cursor


@pytest.mark.parametrize("terms", fx.PRINCIPALS, ids=lambda t: ",".join(t) or "none")
def test_the_items_and_their_labels_are_the_oracle_s(expression_server, terms):
    token = expression_server.authorise(terms)["token"]
    served = read_all(expression_server, token)

    assert set(served) == fx.visible_to(terms)
    for source_id, (tessera_id, labels) in served.items():
        expected = fx.card_of(source_id, terms)
        assert labels == expected, (source_id, fx.labels_of(source_id))
        card = expression_server.item(token, tessera_id)
        assert card.status_code == 200, card.text
        assert card.json()["labels"] == expected, (source_id, fx.labels_of(source_id))


def test_the_principals_separate_every_shape():
    """Each label shape is seen by one principal and withheld from another, or a disagreement in
    its evaluation could pass unseen."""
    for shape in range(len(fx.LABELS)):
        seen_by = [
            terms
            for terms in fx.PRINCIPALS
            if shape in {s % len(fx.LABELS) for s in fx.visible_to(terms)}
        ]
        assert seen_by, fx.LABELS[shape]
        if fx.LABELS[shape]:
            assert len(seen_by) < len(fx.PRINCIPALS), fx.LABELS[shape]
