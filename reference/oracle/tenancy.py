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
identifier of its number's latest holder. A new item may be the first of a shard the count does
not know only where that shard takes the next number after every shard it knows: a service opens
shards in number order.

Every check raises `AssertionError`, since a disagreement is the service's.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from .identity import IdentityKey, forward_item, invert_item, item_key, item_of_key


@dataclass
class HolderCount:
    """Who has held each number of each shard.

    `built` maps each shard of the bundle as built to its high water: every number below it was one
    item's, at tenancy 0, before any write. `deleted` holds the identifiers of the items seen
    deleted.
    """

    key: IdentityKey
    built: dict[int, int]
    deleted: set[int] = field(default_factory=set)
    _latest: dict[int, int] = field(default_factory=dict, init=False, repr=False)
    #: Each known shard's least possible high water: its built one, raised past every number a new
    #: item of it took. A shard opened since the build is known from its first new item.
    _high_water: dict[int, int] = field(default_factory=dict, init=False, repr=False)

    def __post_init__(self) -> None:
        self._high_water = dict(self.built)

    @classmethod
    def of_bundle(cls, bundle) -> "HolderCount":
        """The count for a `Bundle` opened as built, before a server has written to it."""
        if bundle.identity_key is None:
            raise ValueError("the bundle's MANIFEST carries no `identity`; rebuild it to read one")
        built = {record["id"]: int(record["high_water"]) for record in bundle.shards}
        return cls(key=bundle.identity_key, built=built)

    def latest(self, item: int) -> int | None:
        """The tenancy of the latest item to hold `item`'s number, or `None` where none has."""
        shard, number = item_of_key(item)
        return self._latest.get(item, 0 if number < self.built.get(shard, 0) else None)

    def holder(self, item: int) -> int | None:
        """The `mosaica_id` of the latest item to hold `item`'s number, or `None` where none has."""
        latest = self.latest(item)
        if latest is None:
            return None
        shard, number = item_of_key(item)
        return forward_item(self.key, shard, latest, number)

    def high_water_floor(self) -> int:
        """The least `entity_id_high_water` a service can report: each known shard's high water,
        summed."""
        return sum(self._high_water.values())

    def _item(self, mosaica_id: int, *, opening: bool = False) -> tuple[int, int] | None:
        """The tenancy and [`item_key`] `mosaica_id` inverts to, or `None` where it names no item
        of a shard this count knows, or, if `opening`, of the shard opened next."""
        item = invert_item(self.key, mosaica_id)
        if item is None:
            return None
        if item[0] not in self._high_water and not (
            opening and item[0] == max(self._high_water, default=-1) + 1
        ):
            return None
        return item[1], item_key(item[0], item[2])

    def item_of(self, mosaica_id: int, *, opening: bool = False) -> tuple[int, int]:
        """The tenancy and [`item_key`] an item's identifier inverts to. An identifier of another
        kind or of a shard this count does not know names no item here and is refused; if
        `opening`, the shard opened next is known."""
        item = self._item(mosaica_id, opening=opening)
        if item is None:
            raise AssertionError(
                f"mosaica_id {mosaica_id} names no item of shards {sorted(self._high_water)}"
            )
        return item

    def seen(self, mosaica_id: int) -> bool:
        """Whether `mosaica_id` was issued to an item this count knows, deleted or not."""
        item = self._item(mosaica_id)
        if item is None:
            return False
        tenancy, key = item
        latest = self.latest(key)
        return latest is not None and tenancy <= latest

    def live(self, mosaica_id: int) -> bool:
        """Whether `mosaica_id` is its number's latest holder and was not seen deleted."""
        item = self._item(mosaica_id)
        if item is None:
            return False
        tenancy, key = item
        return tenancy == self.latest(key) and mosaica_id not in self.deleted

    def named(self, mosaica_id: int) -> int:
        """Check the identifier answered for a row naming an item, and answer its item."""
        tenancy, item = self.item_of(mosaica_id)
        if not self.live(mosaica_id):
            raise AssertionError(
                f"mosaica_id {mosaica_id} was answered for a held item, and it is item {item} "
                f"at tenancy {tenancy}, which is not the number's latest holder or was deleted "
                f"(its latest holder is at tenancy {self.latest(item)})"
            )
        return item

    def created(self, mosaica_id: int) -> int:
        """Check and count the identifier issued to a new item, and answer its item."""
        tenancy, item = self.item_of(mosaica_id, opening=True)
        latest = self.latest(item)
        if latest is None and tenancy != 0:
            raise AssertionError(
                f"mosaica_id {mosaica_id} was issued to a new item at tenancy {tenancy} of item "
                f"{item}, whose number nobody has held"
            )
        if latest is not None and tenancy <= latest:
            raise AssertionError(
                f"mosaica_id {mosaica_id} was issued to a new item at tenancy {tenancy} of item "
                f"{item}, whose number's latest holder was at tenancy {latest}"
            )
        previous = self.holder(item)
        if previous is not None and previous not in self.deleted:
            raise AssertionError(
                f"item {item}'s number was issued to a new item while its holder {previous} is "
                "not deleted"
            )
        self._latest[item] = tenancy
        shard, number = item_of_key(item)
        self._high_water[shard] = max(self._high_water.get(shard, 0), number + 1)
        return item

    def delete(self, mosaica_id: int) -> None:
        """Record an accepted deletion. An item deleted twice in one request is deleted once."""
        if mosaica_id not in self.deleted:
            self.named(mosaica_id)
            self.deleted.add(mosaica_id)

    def receipt(self, receipt: dict) -> list[int]:
        """Check an accepted ingest receipt, count its new items, and answer their items.

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
        items = [self.created(t) for t in answered if not self.live(t)]
        if len(items) != receipt["created"]:
            raise AssertionError(
                f"the receipt says {receipt['created']} rows created an item, and "
                f"{len(items)} of its identifiers are new"
            )
        return items
