"""Each number's holders, counted from what the oracle has seen issued.

An item's `mosaica_id` is `forward_item(key, shard, tenancy, number)`, where the tenancy is how
many items held the number before it (`docs/sharding.md` §1.2). A compaction that removes a deleted
item's last entity frees its number, and a new item may then take it one tenancy higher, so a held
`mosaica_id` never names another item.

`HolderCount` derives the tenancy every identifier must carry from its own count. It never reads
the stored identifiers or the server's tenancy index. A number belongs to its shard, so the count
keys each by [`item_key`] over the two. A bundle as built holds each number below its shard's high
water once, at tenancy 0. Each new item an acknowledged ingest reports must carry a tenancy
above every earlier holder's: one above, unless a restart replayed the deletion after the
compaction that freed the number, which frees it again one tenancy higher. The number's previous
holder must have been seen deleted first. A row naming an item answers the
identifier of its number's latest holder.

Every check raises `AssertionError`, since a disagreement is the service's.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from .identity import IdentityKey, forward_item, invert_item, item_key, item_of_key


@dataclass
class HolderCount:
    """Who has held each number of each shard. A number here is an [`item_key`].

    `built` maps each shard of the bundle as built to its high water: every number below it was one
    item's, at tenancy 0, before any write. `deleted` holds the identifiers of the items seen
    deleted.
    """

    key: IdentityKey
    built: dict[int, int]
    deleted: set[int] = field(default_factory=set)
    _latest: dict[int, int] = field(default_factory=dict, init=False, repr=False)

    @classmethod
    def of_bundle(cls, bundle) -> "HolderCount":
        """The count for a `Bundle` opened as built, before a server has written to it."""
        if bundle.identity_key is None:
            raise ValueError("the bundle's MANIFEST carries no `identity`; rebuild it to read one")
        built = {record["id"]: int(record["high_water"]) for record in bundle.shards}
        return cls(key=bundle.identity_key, built=built)

    def latest(self, number: int) -> int | None:
        """The tenancy of the latest item to hold `number`, or `None` where none has."""
        shard, local = item_of_key(number)
        return self._latest.get(number, 0 if local < self.built.get(shard, 0) else None)

    def holder(self, number: int) -> int | None:
        """The `mosaica_id` of the latest item to hold `number`, or `None` where none has."""
        latest = self.latest(number)
        if latest is None:
            return None
        shard, local = item_of_key(number)
        return forward_item(self.key, shard, latest, local)

    def _item(self, mosaica_id: int) -> tuple[int, int] | None:
        """The tenancy and number `mosaica_id` inverts to, or `None` where it names no item of a
        shard this count knows."""
        item = invert_item(self.key, mosaica_id)
        if item is None or item[0] not in self.built:
            return None
        return item[1], item_key(item[0], item[2])

    def number_of(self, mosaica_id: int) -> tuple[int, int]:
        """The tenancy and number an item's identifier inverts to. An identifier of another kind
        or of a shard this count does not know names no item here and is refused."""
        item = self._item(mosaica_id)
        if item is None:
            raise AssertionError(
                f"mosaica_id {mosaica_id} names no item of shards {sorted(self.built)}"
            )
        return item

    def seen(self, mosaica_id: int) -> bool:
        """Whether `mosaica_id` was issued to an item this count knows, deleted or not."""
        item = self._item(mosaica_id)
        if item is None:
            return False
        tenancy, number = item
        latest = self.latest(number)
        return latest is not None and tenancy <= latest

    def live(self, mosaica_id: int) -> bool:
        """Whether `mosaica_id` is its number's latest holder and was not seen deleted."""
        item = self._item(mosaica_id)
        if item is None:
            return False
        tenancy, number = item
        return tenancy == self.latest(number) and mosaica_id not in self.deleted

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
