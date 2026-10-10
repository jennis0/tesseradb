"""Each number's holders, counted from what the oracle has seen issued.

An item's `mosaica_id` is `forward_item(key, shard, tenancy, number)`, where the tenancy is how
many items held the number before it (`docs/sharding.md` §1.2). A compaction that removes a deleted
item's last entity frees its number, and a new item may then take it one tenancy higher, so a held
`mosaica_id` never names another item.

`HolderCount` derives the tenancy every identifier must carry from its own count. It never reads
the stored identifiers or the server's tenancy index. A bundle as built holds each number below its
high water once, at tenancy 0. Each new item an acknowledged ingest reports must carry a tenancy
above every earlier holder's: one above, unless a restart replayed the deletion after the
compaction that freed the number, which frees it again one tenancy higher. The number's previous
holder must have been seen deleted first. A row naming an item answers the
identifier of its number's latest holder.

Every check raises `AssertionError`, since a disagreement is the service's.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from .identity import KIND_ITEM, IdentityKey, forward_item, invert_item


@dataclass
class HolderCount:
    """Who has held each number of one shard.

    `built` is the high water of the bundle as built: every number below it was one item's, at
    tenancy 0, before any write. `deleted` holds the identifiers of the items seen deleted.
    """

    key: IdentityKey
    shard: int
    built: int
    deleted: set[int] = field(default_factory=set)
    _latest: dict[int, int] = field(default_factory=dict, init=False, repr=False)

    @classmethod
    def of_bundle(cls, bundle) -> "HolderCount":
        """The count for a `Bundle` opened as built, before a server has written to it."""
        if bundle.identity_key is None:
            raise ValueError("the bundle's MANIFEST carries no `identity`; rebuild it to read one")
        return cls(
            key=bundle.identity_key,
            shard=bundle.identity_shard_id,
            built=int(bundle.manifest["entity_id_high_water"]),
        )

    def latest(self, number: int) -> int | None:
        """The tenancy of the latest item to hold `number`, or `None` where none has."""
        return self._latest.get(number, 0 if number < self.built else None)

    def holder(self, number: int) -> int | None:
        """The `mosaica_id` of the latest item to hold `number`, or `None` where none has."""
        latest = self.latest(number)
        return None if latest is None else forward_item(self.key, self.shard, latest, number)

    def number_of(self, mosaica_id: int) -> tuple[int, int]:
        """The tenancy and number an item's identifier inverts to. An identifier of another kind
        or another shard names no item here and is refused."""
        kind, shard, tenancy, number = invert_item(self.key, mosaica_id)
        if kind != KIND_ITEM or shard != self.shard:
            raise AssertionError(
                f"mosaica_id {mosaica_id} inverts to kind {kind} in shard {shard}, which is no "
                f"item of shard {self.shard}"
            )
        return tenancy, number

    def seen(self, mosaica_id: int) -> bool:
        """Whether `mosaica_id` was issued to an item this count knows, deleted or not."""
        kind, shard, tenancy, number = invert_item(self.key, mosaica_id)
        latest = self.latest(number)
        return kind == KIND_ITEM and shard == self.shard and latest is not None and tenancy <= latest

    def live(self, mosaica_id: int) -> bool:
        """Whether `mosaica_id` is its number's latest holder and was not seen deleted."""
        kind, shard, tenancy, number = invert_item(self.key, mosaica_id)
        return (
            kind == KIND_ITEM
            and shard == self.shard
            and tenancy == self.latest(number)
            and mosaica_id not in self.deleted
        )

    def named(self, mosaica_id: int) -> int:
        """Check the identifier answered for a row naming an item, and answer its number."""
        tenancy, number = self.number_of(mosaica_id)
        if not self.live(mosaica_id):
            raise AssertionError(
                f"mosaica_id {mosaica_id} was answered for a held item, and it is number {number} "
                f"at tenancy {tenancy}, which is not the number's latest holder or was deleted "
                f"(its latest holder is at tenancy {self.latest(number)})"
            )
        return number

    def created(self, mosaica_id: int) -> int:
        """Check and count the identifier issued to a new item, and answer its number."""
        tenancy, number = self.number_of(mosaica_id)
        latest = self.latest(number)
        if latest is None and tenancy != 0:
            raise AssertionError(
                f"mosaica_id {mosaica_id} was issued to a new item at tenancy {tenancy} of number "
                f"{number}, which nobody has held"
            )
        if latest is not None and tenancy <= latest:
            raise AssertionError(
                f"mosaica_id {mosaica_id} was issued to a new item at tenancy {tenancy} of number "
                f"{number}, whose latest holder was at tenancy {latest}"
            )
        previous = self.holder(number)
        if previous is not None and previous not in self.deleted:
            raise AssertionError(
                f"number {number} was issued to a new item while its holder {previous} is not "
                "deleted"
            )
        self._latest[number] = tenancy
        return number

    def delete(self, mosaica_id: int) -> None:
        """Record an accepted deletion. An item deleted twice in one request is deleted once."""
        if mosaica_id not in self.deleted:
            self.named(mosaica_id)
            self.deleted.add(mosaica_id)

    def receipt(self, receipt: dict) -> list[int]:
        """Check an accepted ingest receipt, count its new items, and answer their numbers.

        The receipt counts the rows that created an item without saying which they are. A row
        naming an item answers an identifier this count holds live; a row creating one answers an
        identifier that is not. A new item issued a live item's identifier is therefore taken for
        a row naming it, and the receipt's `created` then disagrees. A replay answers its first
        acceptance's identifiers and writes nothing, so each must be one this count has seen.
        """
        answered = [int(t) for t in receipt["mosaica_ids"] if t is not None]
        if receipt.get("replayed"):
            unseen = [t for t in answered if not self.seen(t)]
            if unseen:
                raise AssertionError(f"a replay answered identifiers never issued: {unseen}")
            return []
        numbers = [self.created(t) for t in answered if not self.live(t)]
        if len(numbers) != receipt["created"]:
            raise AssertionError(
                f"the receipt says {receipt['created']} rows created an item, and "
                f"{len(numbers)} of its identifiers are new"
            )
        return numbers
