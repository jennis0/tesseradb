"""A small corpus built over shards of a size a test sets, and what the oracle knows of it.

The corpus is `declared_fixture`'s world: items named by their unique `fx`, each with one access
label, and a rendered `score`. Every identifier a server answers is inverted with the bundle's
identity key to `(shard, tenancy, number)`, and every new item and deletion is counted by
`oracle.tenancy.HolderCount`, so a test asserts where an item went from identifiers alone. What a
principal is served is checked against the set the oracle derives: every item it ingested or the
build holds, carrying a label the principal holds, less the deleted and the suppressed.
"""

from __future__ import annotations

import io
import json
from pathlib import Path

import pyarrow as pa
import pyarrow.ipc as ipc

import declared_fixture as fx
from declared_fixture import BBOX, PRINCIPALS, WORLD, Column, Corpus, Deployment
from oracle.bundle import Bundle
from oracle.identity import invert_item
from oracle.tenancy import HolderCount
from oracle.wire import decode_viewport, split_aggregate_frames, split_items_frames

#: The rendered value an edit changes.
SCORE = Column("score", "u32", pa.uint32(), lambda i: 3 * i, render=True)


def shards_toml(size: int) -> str:
    return f"[shards]\nsize = {size}\n"


class Shards:
    """One deployment of `n_items` items at `[shards] size = size`, served."""

    def __init__(self, work: Path, n_items: int, size: int):
        Deployment.stop_all()
        blocks = [shards_toml(size), fx.plain_view_toml(), fx.fx_column().toml(), SCORE.toml()]
        world = {WORLD: (list(range(n_items)), [fx.fx_column(), SCORE], 0, fx.WORLD_EXTENT)}
        self.d = Deployment(work, Corpus(blocks, world))
        built = Bundle(self.d.bundle)
        self.built_shards = {r["id"]: int(r["high_water"]) for r in built.shards}
        self.key = built.identity_key
        self.numbers = HolderCount.of_bundle(built)
        #: Each live or suppressed item's identifier and label, by `fx`.
        self.ids: dict[int, int] = dict(self.served("everyone"))
        self.access: dict[int, str] = {i: fx.access_of(i) for i in range(n_items)}
        self.deleted: dict[int, int] = {}
        self.suppressed: set[int] = set()
        #: The shards written to since the build or the last compaction.
        self.written: set[int] = set()
        self._batches = 0

    @property
    def server(self):
        return self.d.server

    def stop(self) -> None:
        self.d.stop()

    def restart(self) -> None:
        self.d.restart()

    def compact(self) -> None:
        """Compact, wait for the request to be answered and for the publication after it."""
        self.d.fold()
        self.written.clear()

    def status(self) -> dict:
        return self.server.status()

    def high_water(self) -> int:
        return int(self.status()["entity_id_high_water"])

    def place(self, mosaica_id: int) -> tuple[int, int, int]:
        """(shard, tenancy, number) of an item's identifier."""
        placed = invert_item(self.key, mosaica_id)
        assert placed is not None, f"{mosaica_id} names no item"
        return placed

    def placement(self, i: int) -> tuple[int, int, int]:
        return self.place(self.ids[i])

    # -- writes ----------------------------------------------------------------------------------

    def ingest(self, rows: list[dict]) -> list[int]:
        """Ingest `rows` in one batch and answer each row's identifier, checking every one
        against the holder count."""
        self._batches += 1
        resp = self.d.control(
            "POST",
            "/control/ingest?strict=true",
            expect=(200,),
            headers={
                "x-mosaica-batch-id": f"shards-{self._batches}",
                "content-type": "application/json",
            },
            data=json.dumps(rows),
        )
        receipt = resp.json()
        self.numbers.receipt(receipt)
        ids = [int(t) for t in receipt["mosaica_ids"]]
        for row, t in zip(rows, ids):
            if row["fx"] in self.ids:
                assert self.ids[row["fx"]] == t, f"an edit of fx {row['fx']} changed its identifier"
            self.ids[row["fx"]] = t
            self.written.add(self.place(t)[0])
            if "access" in row:
                self.access[row["fx"]] = row["access"][0]
        return ids

    def new_items(self, fxs, access: str = "pa") -> list[tuple[int, int, int]]:
        """Ingest new items `fxs`, all with one label, in batches of at most 500, and answer
        where each went, in row order."""
        fxs = list(fxs)
        assert not set(fxs) & set(self.ids), "a new item's fx is held"
        placed = []
        for start in range(0, len(fxs), 500):
            rows = []
            for i in fxs[start : start + 500]:
                x, y = fx.position(0, i)
                rows.append({"fx": i, "x": x, "y": y, "access": [access], "score": 3 * i})
            placed += [self.place(t) for t in self.ingest(rows)]
        return placed

    def edit(self, i: int, score: int) -> int:
        """Move item `i` and give it `score`, answering the identifier the receipt names."""
        x, y = fx.position(1, i)
        [t] = self.ingest([{"fx": i, "x": x, "y": y, "score": score}])
        return t

    def changes(self, ops: list[tuple[str, int]]) -> None:
        """Apply `(op, fx)` changes, strictly."""
        body = [{"op": op, "match": {"mosaica_id": str(self.ids[i])}} for op, i in ops]
        resp = self.server.changes(body, strict=True)
        assert resp.status_code == 200, resp.text
        for op, i in ops:
            self.written.add(self.placement(i)[0])
            if op == "delete":
                self.numbers.delete(self.ids[i])
                self.deleted[i] = self.ids.pop(i)
                self.suppressed.discard(i)
            elif op == "suppress":
                self.suppressed.add(i)
            else:
                self.suppressed.discard(i)

    def publish(self) -> None:
        self.d.publish()

    # -- reads -----------------------------------------------------------------------------------

    def token(self, principal: str) -> str:
        return self.server.authorise(list(PRINCIPALS[principal]))["token"]

    def expected(self, principal: str) -> set[int]:
        """The items `principal` may see, by `fx`."""
        held = set(PRINCIPALS[principal])
        return {
            i for i in self.ids if i not in self.suppressed and self.access.get(i, "pa") in held
        }

    def served(self, principal: str) -> dict[int, int]:
        """Every item `principal` is served, `fx` to identifier, from a paged items read."""
        token = self.token(principal)
        body: dict = {"view": WORLD, "fields": ["fx"], "order": "map"}
        out: dict[int, int] = {}
        while True:
            resp = self.server.items(token, **body)
            assert resp.status_code == 200, resp.text
            decoded = split_items_frames(resp.content)
            for records, _end in decoded.pages:
                with ipc.open_stream(io.BytesIO(records)) as reader:
                    for row in reader.read_all().to_pylist():
                        out[int(row["fx"])] = int(row["mosaica_id"])
            if decoded.trailer["next"] is None:
                return out
            body["cursor"] = decoded.trailer["next"]

    def counted(self, principal: str) -> tuple[int, int]:
        """The visible count a whole-map viewport sums to, and the aggregate's total."""
        token = self.token(principal)
        tiles, _points = decode_viewport(self.server.viewport(token, WORLD, 0, BBOX, k=1))
        resp = self.server.aggregate(token, view=WORLD, groupings=[{}])
        assert resp.status_code == 200, resp.text
        decoded = split_aggregate_frames(resp.content)
        total = 0
        for _head, pages in decoded.tables:
            for records, _end in pages:
                with ipc.open_stream(io.BytesIO(records)) as reader:
                    total += sum(reader.read_all().column("count").to_pylist())
        return sum(tile[1] for tile in tiles), total

    def card(self, mosaica_id: int):
        return self.server.item(self.token("everyone"), mosaica_id)

    def check_served(self) -> None:
        """Every principal is served what the oracle derives, by an items read, a viewport and an
        aggregate; and every principal's served identifiers are the ones the count holds live."""
        for principal in PRINCIPALS:
            expected = self.expected(principal)
            served = self.served(principal)
            assert set(served) == expected, (
                f"{principal}: served but not expected {sorted(set(served) - expected)[:10]}, "
                f"expected but not served {sorted(expected - set(served))[:10]}"
            )
            for i, t in served.items():
                assert t == self.ids[i] and self.numbers.live(t), (i, t)
            assert self.counted(principal) == (len(expected), len(expected)), principal
