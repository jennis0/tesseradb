from __future__ import annotations

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib
import base64
import concurrent.futures
import json
import random
import shutil
import threading
import time
import uuid
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import requests

from .. import serve_battery
from ..deployment import Deployment, minted_credentials
from ..serve_battery import full_box
from .census import (
    census,
    census_coverage,
    census_zooms,
    compare_census,
    coverage_failures,
    incomplete_sentences,
)
from .control import Control, wait_for
from .holdout import HoldOut
from .publication import Publication
from .split import (
    build_bundle,
    declared_entities,
    declared_layers,
    declared_views,
    ranks_for,
    split_entities,
    state_extent,
    write_base_inputs,
)

# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------

#: Candidate boxes ranked per census box asked for, so the deciles have something to rank.
CENSUS_CANDIDATES = 6


def executor_laps(before: dict, after: dict, rows: int) -> dict:
    """The `WriteStage` laps across one ingest phase, µs per accepted row, differenced rather
    than read absolute since the laps are process totals. All zero without `bench-timing`.
    `unattributed` is the coarse `apply_nanos_total` minus the three `apply` laps plus whatever
    `submit\u2192receipt` sees beyond the executor's own stages.
    """
    stages = after.get("stage_nanos") or {}
    prior = before.get("stage_nanos") or {}
    windows = after.get("wal_fsyncs", 0) - before.get("wal_fsyncs", 0)
    laps = {
        name: round((nanos - prior.get(name, 0)) / 1000.0 / rows, 3) if rows else None
        for name, nanos in stages.items()
    }
    executor = sum(
        laps.get(name) or 0.0
        for name in ("allocate", "wal_append", "wal_fsync", "buffer_clone", "apply_rows",
                     "swap", "admit", "record_batch")
    )
    return {
        "bench_timing": after.get("bench_timing", False),
        "rows": rows,
        "windows": windows,
        "rows_per_window": round(rows / windows, 1) if windows else None,
        "us_per_row": laps,
        "executor_sum_us_per_row": round(executor, 3),
        "queueing_us_per_row": round((laps.get("submit\u2192receipt") or 0.0) - executor, 3),
        "apply_nanos_total_us_per_row": round(
            (after.get("apply_nanos_total", 0) - before.get("apply_nanos_total", 0))
            / 1000.0 / rows, 3
        ) if rows else None,
        "apply_nanos_max_ms": round(after.get("apply_nanos_max", 0) / 1e6, 1),
        "work_service_nanos_ewma_ms": round(after.get("work_service_nanos_ewma", 0) / 1e6, 1),
        "flush": flush_laps(before, after),
    }


def flush_laps(before: dict, after: dict) -> dict:
    """The `FlushStage` laps across the same phase, in milliseconds per publication and per pool
    execution. The two halves are wall clock on two threads and are reported apart: the executor's
    are what an ack queues behind, the pool's are not. `publish_wall` overlaps the executor stages
    beside it rather than adding to them.
    """
    now, prior = after.get("flush_stages") or {}, before.get("flush_stages") or {}

    def delta(key):
        return now.get(key, 0) - prior.get(key, 0)

    def per(half, divisor):
        was = prior.get(half) or {}
        return {
            name: round((nanos - was.get(name, 0)) / 1e6 / divisor, 2) if divisor else None
            for name, nanos in (now.get(half) or {}).items()
        }

    publications, executions = delta("flushes"), delta("executions")
    return {
        "publications": publications,
        "executions": executions,
        "rows_published": delta("rows_published"),
        "executor_ms_per_publication": per("executor_nanos", publications),
        "pool_ms_per_execution": per("pool_nanos", executions),
    }


class Cycle:
    def __init__(self, args):
        self.args = args
        self.rung = Path(args.rung_dir)
        #: The bundle built from every row of the rung: the census reference.
        self.all_in = Path(args.all_in_bundle) if args.all_in_bundle else self.rung / "bundle"
        self.work = Path(args.work)
        self.binary = Path(args.binary)
        self.result: dict = {
            "fraction": args.fraction,
            "concurrency": args.concurrency,
            "seed": args.seed,
            "all_in_bundle": str(self.all_in),
        }
        #: The served deployment's `limits` block, read once the server is up.
        self.limits: dict | None = None
        #: Every declared view, the anchor first, and each one's frame off `/v1/meta`.
        self.views: list[dict] = []
        self.frames: dict[str, dict] = {}
        #: Credentials minted for this run, by variable name.
        self.minted: dict[str, str] = {}
        self.ladder: list[dict] | None = None
        #: The census's boxes per view, chosen once on the all-in deployment, and every layer
        #: `/v1/meta` publishes, which carries each one's declared levels and their zoom ranges.
        self.census_boxes: dict[str, list[tuple[int, list[float]]]] = {}
        self.meta_layers: list[dict] = []
        #: The served deployment, its session credential and the 100% principal's terms.
        self.served: Deployment | None = None
        self.session_cred = ""
        self.all_terms: list[str] = []

    @property
    def view_names(self) -> list[str]:
        return [view["name"] for view in self.views]

    @property
    def anchor(self) -> str:
        return self.views[0]["name"]

    def credentials(self) -> dict[str, str]:
        """A value for every credential or identity-key variable the environment does not carry,
        minted for this run only."""
        self.minted = minted_credentials(self.rung)
        self.result["minted_credentials"] = sorted(self.minted)
        if self.minted:
            self.log(f"minted a value for {', '.join(sorted(self.minted))} for this run")
        return self.minted

    def log(self, message: str) -> None:
        print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)

    # -- 1. split and build ---------------------------------------------------------------

    def build_base(self) -> Path:
        base_dir = self.work / f"base-{self.args.fraction:g}"
        bundle = base_dir / "bundle"
        base_ids, held = split_entities(
            declared_entities(self.rung), self.args.fraction, self.args.seed
        )
        self.result["base_rows"] = int(len(base_ids))
        self.result["holdout_rows"] = int(len(held))
        self.held = held
        if self.args.reuse_base and (bundle / "CURRENT").exists():
            self.log(f"reusing {bundle}")
            self.result["build"] = {"reused": True}
            return base_dir
        # `--reuse-base` also reuses prepared inputs whose build failed, where they hold the
        # entity count this fraction and seed give.
        prepared = base_dir / "base-inputs.json"
        reuse_inputs = (
            self.args.reuse_base
            and prepared.exists()
            and json.loads(prepared.read_text()).get("entities") == len(base_ids)
        )
        if reuse_inputs:
            self.log(f"reusing the prepared base inputs at {base_dir}")
        else:
            if base_dir.exists():
                shutil.rmtree(base_dir)
            self.log(f"splitting: base {len(base_ids):,} rows, hold-out {len(held):,} rows")
            write_base_inputs(self.rung, base_dir, base_ids)
            # Release what the split left in the pool: the build runs beside this process.
            pa.default_memory_pool().release_unused()
        if self.args.state_extent:
            self.result["stated_extent"] = state_extent(base_dir / "corpus.toml", self.all_in)
            self.log(f"stated the all-in frame in the base declaration: {self.result['stated_extent']}")
        # At f = 1.0 this is `tessera build` over an empty points file. `--mint-external-ids`,
        # since published artifacts name their members that way.
        self.result["build"] = build_bundle(
            self.binary,
            base_dir,
            bundle,
            base_dir / "stage-timings.json",
            extra=["--mint-external-ids", "--deployment", str(base_dir / "tessera.toml")],
        )
        if self.result["build"]["returncode"] != 0:
            self.result["blocked"] = {
                "at": "base build",
                "fraction": self.args.fraction,
                "refusal": self.result["build"]["stderr_tail"][-2000:],
            }
        return base_dir

    # -- 2. ingest ------------------------------------------------------------------------

    @staticmethod
    def served_limits(status: dict) -> dict:
        """The pagination units every route publishes: `/control/status`'s `limits` block.
        Raises if absent."""
        limits = status["limits"]
        for route, keys in {
            "ingest": ("max_batch_rows", "max_batch_bytes"),
            "publish": ("max_body_bytes", "max_artifacts_per_request"),
            "grow": ("max_body_bytes", "max_members_per_request"),
            "changes": ("max_changes_per_request",),
        }.items():
            for key in keys:
                int(limits[route][key])
        return limits

    def run_ingest(self, control: Control, source, label: str) -> dict:
        """Put `source`'s batches through `/control/ingest` at `C` concurrent callers. `source`
        yields `(first row index, body, rows)` as a generator, fed through a bounded window so
        a fast producer cannot outrun a slower server.
        """
        run_id = uuid.uuid4().hex[:8]
        acks: list[float] = []
        statuses: dict[str, int] = {}
        totals = {"accepted": 0, "minted": 0, "offered": 0, "batches": 0}
        lock = threading.Lock()

        def one(item):
            start, body, rows = item
            batch_id = f"{label}-{run_id}-{start}"
            session = requests.Session()
            r = dt = None
            # A 429 is backpressure, not a failure: the driver retries and counts every one.
            for _ in range(600):
                r, dt = control.ingest(body, batch_id, session)
                with lock:
                    statuses[str(r.status_code)] = statuses.get(str(r.status_code), 0) + 1
                if r.status_code == 429:
                    time.sleep(min(float(r.headers.get("retry-after", "1")), 5.0))
                    continue
                break
            return r, dt, rows

        t0 = time.perf_counter()
        window = max(2 * self.args.concurrency, 4)
        with concurrent.futures.ThreadPoolExecutor(max_workers=self.args.concurrency) as pool:
            pending: set = set()
            for item in source:
                pending.add(pool.submit(one, item))
                if len(pending) >= window:
                    done, pending = concurrent.futures.wait(
                        pending, return_when=concurrent.futures.FIRST_COMPLETED
                    )
                    self._collect(done, acks, totals)
            self._collect(pending, acks, totals)
        wall = time.perf_counter() - t0
        return {
            "batches": totals["batches"],
            "rows_offered": totals["offered"],
            "accepted": totals["accepted"],
            "minted": totals["minted"],
            "wall_s": round(wall, 2),
            "items_per_s": round(totals["accepted"] / wall, 1) if wall else None,
            "ack_ms": serve_battery.percentiles(acks),
            "statuses": statuses,
        }

    def _collect(self, futures, acks: list, totals: dict) -> None:
        for future in futures:
            r, dt, rows = future.result()
            acks.append(dt * 1000.0)
            totals["batches"] += 1
            totals["offered"] += rows
            if r.status_code == 200:
                body = r.json()
                totals["accepted"] += body["accepted"]
                totals["minted"] += body.get("minted", 0)
            elif "first_refusal" not in self.result:
                self.result["first_refusal"] = {"status": r.status_code, "body": r.text[:1500]}
            if totals["batches"] % 100 == 0:
                self.log(f"  {totals['batches']} batches, {totals['accepted']:,} accepted")

    # -- the whole thing ------------------------------------------------------------------

    def run(self) -> dict:
        """[`Cycle._run`], and a `failures` list whatever happened to it."""
        try:
            self._run()
        except Exception as e:  # noqa: BLE001 — a driver failure is a recorded outcome
            self.result["driver_failure"] = f"{type(e).__name__}: {e}"[:2000]
            self.log(f"DRIVER FAILED: {type(e).__name__}: {e}")
        self.result["failures"] = self.failures()
        return self.result

    def serve_base(self, base_dir: Path) -> Deployment | None:
        """Serve a copy of the base bundle on this run's own ports; None where it would not
        open. A base served directly is a different base once a publication or flush writes."""
        args = self.args
        scratch = self.work / f"serve-{args.fraction:g}"
        bundle = scratch / "bundle"
        if bundle.exists():
            shutil.rmtree(bundle)
        scratch.mkdir(parents=True, exist_ok=True)
        t0 = time.perf_counter()
        shutil.copytree(base_dir / "bundle", bundle)
        self.result["base_copy_s"] = round(time.perf_counter() - t0, 2)
        self.log(f"copied the base bundle in {self.result['base_copy_s']} s")
        served = Deployment(
            base_dir,
            bundle,
            scratch,
            (args.port0, args.port0 + 1, args.port0 + 2),
            self.binary,
            cap_bytes=args.cap_bytes,
            env=self.credentials(),
            ingest=json.loads(args.ingest_config) if args.ingest_config else None,
        )
        self.result["ingest_config"] = served.ingest
        served.clear_scratch()
        t0 = time.perf_counter()
        try:
            served.start()
        except Exception as e:  # a zero-row bundle that cannot be opened is the finding
            self.result["blocked"] = {"at": "serve", "refusal": str(e)[:2000]}
            self.log(f"BLOCKED at serve: {e}")
            return None
        self.result["open_s"] = round(time.perf_counter() - t0, 2)
        self.log(f"served pid={served.pid} open={self.result['open_s']} s")
        self.served = served
        return served

    def read_views(self) -> None:
        """Every declared view, the anchor first, each group's views among them, with the file
        each one's pass reads: a row in a second view joins an entity that must already exist."""
        self.views = [{**view, "name": view["id"]} for view in declared_views(self.rung)]
        self.result["views"] = self.view_names

    def open_session(self, served: Deployment, ranks: list[dict]) -> None:
        """The 100% principal's terms, every view's frame off `/v1/meta`, and the base's count."""
        self.session_cred = served.credential("session")
        self.all_terms = sorted(r["term"] for r in ranks)
        token, _ = serve_battery.authorise(served.session, self.session_cred, self.all_terms)
        m = serve_battery.meta(served.viewer, token)
        self.frames = {v["id"]: v["quantisation"] for v in m["views"]}
        self.result["base_visible"] = self.visible()

    def control_for(self, served: Deployment, view: str | None) -> Control:
        """A control client for one view's batches, unlabelled where the bundle has one view."""
        return Control(
            served.control,
            served.credential("operator"),
            view=view if len(self.views) > 1 else None,
        )

    def ingest_holdout(self, served: Deployment, cap: int, batch_rows: int) -> HoldOut:
        """One ingest pass per declared view, the anchor first; the anchor's hold-out is
        returned, since entities are allocated on its pass alone."""
        head = 3 * self.args.write_cycle_n if self.args.write_cycle else 0
        hold = None
        self.result["ingest_by_view"] = {}
        for entry in self.views:
            name = entry["name"]
            first = hold is None
            source = HoldOut(
                self.rung,
                self.held,
                cap,
                batch_rows,
                head_rows=head if first else 0,
                log=self.log,
                view=entry,
                members=first,
            )
            self.log(
                f"ingesting {len(self.held):,} rows into {name} from "
                f"{entry['points'].name} at C={self.args.concurrency}, bodies of at most "
                f"{batch_rows:,} rows under {cap:,} B (the served deployment's limits)"
            )
            figures = self.run_ingest(
                self.control_for(served, name), source.batches(), f"cycle-{name}"
            )
            figures["membership_columns"] = source.member_stats
            figures["max_body_bytes"] = cap
            figures["bodies_split"] = source.body_stats["bodies_split"]
            figures["largest_body_bytes"] = source.body_stats["largest_body_bytes"]
            figures["bodies_over_cap"] = source.body_stats["over_cap"]
            self.result["ingest_by_view"][name] = figures
            self.log(f"  {name}: {figures['items_per_s']} items/s")
            if first:
                hold = source
                self.result["ingest"] = figures
        return hold

    def phase(self, name: str, fn) -> None:
        """Run one named phase, recording a failure rather than ending the run: a failure here is
        as often a result — a shed stream, a refusal — as it is a bug in the driver. `phase_s` is
        the phase's own wall, whether it held or failed.
        """
        t0 = time.perf_counter()
        try:
            self.result[name] = fn()
        except Exception as e:  # noqa: BLE001 — a driver failure is a recorded outcome
            self.result[name] = {"failed": f"{type(e).__name__}: {e}"[:2000]}
            self.log(f"  {name} FAILED: {type(e).__name__}: {e}")
        if isinstance(self.result[name], dict):
            self.result[name]["phase_s"] = round(time.perf_counter() - t0, 1)

    def _run(self) -> None:
        args = self.args
        base_dir = self.build_base()
        if self.result.get("blocked"):
            self.log("BLOCKED at the base build; the refusal is in the result")
            return
        self.result["driver_rss"] = {"after_base": driver_rss()}

        served = self.serve_base(base_dir)
        if served is None:
            return
        try:
            self.read_views()
            if self.args.ranks:
                ranks_path = Path(self.args.ranks)
            else:
                ranks_path, self.result["ranks"] = ranks_for(self.rung, self.work)
            ranks = json.loads(ranks_path.read_text())
            self.open_session(served, ranks)

            control = self.control_for(served, self.anchor)
            self.result["ingested_view"] = control.view
            status = control.status()
            self.limits = self.served_limits(status)
            cap = int(self.limits["ingest"]["max_batch_bytes"])
            batch_rows = int(self.limits["ingest"]["max_batch_rows"])
            self.result["batch_rows"] = batch_rows
            before = status["write_executor"]
            # A key a batch's column names must already resolve, so its roster publishes first.
            self.result["publish_rosters"] = self.publish_rosters(control)
            hold = self.ingest_holdout(served, cap, batch_rows)
            self.result["executor_laps"] = executor_laps(
                before,
                control.status()["write_executor"],
                sum(f["accepted"] for f in self.result["ingest_by_view"].values()),
            )
            self.result["driver_rss"]["after_ingest"] = driver_rss()
            if args.stop_after_ingest:
                # The attribution cell, not the cycle: see `stop_after`.
                self.result["stop_after"] = "ingest"
                return

            # Every artifact is published after every point it depends on, before the flush.
            self.result["publish"] = self.publish_layers(control)
            self.result["driver_rss"]["after_publish"] = driver_rss()

            self.phase("flush", lambda: self.do_flush(control))
            self.phase("layers_after_ingest", self.probe_layers_after_ingest)
            self.phase("fold", lambda: self.do_fold(control))
            self.phase("equivalence", lambda: self.do_equivalence(ranks))
            if args.write_cycle:
                self.phase("write_cycle", lambda: self.do_write_cycle(control, hold))
                self.phase("restart", lambda: self.do_restart(ranks))
        finally:
            self.result["status_at_end"] = safe(
                lambda: self.control_for(served, None).status()
            )
            self.result["driver_rss"]["end"] = driver_rss()
            served.stop()

    # -- the artifacts, on the wire --------------------------------------------------------

    def publish_rosters(self, control: Control) -> dict:
        """A column-route layer's roster — keys, content, parents, no members — before the
        ingest, since every key the hold-out's rows name must already exist."""
        out: dict = {}
        work = self.work / f"roster-{self.args.fraction:g}"
        for layer in declared_layers(self.rung):
            if layer["route"] != "column" or layer["roster"] is None:
                continue
            name = layer["name"]
            if not layer["roster"].exists():
                out[name] = {
                    "failed": True,
                    "reason": f"roster {layer['roster'].name} is not in the rung directory",
                }
                continue
            self.log(f"  {name}: publishing the roster before the ingest, members empty")
            try:
                out[name] = self.publish_layer(
                    control, name, {**layer, "members": None}, work / name.replace("/", "__")
                )
            except Exception as e:  # noqa: BLE001 — the failure is the layer's record
                out[name] = {"failed": True, "reason": f"{type(e).__name__}: {e}"[:1500]}
                self.log(f"  {name}: ROSTER FAILED, {type(e).__name__}: {str(e)[:200]}")
        return out

    def publish_layers(self, control: Control) -> dict:
        """Publish every declared layer's whole roster and membership, or record why not, under
        `layers`, `on_column` or `declined`."""
        out: dict = {"layers": {}, "on_column": {}, "declined": {}}
        work = self.work / f"publish-{self.args.fraction:g}"
        for layer in declared_layers(self.rung):
            name = layer["name"]
            elsewhere = self.routed_elsewhere(layer)
            if elsewhere is not None:
                where, entry = elsewhere
                out[where][name] = entry
                continue
            try:
                out["layers"][name] = self.publish_layer(
                    control, name, layer, work / name.replace("/", "__")
                )
            except Exception as e:  # noqa: BLE001 — the failure is the layer's record
                out["declined"][name] = {"failed": True, "reason": f"{type(e).__name__}: {e}"[:1500]}
                self.log(f"  {name}: FAILED, {type(e).__name__}: {str(e)[:200]}")
        published = out["layers"].values()
        wall_s = round(sum(e["wall_s"] for e in published), 2)
        artifacts = sum(e["published_artifacts"] for e in published)
        members = sum(e["published_members"] for e in published)
        out["totals"] = {
            "artifacts": artifacts,
            "members": members,
            "wall_s": wall_s,
            "requests": sum(e["requests"] for e in published),
            "artifacts_per_s": round(artifacts / wall_s, 1) if wall_s else None,
            "members_per_s": round(members / wall_s, 1) if wall_s else None,
        }
        out["edges_declared"] = sum(e["edges_declared"] for e in published)
        out["edges_published"] = sum(e["edges_published"] for e in published)
        return out

    def routed_elsewhere(self, layer: dict) -> tuple[str, dict] | None:
        """Why this layer is not published here — `("on_column" | "declined", its record)` — or
        None for one the publication route takes."""
        name = layer["name"]
        if layer["route"] == "column":
            self.log(f"  {name}: nothing to publish, membership travelled on the ingest column")
            return "on_column", {
                "reason": "a per-point member table: the base build read it over the base's "
                "rows, and every hold-out row carried its own member list as the ingest "
                "batch's column named for the layer",
                "member_rows": pq.ParquetFile(layer["members"]).metadata.num_rows,
                "roster": (self.result.get("publish_rosters") or {}).get(name),
                "holdout": (self.result.get("ingest") or {}).get("membership_columns", {}).get(name),
            }
        if layer["attribute"] is not None:
            self.log(f"  {name}: nothing to publish, membership is the `{layer['attribute']}` attribute")
            return "declined", {
                "reason": f"membership is the `{layer['attribute']}` attribute column: the layer "
                f"has no roster and no member table, and an ingested row joins it through the "
                f"attribute its batch carries",
            }
        if layer["roster"] is None and layer["inline"]:
            self.log(f"  {name}: nothing to publish, its artifacts are written in the declaration")
            return "declined", {
                "reason": "artifacts written in the declaration: the base build carries them, "
                "and their members are computed at request time or named there",
            }
        if layer["roster"] is None:
            members = layer["members"]
            if layer["supplied"]:
                reason = (
                    "supplied content and no artifact roster: a layer declaring supplied content "
                    "refuses to mint from a column, and the publication route takes a roster"
                )
            else:
                reason = (
                    "no artifact roster and no member table: nothing names this layer's "
                    "artifacts, so there is nothing to mint at the build or to publish"
                )
            self.log(f"  {name}: NOT PUBLISHED, {reason.split(':')[0]}")
            return "declined", {
                "reason": reason + "; not published",
                "member_rows": pq.ParquetFile(members).metadata.num_rows
                if members is not None and members.exists()
                else None,
            }
        if not layer["roster"].exists():
            self.log(f"  {name}: NOT PUBLISHED, {layer['roster'].name} absent")
            return "declined", {
                "failed": True,
                "reason": f"roster {layer['roster'].name} is not in the rung directory",
            }
        return None

    def publish_layer(self, control: Control, name: str, layer: dict, work: Path) -> dict:
        """One layer: its bodies assembled and sent in turn, one caller, serial."""
        assert self.limits is not None, "the limits block is read before any layer is published"
        publication = Publication(
            layer["roster"],
            layer["members"],
            work,
            self.args.publish_max_bytes,
            self.args.publish_bucket_rows,
            self.limits,
            view_column=layer.get("view_column"),
        )
        try:
            sent = self.send_publication(control, name, publication)
        finally:
            publication.cleanup()
        stats = publication.stats
        declined = stats.pop("declined_artifacts")
        entry = dict(stats)
        entry.update(sent)
        entry["declined_artifacts"] = declined
        entry["declined_members"] = sum(d["members"] for d in declined)
        if entry["grown_members_unjoined"]:
            self.log(
                f"  {name}: {entry['grown_members_unjoined']:,} of {entry['grown_members']:,} "
                f"grown members did not join — the route already held them, or refused them"
            )
        self.log(
            f"  {name}: {entry['published_artifacts']:,} of {stats['artifacts']:,} artifacts "
            f"({entry['created_artifacts']:,} created), "
            f"{entry['published_members']:,} members in {entry['wall_s']:.1f} s "
            f"({entry['artifacts_per_s']} "
            f"artifacts/s, {entry['members_per_s']} members/s), {entry['requests']} requests, "
            f"{entry['refusals']} refused; {stats['grown_artifacts']:,} artifact(s) grown by "
            f"{entry['grow_requests']:,} PATCH(es) carrying {entry['grown_members']:,} members; "
            f"{entry['edges_published']:,}/{stats['edges_declared']:,} parent edges, "
            f"{stats['edges_dropped_to_declined']:,} dropped to declined parents, "
            f"{len(declined)} artifact(s) declined over the cap; {stats['read_path']}, "
            f"driver {entry['prepared_s']:.1f} s"
        )
        return entry

    def send_publication(self, control: Control, name: str, publication: Publication) -> dict:
        """Send one layer's bodies as they are assembled. `wall_s` is the requests' round
        trips, `prepared_s` the time in the body generator, `phase_s` the two together."""
        statuses: dict[str, int] = {}
        refusal = None
        refusals = 0
        first_by_status: dict[str, dict] = {}
        published = {"artifacts": 0, "members": 0, "edges": 0, "created": 0}
        grown = {"requests": 0, "members": 0, "joined": 0}
        prepared_s = 0.0
        wall_s = 0.0
        requests_n = 0
        session = requests.Session()
        t_phase = time.perf_counter()
        bodies = publication.bodies()
        while True:
            t0 = time.perf_counter()
            item = next(bodies, None)
            prepared_s += time.perf_counter() - t0
            if item is None:
                break
            if item[0] == "grow":
                _, level, key, body, members_n = item
                artifacts, edges = 0, 0
                r, dt = control.grow(name, body, session)
                grown["requests"] += 1
            else:
                _, level, body, artifacts, members_n, edges = item
                key = None
                r, dt = control.publish(name, body, session)
            del body
            wall_s += dt
            requests_n += 1
            statuses[str(r.status_code)] = statuses.get(str(r.status_code), 0) + 1
            if key is not None and r.status_code == 200:
                grown["members"] += members_n
                published["members"] += members_n
                try:
                    grown["joined"] += sum(int(a.get("joined") or 0) for a in r.json()["artifacts"])
                except (ValueError, KeyError, TypeError):
                    pass
            elif key is None and r.status_code in (200, 201):
                # 201 for a created artifact, 200 where the level already held the key.
                published["artifacts"] += artifacts
                published["members"] += members_n
                published["edges"] += edges
                try:
                    published["created"] += int(r.json().get("created") or 0)
                except (ValueError, KeyError, TypeError):
                    pass
            else:
                # Each status is logged with its detail the first time it appears.
                refusals += 1
                if refusal is None:
                    refusal = {"level": level, "status": r.status_code, "body": r.text[:1500]}
                if str(r.status_code) not in first_by_status:
                    first_by_status[str(r.status_code)] = {"level": level, "body": r.text[:1500]}
                    what = f"grow of {key!r}" if key is not None else f"{artifacts} artifact(s) in the batch"
                    self.log(
                        f"  {name}: REFUSED {r.status_code} at level {level} ({what}): {r.text[:300]}"
                    )
        return {
            "requests": requests_n,
            "prepared_s": round(prepared_s, 2),
            "wall_s": round(wall_s, 2),
            "phase_s": round(time.perf_counter() - t_phase, 2),
            "published_artifacts": published["artifacts"],
            "created_artifacts": published["created"],
            "published_members": published["members"],
            "edges_published": published["edges"],
            "artifacts_per_s": round(published["artifacts"] / wall_s, 1) if wall_s else None,
            "members_per_s": round(published["members"] / wall_s, 1) if wall_s else None,
            "statuses": statuses,
            "refusals": refusals,
            "first_refusal": refusal,
            "first_refusal_by_status": first_by_status,
            "grow_requests": grown["requests"],
            "grown_members": grown["members"],
            "grown_members_joined": grown["joined"],
            "grown_members_unjoined": grown["members"] - grown["joined"],
        }

    def probe_layers_after_ingest(self) -> dict:
        """One zoom-0 viewport with `layers: "all"` after the flush, timed and allowed to fail:
        a record change moves the level's version, so the next layered request rebuilds its row
        form and can be shed. Recorded rather than routed around."""
        served = self.served
        token, _ = serve_battery.authorise(served.session, self.session_cred, self.all_terms)
        t0 = time.perf_counter()
        try:
            s = serve_battery.viewport(
                served.viewer,
                token,
                self.anchor,
                0,
                full_box(self.frames[self.anchor]),
                k=1,
                layers="all",
                timeout=600,
            )
            return {
                "served": True,
                "wall_s": round(time.perf_counter() - t0, 2),
                "server_ms": s["server_ms"],
                "visible": s["counts"]["visible"],
            }
        except Exception as e:  # noqa: BLE001 — the shed is the finding
            return {
                "served": False,
                "wall_s": round(time.perf_counter() - t0, 2),
                "error": f"{type(e).__name__}: {e}"[:600],
            }

    # -- flush, fold, equivalence, write cycle ---------------------------------------------

    def do_flush(self, control) -> dict:
        before = control.status()["write_executor"]["flush"]["flushes"]
        expected = (self.result["base_visible"] or 0) + self.result["ingest"]["accepted"]
        t0 = time.perf_counter()
        code = control.flush().status_code
        request_s = time.perf_counter() - t0
        # Or nothing left to flush: the buffer can already be empty under the row trigger.
        def flush_landed() -> bool:
            flush = control.status()["write_executor"]["flush"]
            return flush["flushes"] > before or flush["buffered_items"] == 0

        published, publish_s = wait_for(flush_landed, timeout=self.args.flush_timeout)
        reached, visibility_s = wait_for(
            lambda: self.visible() >= expected, timeout=self.args.flush_timeout, interval=0.25
        )
        return {
            "status": code,
            "request_s": round(request_s, 4),
            "published": published,
            "publish_s": round(publish_s, 3),
            "expected_visible": expected,
            "visible": self.visible(),
            "visibility_reached": reached,
            "visibility_s": round(visibility_s, 3),
        }

    def do_fold(self, control) -> dict:
        """`POST /control/compact`, and the compaction block once a fold has landed."""
        before = control.status()["compaction"]
        code = control.compact().status_code

        # A fold the server discarded counts a failure and never lands, so either ends the wait.
        def settled() -> bool:
            now = control.status()["compaction"]
            return now["folds"] > before["folds"] or now["fold_failures"] > before["fold_failures"]

        settled_in_time, wall = wait_for(settled, timeout=self.args.fold_timeout, interval=1.0)
        compaction = control.status()["compaction"]
        done = settled_in_time and compaction["folds"] > before["folds"]
        return {
            "status": code,
            "completed": done,
            "observed_s": round(wall, 2),
            "fold_s": compaction.get("last_secs"),
            "fold_peak_rss_bytes": compaction.get("last_rss_bytes"),
            "folds": compaction.get("folds"),
            "fold_failures": compaction.get("fold_failures", 0) - before.get("fold_failures", 0),
            "live_rows": compaction.get("live_rows"),
        }

    def declared_levels(self, view: str) -> dict[str, list[int]]:
        """The levels each of one view's layers declares, from `/v1/meta`: what the census's zooms
        are chosen to reach and what its coverage is measured against."""
        return {
            layer["name"]: [level["level"] for level in layer["levels"]]
            for layer in self.meta_layers
            if view in layer["views"]
        }

    def choose_boxes(self, viewer: str, token: str) -> None:
        """Fill `census_boxes`: `--equivalence-boxes` boxes per census zoom per view, where the
        points are. Candidates are drawn from the seed and ranked by what the 100% principal sees
        at the census zoom itself, and the boxes are taken from the densest three deciles — a box
        with no artifacts in it compares nothing. Chosen once, on the all-in deployment, and asked
        of both deployments and every principal.
        """
        wanted = self.args.equivalence_boxes
        for name in self.view_names:
            quant = self.frames[name]
            rng = random.Random(self.args.seed)
            chosen: list[tuple[int, list[float]]] = []
            zooms = census_zooms(
                [
                    level["zoom"]
                    for layer in self.meta_layers
                    if name in layer["views"]
                    for level in layer["levels"]
                    if level.get("zoom")
                ]
            )
            for zoom in zooms:
                ranked = serve_battery.rank_by_density(
                    viewer,
                    token,
                    name,
                    zoom,
                    serve_battery.candidate_boxes(quant, zoom, CENSUS_CANDIDATES * wanted, rng),
                    quant,
                    0,
                    serve_battery.DEFAULT_MAX_TILES,
                    self.log,
                )
                pool = [
                    pair
                    for decile in reversed(serve_battery.decile_pools(ranked)[7:])
                    for pair in reversed(decile)
                    if pair[1] > 0
                ]
                step = max(len(pool) // wanted, 1)
                boxes = [box for box, _ in pool[::step]][:wanted] or [
                    box for box, _ in ranked[-wanted:]
                ]
                chosen += [(zoom, box) for box in boxes]
            self.log(f"  {name}: census boxes at zooms {zooms}, {wanted} each")
            self.census_boxes[name] = chosen

    def census_views(self, deployment, cred: str, ladder) -> dict:
        """One census per declared view, keyed by view, in the folded deployment's frames
        whichever deployment is asked, over the boxes chosen for that view."""
        return {
            name: census(
                deployment.viewer,
                deployment.session,
                cred,
                name,
                self.frames[name],
                ladder,
                self.census_boxes.get(name) or [],
            )
            for name in self.view_names
        }

    def visible(self) -> int:
        """The anchor view's masked count at zoom 0, under every term. `layers=None`, since a
        layered request rebuilds the level's row form after a growth and a poll needs only the
        count."""
        token, _ = serve_battery.authorise(
            self.served.session, self.session_cred, self.all_terms
        )
        return serve_battery.viewport(
            self.served.viewer, token, self.anchor, 0, full_box(self.frames[self.anchor]), k=1,
            layers=None,
        )["counts"]["visible"]

    def do_equivalence(self, ranks) -> dict:
        """The folded deployment's census against the all-in build's, per view, same boxes.
        `equal` is every view agreeing.

        The all-in deployment goes first: it publishes every layer's declaration, from which the
        census zooms follow, and it is the deployment the boxes are ranked on.
        """
        targets = [float(t) for t in self.args.targets.split(",")]
        ladder = serve_battery.compose_ladder(ranks, self.visible() or 1, targets)
        self.ladder = ladder

        # The all-in deployment, served on the three ports after the folded deployment's.
        allin = Deployment(
            self.rung,
            self.all_in,
            self.work / "serve-allin",
            (self.args.port0 + 3, self.args.port0 + 4, self.args.port0 + 5),
            self.binary,
            cap_bytes=self.args.cap_bytes,
            env=self.minted,
        )
        allin.clear_scratch()
        allin.start()
        try:
            reference_token, _ = serve_battery.authorise(
                allin.session, allin.credential("session"), self.all_terms
            )
            all_in_meta = serve_battery.meta(allin.viewer, reference_token)
            all_in_frames = {v["id"]: v["quantisation"] for v in all_in_meta["views"]}
            self.meta_layers = all_in_meta.get("layers") or []
            self.choose_boxes(allin.viewer, reference_token)
            reference = self.census_views(allin, allin.credential("session"), ladder)
        finally:
            allin.stop()
        folded = self.census_views(self.served, self.session_cred, ladder)
        out: dict = {
            "ladder": [{"target": r["target"], "terms": r["terms"]} for r in ladder],
            "views": {},
            # The censuses themselves, per view, under the two names a collator strips.
            "folded": folded,
            "all_in": reference,
        }
        by_surface: dict[str, int] = {}
        differences: list[dict] = []
        for name in self.view_names:
            compared = compare_census(folded[name], reference[name])
            # The frames, side by side: under `extent = "auto"` they differ at the margins.
            compared["frames"] = {"folded": self.frames[name], "all_in": all_in_frames.get(name)}
            compared["frames_equal"] = self.frames[name] == all_in_frames.get(name)
            # What the folded deployment's census reached: the zooms, and each layer's levels.
            compared["census_coverage"] = census_coverage(
                folded[name], self.declared_levels(name)
            )
            out["views"][name] = compared
            for surface, count in compared["differences_by_surface"].items():
                by_surface[surface] = by_surface.get(surface, 0) + count
            differences += [{"view": name, **d} for d in compared["differences"]]
        # The views together, so a reader asking whether the two deployments agree has one answer.
        out["differences_by_surface"] = by_surface
        out["differences"] = differences
        for surface in ("zoom0", "boxes", "layers", "parents"):
            out[f"{surface}_equal"] = by_surface.get(surface, 0) == 0
        out["equal"] = all(v["equal"] for v in out["views"].values())
        out["frames_equal"] = all(v["frames_equal"] for v in out["views"].values())
        out["incomplete"] = incomplete_sentences(folded) + incomplete_sentences(reference)
        return out

    def do_restart(self, ranks) -> dict:
        """Stop the server and reopen it over the same bundle, cache and WAL: counts and census
        must match those from before it stopped. Compared against itself, not the all-in build,
        since the write cycle suppresses rows the all-in deployment still serves."""
        served = self.served
        ladder = self.ladder or serve_battery.compose_ladder(
            ranks,
            self.visible() or 1,
            [float(t) for t in self.args.targets.split(",")],
        )
        before = self.census_views(served, self.session_cred, ladder)
        visible_before = self.visible()
        served.stop()
        t0 = time.perf_counter()
        served.start()
        open_s = round(time.perf_counter() - t0, 2)
        self.log(f"reopened pid={served.pid} in {open_s} s")
        after = self.census_views(served, self.session_cred, ladder)
        out = {
            "open_s": open_s,
            "visible": self.visible(),
            "visible_before": visible_before,
            "views": {
                name: compare_census(after[name], before[name]) for name in self.view_names
            },
        }
        out["census_equal"] = all(v["equal"] for v in out["views"].values())
        out["incomplete"] = incomplete_sentences(before) + incomplete_sentences(after)
        return out

    def send_changes(self, control, items: list[dict]) -> tuple[requests.Response, float]:
        """Every change, paged by the route's published record count. Returns the first refusal
        or the last acknowledgement, and the pages' total wall."""
        assert self.limits is not None, "the limits block is read before any change is sent"
        per_page = int(self.limits["changes"]["max_changes_per_request"])
        r, wall = control.changes(items[:per_page])
        for start in range(per_page, len(items), per_page):
            if r.status_code != 200:
                return r, wall
            r, page_wall = control.changes(items[start : start + per_page])
            wall += page_wall
        return r, wall

    def change_and_wait(self, control, op: str, ids: list[str], start_visible: int) -> dict:
        """One change op over `ids`, and the wait for the count it takes the deployment to."""
        r, wall = self.send_changes(control, [{"external_id": e, "op": op} for e in ids])
        target = start_visible - len(ids)
        reached, visibility_s = wait_for(
            lambda: self.visible() <= target, timeout=120, interval=0.25
        )
        return {
            "status": r.status_code,
            "body": r.text[:600] if r.status_code != 200 else None,
            "wall_s": round(wall, 3),
            "visibility_s": round(visibility_s, 3),
            "visibility_reached": reached,
            "visible_after": self.visible(),
        }

    def do_write_cycle(self, control, hold) -> dict:
        """1,000 deletes, 1,000 suppressions, 1,000 re-ingests, a fold, and the census again,
        addressed by `external_id` since a deleted holder never blocks a re-ingest of it."""
        if hold.head is None or hold.head.num_rows < 2:
            return {"skipped": "hold-out too small for a write cycle"}
        ids = [
            base64.b64encode(int(e).to_bytes(8, "little")).decode()
            for e in hold.head.column("entity_id").to_pylist()
        ]
        n = min(self.args.write_cycle_n, len(ids) // 3)
        if n == 0:
            return {"skipped": "hold-out too small for a write cycle"}
        start_visible = self.visible()
        # Deleted rows come back and suppressed ones do not.
        out: dict = {"n": n, "visible_before": start_visible, "expected_after_cycle": start_visible - n}

        for op, batch in (("delete", ids[:n]), ("suppress", ids[n : 2 * n])):
            out[op] = self.change_and_wait(control, op, batch, start_visible)
            start_visible = out[op]["visible_after"]

        # Re-ingest the deleted rows, the head's first `n`, under fresh batch ids: a deleted
        # holder never blocks a re-ingest, so these must be accepted rather than 409'd.
        reingest_bodies = hold.new_body_stats()

        def head_slice():
            for start in range(0, n, hold.batch_rows):
                chunk = hold.head.slice(start, min(hold.batch_rows, n - start))
                yield from hold.bodies(chunk, start, reingest_bodies)

        out["reingest"] = self.run_ingest(control, head_slice(), "recycle")
        out["reingest"]["bodies_split"] = reingest_bodies["bodies_split"]
        out["reingest"]["largest_body_bytes"] = reingest_bodies["largest_body_bytes"]
        out["reingest"]["bodies_over_cap"] = reingest_bodies["over_cap"]
        control.flush()
        folded = self.do_fold(control)
        out["fold"] = {
            key: folded[key]
            for key in ("completed", "observed_s", "fold_s", "fold_peak_rss_bytes", "fold_failures")
        }
        out["visible_after_cycle"] = self.visible()
        out["overlay"] = control.status()["overlay"]
        return out

    # -- what did not hold -----------------------------------------------------------------

    def failures(self) -> list[str]:
        """One short sentence per way this run did not hold; empty is a cycle that held."""
        out: list[str] = []
        result = self.result
        if result.get("driver_failure"):
            out.append(f"the driver stopped: {result['driver_failure']}")
        if result.get("blocked"):
            blocked = result["blocked"]
            refusal = " ".join(str(blocked.get("refusal", "")).split())[:300]
            out.append(f"the run was blocked at {blocked['at']}: {refusal}")
        for name, phase in (result.get("ingest_by_view") or {}).items():
            unexpected = {
                status: count
                for status, count in (phase.get("statuses") or {}).items()
                if status not in ("200", "429")
            }
            if unexpected:
                out.append(f"ingest into {name} was refused: {unexpected}")
            if phase.get("accepted") != phase.get("rows_offered"):
                out.append(
                    f"ingest into {name} accepted {phase.get('accepted')} of "
                    f"{phase.get('rows_offered')} rows offered"
                )
        for where in ("publish_rosters", "publish"):
            published = result.get(where) or {}
            groups = [published] if where == "publish_rosters" else [
                published.get("layers") or {},
                published.get("declined") or {},
            ]
            for group in groups:
                if not isinstance(group, dict):
                    continue
                for layer, entry in group.items():
                    if not isinstance(entry, dict):
                        continue
                    if entry.get("failed"):
                        out.append(f"{layer} was not published: {entry.get('reason')}")
                    elif entry.get("refusals"):
                        out.append(
                            f"{layer}: {entry['refusals']} publication request(s) were refused "
                            f"{entry.get('first_refusal', {}).get('status')}"
                        )
        for name in ("flush", "layers_after_ingest", "fold", "equivalence", "write_cycle", "restart"):
            phase = result.get(name)
            if isinstance(phase, dict) and phase.get("failed"):
                out.append(f"the {name} phase failed: {phase['failed']}")
        flush = result.get("flush") or {}
        if isinstance(flush, dict) and flush.get("visibility_reached") is False:
            out.append(
                f"the flush never reached the expected visible count: {flush.get('visible')} of "
                f"{flush.get('expected_visible')}"
            )
        for name in ("fold", "write_cycle"):
            fold = (result.get(name) or {}) if name == "fold" else (result.get(name) or {}).get("fold") or {}
            if not isinstance(fold, dict) or not fold:
                continue
            if fold.get("completed") is False:
                out.append(f"the {name}'s fold did not complete")
            if fold.get("fold_failures"):
                out.append(f"the {name}'s fold reported {fold['fold_failures']} failure(s)")
        equivalence = result.get("equivalence") or {}
        for name, compared in (equivalence.get("views") or {}).items():
            if not compared.get("equal"):
                out.append(
                    f"the census on {name} is unequal: {compared.get('differences_by_surface')}"
                )
            out += [
                f"the census on {name} proved nothing about a declared level: {sentence}"
                for sentence in coverage_failures(compared.get("census_coverage") or {})
            ]
        for name in ("equivalence", "restart"):
            phase = result.get(name)
            if isinstance(phase, dict):
                out += list(phase.get("incomplete") or [])
        cycle = result.get("write_cycle") or {}
        if isinstance(cycle, dict) and cycle and not cycle.get("skipped") and not cycle.get("failed"):
            for op in ("delete", "suppress"):
                step = cycle.get(op) or {}
                if step.get("status") != 200:
                    out.append(f"the write cycle's {op} was refused {step.get('status')}")
                if step.get("visibility_reached") is False:
                    out.append(
                        f"the write cycle's {op} never reached its expected visible count "
                        f"({step.get('visible_after')} visible)"
                    )
            reingest = cycle.get("reingest") or {}
            if reingest.get("accepted") != reingest.get("rows_offered"):
                out.append(
                    f"the write cycle's re-ingest accepted {reingest.get('accepted')} of "
                    f"{reingest.get('rows_offered')} rows offered"
                )
            if cycle.get("visible_after_cycle") != cycle.get("expected_after_cycle"):
                out.append(
                    f"the write cycle ended at {cycle.get('visible_after_cycle')} visible, "
                    f"expecting {cycle.get('expected_after_cycle')}"
                )
        restart = result.get("restart") or {}
        if isinstance(restart, dict) and restart and not restart.get("failed"):
            if restart.get("visible") != restart.get("visible_before"):
                out.append(
                    f"the restart answers {restart.get('visible')} visible where the deployment "
                    f"answered {restart.get('visible_before')} before it"
                )
            if not restart.get("census_equal"):
                out.append("the census after the restart is not the census before it")
        return out


def driver_rss() -> dict:
    """This process's `VmRSS` and `VmHWM`, in bytes, from `/proc/self/status`: a figure of every
    cell, since the driver runs beside the server it loads on the same box. `peak` only rises.
    """
    out: dict = {}
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmHWM:")):
            out[line.split(":")[0]] = int(line.split()[1]) * 1024
    return {"rss": out.get("VmRSS"), "peak": out.get("VmHWM")}


def safe(fn):
    try:
        return fn()
    except Exception as e:
        return {"error": str(e)[:400]}
