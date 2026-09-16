"""The map from the user's own id to the source id the build reads (python-sdk.md §3).

The build reads a `u64` source id from each points file's `entity_id` column and mints the
external id from it. The SDK assigns that source id, so a user identifies a row by whatever they
identify it by, a string or an integer, and one user id is one entity in every source that names
entities.

**Identity mode.** A points file read in place already carries the ids the build will read, so the
map over that database is the identity: an integer entity column on every later source passes
through unchanged, and a non-integer id column is refused naming the file the ids came from.
Mapping them instead would re-key a members table against a points file nobody rewrote, which
attaches every cluster to the wrong rows and reports nothing.

Each id carries a state. `assigned` is what staging gives it, `acknowledged` is what a commit
gives it, and `removed` is what `remove()` gives it. The pre-flight reads `acknowledged` alone, so
a page refused at one commit is sent at the next, and a removed id staged again goes as a point
row (decision 0047).

The map is a JSON document under `.tessera/`, written whole at each save. It holds one entry per
entity, so a corpus of 10^8 entities is a file of that order. The shape that would replace it is a
sorted sidecar, and the cost of writing one is not paid for a notebook corpus.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Hashable, Iterable

from ._refusal import Refusal

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
        #: The source whose ids this map is the identity over, where it is in identity mode.
        self.identity_source: str | None = None
        if self.path.exists():
            self._load()

    @property
    def identity(self) -> bool:
        return self.identity_source is not None

    def use_identity(self, source: str) -> None:
        """Take this source's own integer ids as the source ids, for this database's life."""
        if self.identity_source == source:
            return
        if self._ids:
            raise Refusal(
                f"source {source!r} is read in place, so its ids are the source ids, but ids have "
                f"already been assigned to staged frames. Stage the points file first, or stage it "
                f"as a frame so that every source is mapped alike"
            )
        self.identity_source = source

    @staticmethod
    def _key(user_id: Hashable) -> tuple[str, str]:
        """The key carries its type beside its text: `1` and `"1"` are two ids."""
        return (type(user_id).__name__, str(user_id))

    def source_ids(self, user_ids: Iterable[Hashable], source: str = "this source") -> list[int]:
        """The source id of each user id, assigning one in staging order to an id not seen."""
        values = list(user_ids)
        if self.identity:
            return self._identity_ids(values, source)
        out = []
        for user_id in values:
            key = self._key(user_id)
            source_id = self._ids.get(key)
            if source_id is None:
                source_id = self._next
                self._next += 1
                self._ids[key] = source_id
                self._states[source_id] = ASSIGNED
            out.append(source_id)
        return out

    def _identity_ids(self, values: list[Hashable], source: str) -> list[int]:
        out = []
        for user_id in values:
            if isinstance(user_id, bool) or not isinstance(user_id, int) or user_id < 0:
                raise Refusal(
                    f"source {source!r}: {user_id!r} is not a source id. The points source "
                    f"{self.identity_source!r} is read where it lies, so its own integer ids are "
                    f"what the build reads and every source beside it names entities by those "
                    f"ids. Stage {self.identity_source!r} as a frame to map both alike"
                )
            out.append(user_id)
        return out

    def state_of(self, user_id: Hashable) -> str | None:
        source_id = self._ids.get(self._key(user_id))
        return None if source_id is None else self._states[source_id]

    def source_id_of(self, user_id: Hashable) -> int | None:
        """The source id a user id names, or `None` where this map has never seen it.

        Under identity mode the map holds no entries, the points file's own integer ids being the
        source ids, so an integer passes through and anything else names nothing.
        """
        if self.identity:
            if isinstance(user_id, bool) or not isinstance(user_id, int) or user_id < 0:
                return None
            return user_id
        return self._ids.get(self._key(user_id))

    def record_identity(self, source_ids: Iterable[int]) -> int:
        """Record ids a build read under identity mode, acknowledged (§3).

        Under identity mode the points file's own integer ids are the source ids and nothing is
        assigned, so the map would hold no entry and every id would read as new. The pre-flight
        reads acknowledged ids to tell a delta's new rows from its held ones, so the ids the first
        commit read are written here at that commit. One entry per entity: the same file the
        mapped case writes.
        """
        added = 0
        for source_id in source_ids:
            key = self._key(int(source_id))
            if key in self._ids:
                continue
            self._ids[key] = int(source_id)
            self._states[int(source_id)] = ACKNOWLEDGED
            added += 1
        return added

    def acknowledge_ids(self, source_ids: Iterable[int]) -> int:
        """Move source ids to `acknowledged`: what a commit that carried their rows gives them."""
        moved = 0
        for source_id in source_ids:
            if self._states.get(source_id) == ASSIGNED:
                self._states[source_id] = ACKNOWLEDGED
                moved += 1
        return moved

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
            "identity_source": self.identity_source,
            "entries": [
                [kind, text, source_id, self._states[source_id]]
                for (kind, text), source_id in self._ids.items()
            ],
        }
        self.path.write_text(json.dumps(document), encoding="utf-8")

    def _load(self) -> None:
        document = json.loads(self.path.read_text(encoding="utf-8"))
        self._next = document["next"]
        self.identity_source = document.get("identity_source")
        for kind, text, source_id, state in document["entries"]:
            self._ids[(kind, text)] = source_id
            self._states[source_id] = state
