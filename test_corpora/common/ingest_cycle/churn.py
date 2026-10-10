"""The churn: a built corpus deleted and inserted again, part by part, round after round.

Serves a copy of the all-in bundle. Each round deletes a fraction of its items through
`/control/changes`, ingests the same source rows again as new items through `/control/ingest`,
flushes, and compacts. Every item has a position in [0, 1) drawn from the seed, and round `r` takes
the items whose position falls in a window of the fraction's width starting `(r - 1) * fraction`
along, wrapping at 1. At one half the rounds alternate halves, and at any fraction every item is
taken once in each `1 / fraction` rounds.

Each round writes one JSON line beside `--out`. Round 0 is the copy as built, before any change:
the fresh build of the same items, which every later round is read against. `/control/status`
gives the high water and the compaction's own time. The served bundle's newest side-manifest gives
the term index's files and the freed entity ids. The viewer plane gives the counts and the
viewport latency, over boxes ranked once before the first round as the serve battery ranks its
cells.

The union of a principal's postings is timed as the `session/authorise` that builds it, which
`/control/status`'s `fragment_cache.rebuilds` rising by one shows. A fragment the cache already
holds costs no union, so round 0 authorises each principal before anything else asks for its
fragment, and every later round ends the principals' sessions before its compaction: the refresh
that follows a publication rebuilds the fragment of every session still resident.

Not built yet: a publication-route layer's memberships of a deleted item are not published again,
so after the first round such a layer serves fewer members than the fresh build. The result names
those layers under `layers_not_republished`. A column-route layer's member lists travel on the
ingest batches and are restored.
"""

from __future__ import annotations

import base64
import json
import os
import random
import time
from pathlib import Path

import numpy as np

from .. import serve_battery
from ..serve_battery import full_box
from .control import wait_for
from .cycle import Cycle, fold_sentences, safe
from .holdout import HoldOut
from .split import bundle_manifest, declared_entities, declared_layers


class Churn(Cycle):
    """The cycle's churn mode, run in place of [`Cycle._run`] when `--churn-rounds` is given."""

    def __init__(self, args):
        super().__init__(args)
        self.result.pop("fraction")
        #: One JSON line per round, written as each round ends.
        self.rows_path = Path(args.out).with_suffix(".jsonl")
        #: The served deployment's selection constants, from `/v1/meta`.
        self.k = serve_battery.DEFAULT_K
        self.max_tiles = serve_battery.DEFAULT_MAX_TILES
        #: The narrowest and broadest principals of `--targets`' ladder, and the latency boxes.
        self.principals: list[dict] = []
        self.latency_boxes: list[tuple[int, list[float]]] = []
        #: The all-in bundle's term index and free ids: the fresh build's figures.
        self.fresh: dict = {}

    def run(self) -> dict:
        result = super().run()
        rows = result.get("churn_rounds") or []
        if rows:
            print(churn_table(rows), flush=True)
        return result

    def _run(self) -> None:
        served = self.serve_copy(self.rung, self.all_in, self.work / "churn")
        if served is None:
            return
        try:
            self.read_views()
            ranks = self.read_ranks()
            control = self.control_for(served, self.anchor)
            self.result["ingested_view"] = control.view
            self.limits = self.served_limits(control.status())
            self.sessions = serve_battery.Sessions(
                served.session, served.control, served.operator_credential()
            )
            # The broadest principal's union on the fresh build, before `open_session` and every
            # count after it authorise the same grant.
            _, broadest = self.union(control, sorted(r["term"] for r in ranks))
            self.open_session(served, ranks)
            self.result["churn_rounds"] = []
            self.phase("churn", lambda: self.do_churn(control, ranks, broadest))
        finally:
            self.result["status_at_end"] = safe(
                lambda: self.control_for(served, None).status()
            )
            served.stop()

    def do_churn(self, control, ranks: list[dict], broadest: dict) -> dict:
        """Round 0, then `--churn-rounds` rounds, each written to `rows_path` as it ends."""
        args = self.args
        entities = declared_entities(self.rung)
        positions = np.random.default_rng(args.seed).random(len(entities))
        self.fresh = served_state(self.all_in)
        ladder = serve_battery.compose_ladder(
            ranks, self.visible() or 1, [float(t) for t in args.targets.split(",")]
        )
        by_target = sorted(ladder, key=lambda rung: rung["target"])
        self.principals = [by_target[0], by_target[-1]] if len(by_target) > 1 else by_target
        # Each principal's first authorise: the broadest's was taken before anything else asked.
        unions = {
            p["target"]: broadest
            if p["terms"] == self.all_terms
            else self.union(control, p["terms"])[1]
            for p in self.principals
        }
        self.latency_boxes = self.choose_latency_boxes()
        summary = {
            "rounds": args.churn_rounds,
            "fraction": args.churn_fraction,
            "seed": args.seed,
            "items": int(len(entities)),
            "rows": str(self.rows_path),
            "principals": [
                {"target": p["target"], "terms": p["terms"]} for p in self.principals
            ],
            "latency_zooms": [int(z) for z in args.churn_zooms.split(",")],
            "latency_boxes": len(self.latency_boxes),
            "samples_per_box": args.churn_samples,
            "fresh": self.fresh,
            "layers_not_republished": [
                layer["name"]
                for layer in declared_layers(self.rung)
                if layer["route"] == "publication"
            ],
        }
        self.rows_path.parent.mkdir(parents=True, exist_ok=True)
        self.rows_path.write_text("")
        self.write_row({"round": 0, "deleted": 0, **self.measure(control, unions)})
        for n in range(1, args.churn_rounds + 1):
            start = ((n - 1) * args.churn_fraction) % 1.0
            chosen = np.sort(entities[(positions - start) % 1.0 < args.churn_fraction])
            row = self.churn_round(control, n, chosen)
            if row["delete"]["status"] != 200:
                self.write_row(row)
                break
            row.update(self.measure(control))
            self.write_row(row)
        return summary

    def churn_round(self, control, n: int, chosen: np.ndarray) -> dict:
        """Delete `chosen`, ingest their rows again in every view they were in, the anchor's first
        and carrying the column-route layers' member lists, flush, and compact."""
        field = self.naming["name"]
        row: dict = {"round": n, "deleted": int(len(chosen))}
        self.log(f"round {n}: deleting {len(chosen):,} items")
        r, wall = self.send_changes(
            control, ({"op": "delete", "match": {field: str(int(e))}} for e in chosen)
        )
        row["delete"] = {
            "status": r.status_code,
            "wall_s": round(wall, 2),
            "body": None if r.status_code == 200 else r.text[:600],
        }
        if r.status_code != 200:
            return row
        # A deletion is in force once it is answered, so the count needs no wait.
        row["live_after_delete"] = self.visible()
        cap = int(self.limits["ingest"]["max_batch_bytes"])
        batch_rows = int(self.limits["ingest"]["max_batch_rows"])
        row["ingest_by_view"] = {}
        for view in self.views:
            name = view["name"]
            source = HoldOut(
                self.rung,
                chosen,
                cap,
                batch_rows,
                log=self.log,
                view=view,
                members=self.column_layers() if name == self.anchor else (),
            )
            self.log(f"round {n}: ingesting the same items' rows into {name} as new items")
            figures = self.run_ingest(
                self.control_for(self.served, name), source.batches(), f"churn{n}-{name}"
            )
            figures["bodies_over_cap"] = source.body_stats["over_cap"]
            row["ingest_by_view"][name] = figures
        code, landed, publish_s = self.flush_and_wait(control)
        target = self.result["churn_rounds"][0]["live"]
        reached, visible_s = wait_for(
            lambda: self.visible() >= target, timeout=self.args.flush_timeout, interval=0.25
        )
        row["flush"] = {
            "status": code,
            "published": landed,
            "publish_s": round(publish_s, 3),
            "visibility_reached": reached,
            "visibility_s": round(visible_s, 3),
        }
        row["sessions_ended"] = [
            control.end_sessions(self.sessions.principal(p["terms"])).status_code
            for p in self.principals
        ]
        self.log(f"round {n}: compacting")
        row["fold"] = self.do_fold(control)
        return row

    def measure(self, control, unions: dict | None = None) -> dict:
        """What a round is read for, after its compaction: the counts, the high water, the served
        term index and freed ids beside the fresh build's, and each principal's union and
        latency. `unions` gives each principal's union where it was taken earlier."""
        principals = self.latency(control, unions)
        disc = served_state(self.served.bundle)
        fresh = self.fresh
        rows = self.result["churn_rounds"]
        high_water = control.status()["entity_id_high_water"]
        return {
            "live": self.visible(),
            "live_by_view": {name: self.visible(name) for name in self.view_names}
            if len(self.views) > 1
            else None,
            "entity_id_high_water": high_water,
            "high_water_rise": high_water - rows[-1]["entity_id_high_water"] if rows else None,
            "free_ids": disc["free_ids"],
            "held_ids": disc["held_ids"],
            "prefix": disc["prefix"],
            "postings_bytes": disc["postings_bytes"],
            "fresh_postings_bytes": fresh["postings_bytes"],
            "postings_vs_fresh": round(disc["postings_bytes"] / fresh["postings_bytes"], 4)
            if fresh["postings_bytes"]
            else None,
            "term_images_bytes": disc["term_images_bytes"],
            "fresh_term_images_bytes": fresh["term_images_bytes"],
            "bundle_bytes": tree_bytes(self.served.bundle),
            "principals": principals,
        }

    def write_row(self, row: dict) -> None:
        self.result["churn_rounds"].append(row)
        with self.rows_path.open("a") as out:
            out.write(json.dumps(row, default=str) + "\n")
        self.log(f"round {row['round']} written to {self.rows_path}")

    def union(self, control, terms: list[str]) -> tuple[str, dict]:
        """One `session/authorise` for `terms`, timed, and whether it built the union of their
        postings. `union_ms` is the authorise's wall where the fragment cache rebuilt exactly one
        fragment across it, and null where the cache already held the fragment."""
        before = control.status()["fragment_cache"].get("rebuilds")
        token, seconds = self.sessions.authorise(terms)
        after = control.status()["fragment_cache"].get("rebuilds")
        rebuilds = None if before is None or after is None else after - before
        authorise_ms = round(seconds * 1000.0, 2)
        return token, {
            "authorise_ms": authorise_ms,
            "fragment_rebuilds": rebuilds,
            "union_ms": authorise_ms if rebuilds == 1 else None,
        }

    def choose_latency_boxes(self) -> list[tuple[int, list[float]]]:
        """One box per density decile at each `--churn-zooms` zoom, ranked under the broadest
        principal on the fresh build: the box the serve battery's first cell of that decile
        measures, drawn from the seed."""
        token, _ = self.sessions.authorise(self.principals[-1]["terms"])
        selection = serve_battery.meta(self.served.viewer, token).get("selection") or {}
        self.k = int(selection.get("max_k") or serve_battery.DEFAULT_K)
        self.max_tiles = int(
            selection.get("max_tiles_per_request") or serve_battery.DEFAULT_MAX_TILES
        )
        quant = self.frames[self.anchor]
        rng = random.Random(self.args.seed)
        boxes: list[tuple[int, list[float]]] = []
        for zoom in (int(z) for z in self.args.churn_zooms.split(",")):
            ranked = serve_battery.rank_by_density(
                self.served.viewer,
                token,
                self.anchor,
                zoom,
                serve_battery.candidate_boxes(quant, zoom, self.args.churn_candidates, rng),
                quant,
                serve_battery.BUDGET_DEPTH,
                self.max_tiles,
                self.log,
            )
            boxes += [(zoom, pool[0][0]) for pool in serve_battery.decile_pools(ranked) if pool]
        return boxes

    def latency(self, control, unions: dict | None) -> list[dict]:
        """Each principal's union, a fresh session's whole-extent first viewport, and
        `--churn-samples` requests at every latency box, at the battery's depth and `k`. Taken
        once the session refresh a publication starts has ended, so that no other fragment build
        runs beside the authorise."""
        wait_for(
            lambda: not control.status()["write_executor"]["flush"]["refresh_in_flight"],
            timeout=600,
            interval=0.5,
        )
        quant = self.frames[self.anchor]
        out = []
        for principal in self.principals:
            token, union = self.union(control, principal["terms"])
            if unions is not None:
                union = unions[principal["target"]]
            first = self.sample(token, 0, full_box(quant))
            samples = [
                self.sample(token, zoom, box)
                for zoom, box in self.latency_boxes
                for _ in range(self.args.churn_samples)
            ]
            out.append(
                {
                    "target": principal["target"],
                    "terms_n": len(principal["terms"]),
                    "visible": (first.get("counts") or {}).get("visible"),
                    **union,
                    "first_viewport_ms": round(first["wall_ms"], 2),
                    "first_viewport_failed": first.get("failed"),
                    "first_viewport_shed": bool(first.get("shed")),
                    **serve_battery.cell_figures(samples),
                }
            )
        return out

    def sample(self, token: str, zoom: int, box: list[float]) -> dict:
        """One viewport on the anchor view at the depth the battery asks `box` at; a request
        that did not answer is a sample marked `failed`."""
        depth = serve_battery.budget_zoom(
            self.frames[self.anchor], box, zoom, serve_battery.BUDGET_DEPTH, self.max_tiles
        )
        t0 = time.perf_counter()
        try:
            return serve_battery.viewport(
                self.served.viewer, token, self.anchor, depth, box, k=self.k
            )
        except Exception as e:  # noqa: BLE001 — the failure is the measurement
            return {
                "failed": f"{type(e).__name__}: {e}"[:300],
                "wall_ms": (time.perf_counter() - t0) * 1000.0,
            }

    def failures(self) -> list[str]:
        out = super().failures()
        churn = self.result.get("churn")
        if isinstance(churn, dict) and churn.get("failed"):
            out.append(f"the churn phase failed: {churn['failed']}")
        rows = self.result.get("churn_rounds") or []
        for row in rows[1:]:
            out += round_failures(row, rows[0])
        out += high_water_failures(rows)
        if rows and len(rows) - 1 < self.args.churn_rounds:
            out.append(f"the churn ran {len(rows) - 1} of {self.args.churn_rounds} rounds")
        return out


def round_failures(row: dict, fresh: dict) -> list[str]:
    """A sentence per way one round did not hold, against round 0's counts."""
    n = row["round"]
    delete = row["delete"]
    if delete["status"] != 200:
        return [f"round {n}'s deletes were refused {delete['status']}: {delete['body']}"]
    out = []
    passes = row.get("ingest_by_view") or {}
    anchor = next(iter(passes.values()), {})
    expected = fresh["live"] - anchor.get("rows_offered", 0)
    if row.get("live_after_delete") != expected:
        out.append(
            f"round {n}'s deletes left {row.get('live_after_delete')} visible, expecting {expected}"
        )
    for name, figures in passes.items():
        if figures.get("accepted") != figures.get("rows_offered"):
            out.append(
                f"round {n}'s ingest into {name} accepted {figures.get('accepted')} of "
                f"{figures.get('rows_offered')} rows offered"
            )
        unexpected = {
            status: count
            for status, count in (figures.get("statuses") or {}).items()
            if status not in ("200", "429")
        }
        if unexpected:
            out.append(f"round {n}'s ingest into {name} was refused: {unexpected}")
    if (row.get("flush") or {}).get("published") is False:
        out.append(f"round {n}'s flush did not reach its publication")
    if row.get("live") != fresh["live"]:
        out.append(
            f"round {n} ended at {row.get('live')} visible, where round 0 had {fresh['live']}"
        )
    for name, count in (row.get("live_by_view") or {}).items():
        was = (fresh.get("live_by_view") or {}).get(name)
        if count != was:
            out.append(f"round {n} ended at {count} visible on {name}, where round 0 had {was}")
    out += fold_sentences(f"round {n}'s fold", row.get("fold") or {})
    return out


def high_water_failures(rows: list[dict]) -> list[str]:
    """A sentence per round whose new items took more ids from the high water than the free ids
    left by the round before could not cover."""
    out = []
    for before, row in zip(rows, rows[1:]):
        free, rise = before.get("free_ids"), row.get("high_water_rise")
        if free is None or rise is None or row["delete"]["status"] != 200:
            continue
        allowed = max(0, row["deleted"] - free)
        if rise > allowed:
            out.append(
                f"round {row['round']} raised the high water by {rise:,} with {free:,} ids free "
                f"for {row['deleted']:,} new items; expected at most {allowed:,}"
            )
    return out


def served_state(bundle: Path) -> dict:
    """What the churn reads off the newest side-manifest of each partition of `bundle`'s current
    version. `postings_bytes` is the term index on disc, the base postings and every live delta
    tier; `term_images_bytes`, every view's term images; `free_ids` and `held_ids`, the freed
    entity ids the allocator issues next and those it holds back until the log rotates, null
    where the side-manifest carries no such field."""
    read = bundle_manifest(bundle)
    if read is None:
        raise FileNotFoundError(f"{bundle} has no CURRENT; state --all-in-bundle as a built bundle")
    prefix, _ = read
    root = bundle / prefix
    out = {"prefix": prefix, "postings_bytes": 0, "term_images_bytes": 0}
    frees: list[str | None] = []
    held: list[str | None] = []
    for partition in sorted(path for path in (root / "partitions").iterdir() if path.is_dir()):
        side = newest_side_manifest(partition)
        out["postings_bytes"] += (partition / "terms" / "postings.arrow").stat().st_size
        out["postings_bytes"] += sum(
            (root / path).stat().st_size for path in side.get("deltas") or []
        )
        out["term_images_bytes"] += sum(
            (root / extent["path"]).stat().st_size
            for extent in side.get("term_image_extents") or []
        )
        frees.append(side.get("free_entities"))
        if "held_entities" in side:
            held += [entry["entities"] for entry in side["held_entities"]]
        else:
            held.append(None)
    out["free_ids"] = None if None in frees else sum(map(roaring_cardinality, frees))
    out["held_ids"] = None if None in held else sum(map(roaring_cardinality, held))
    return out


def newest_side_manifest(partition: Path) -> dict:
    """The `SEGMENTS-<n>.json` of the largest `n` in one partition directory."""
    numbered = [
        (int(path.stem.split("-", 1)[1]), path) for path in partition.glob("SEGMENTS-*.json")
    ]
    if not numbered:
        raise FileNotFoundError(f"{partition} holds no SEGMENTS-<n>.json")
    return json.loads(max(numbered)[1].read_text())


def roaring_cardinality(encoded: str) -> int:
    """How many ids a side-manifest's base64 portable Roaring bitmap holds, read from its header:
    after the cookie and, where the cookie says run containers are present, their bitset, each
    container's key and its cardinality less one."""
    data = base64.b64decode(encoded)
    cookie = int.from_bytes(data[:4], "little")
    if cookie & 0xFFFF == 12347:
        containers = (cookie >> 16) + 1
        at = 4 + (containers + 7) // 8
    elif cookie == 12346:
        containers = int.from_bytes(data[4:8], "little")
        at = 8
    else:
        raise ValueError(
            f"a Roaring bitmap whose cookie is {cookie}, which is not the portable format"
        )
    if containers == 0:
        return 0
    header = np.frombuffer(data, dtype="<u2", count=2 * containers, offset=at)
    return int(header[1::2].astype(np.int64).sum()) + containers


def tree_bytes(root: Path) -> int:
    """Every file's size under `root`. A file a reclaim removes during the walk counts as none."""
    total = 0
    for directory, _, files in os.walk(root):
        for name in files:
            try:
                total += os.stat(os.path.join(directory, name)).st_size
            except FileNotFoundError:
                continue
    return total


def churn_table(rows: list[dict]) -> str:
    """One line per round: the counts, the high water, the freed ids, the term index against the
    fresh build, the compaction, and the broadest principal's union and viewport latency."""
    head = (
        "round", "live", "high water", "rise", "free", "held", "postings B", "x fresh",
        "fold s", "union ms", "p50 ms", "p99 ms",
    )
    lines = [head]
    for row in rows:
        broadest = (row.get("principals") or [{}])[-1]
        wall = broadest.get("wall_ms") or {}
        lines.append(
            (
                str(row["round"]),
                number(row.get("live")),
                number(row.get("entity_id_high_water")),
                number(row.get("high_water_rise")),
                number(row.get("free_ids")),
                number(row.get("held_ids")),
                number(row.get("postings_bytes")),
                number(row.get("postings_vs_fresh")),
                number((row.get("fold") or {}).get("fold_s")),
                number(broadest.get("union_ms")),
                number(wall.get("p50")),
                number(wall.get("p99")),
            )
        )
    widths = [max(len(line[i]) for line in lines) for i in range(len(head))]
    return "\n".join(
        "  ".join(cell.rjust(width) for cell, width in zip(line, widths)) for line in lines
    )


def number(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:,.2f}"
    return f"{value:,}"
