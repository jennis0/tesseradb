"""The map from the user's own id to the source id the build reads (python-sdk.md §3).

The build reads a `u64` source id from each points file's `entity_id` column and mints the
external id from it. The SDK assigns that source id, so a user identifies a row by whatever they
identify it by — a string, an integer — and one user id is one entity in every source that names
entities.

Each id carries a state. `assigned` is what staging gives it, `acknowledged` is what a commit
gives it, and `removed` is what `remove()` gives it. The pre-flight reads `acknowledged` alone, so
a page refused at one commit is sent at the next, and a removed id staged again goes as a point
row (decision 0047).

The map is a JSON document under `.tessera/`, written whole at each save. It holds one entry per
entity, so a corpus of 10^8 entities is a file of that order; the shape that would replace it is a
sorted sidecar, and the cost of writing one is not paid for a notebook corpus.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Hashable, Iterable

ASSIGNED = "assigned"
ACKNOWLEDGED = "acknowledged"
REMOVED = "removed"


class IdMap:
    """The user's ids, their source ids, and the state of each."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self._ids: dict[tuple[str, str], int] = {}
        self._states: dict[int, str] = {}
        self._next = 1
        if self.path.exists():
            self._load()

    # The key carries its type name beside its text: `1` and `"1"` are two ids, and a map that
    # spelled both `"1"` would join two entities the user kept apart.
    @staticmethod
    def _key(user_id: Hashable) -> tuple[str, str]:
        return (type(user_id).__name__, str(user_id))

    def source_ids(self, user_ids: Iterable[Hashable]) -> list[int]:
        """The source id of each user id, assigning one in staging order to an id not seen."""
        out = []
        for user_id in user_ids:
            key = self._key(user_id)
            source_id = self._ids.get(key)
            if source_id is None:
                source_id = self._next
                self._next += 1
                self._ids[key] = source_id
                self._states[source_id] = ASSIGNED
            out.append(source_id)
        return out

    def state_of(self, user_id: Hashable) -> str | None:
        source_id = self._ids.get(self._key(user_id))
        return None if source_id is None else self._states[source_id]

    def acknowledge(self, user_ids: Iterable[Hashable]) -> int:
        return self._set_state(user_ids, ACKNOWLEDGED)

    def acknowledge_all(self) -> int:
        moved = 0
        for source_id, state in self._states.items():
            if state == ASSIGNED:
                self._states[source_id] = ACKNOWLEDGED
                moved += 1
        return moved

    def remove(self, user_ids: Iterable[Hashable]) -> int:
        return self._set_state(user_ids, REMOVED)

    def _set_state(self, user_ids: Iterable[Hashable], state: str) -> int:
        moved = 0
        for user_id in user_ids:
            source_id = self._ids.get(self._key(user_id))
            if source_id is not None:
                self._states[source_id] = state
                moved += 1
        return moved

    def acknowledged(self) -> set[int]:
        return {i for i, state in self._states.items() if state == ACKNOWLEDGED}

    def __len__(self) -> int:
        return len(self._ids)

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        document = {
            "next": self._next,
            "entries": [
                [kind, text, source_id, self._states[source_id]]
                for (kind, text), source_id in self._ids.items()
            ],
        }
        self.path.write_text(json.dumps(document), encoding="utf-8")

    def _load(self) -> None:
        document = json.loads(self.path.read_text(encoding="utf-8"))
        self._next = document["next"]
        for kind, text, source_id, state in document["entries"]:
            self._ids[(kind, text)] = source_id
            self._states[source_id] = state
