from __future__ import annotations

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib
import base64
import concurrent.futures
import json
import shutil
import subprocess
import time
import uuid
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import requests

from .. import serve_battery
from ..deployment import Deployment
from .census import census, compare_census
from .control import Control, wait_for
from .holdout import HoldOut
from .publication import Publication
from .split import (
    declared_layers,
    ranks_file,
    split_entities,
    state_extent,
    write_base_inputs,
)

# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------


def executor_laps(before: dict, after: dict, rows: int) -> dict:
    """The `WriteStage` laps across one ingest phase, µs per accepted row.

    Differenced rather than read absolute, because the laps are process totals and the base's own
    open may have closed windows before the phase began. **All zero without `bench-timing`** — the
    `bench_timing` flag is carried through so a reader can tell an uninstrumented binary from an
    idle executor. `unattributed` is the coarse `apply_nanos_total` minus the three `apply` laps
    plus whatever `submit\u2192receipt` sees beyond the executor's own stages; it is reported, not
    hidden, because the close's stages are meant to partition its wall clock.
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
    }


class Cycle:
    def __init__(self, args):
        self.args = args
        self.rung = Path(args.rung_dir)
        self.work = Path(args.work)
        self.binary = Path(args.binary)
        self.result: dict = {
            "fraction": args.fraction,
            "concurrency": args.concurrency,
            "seed": args.seed,
        }
        #: The served deployment's `limits` block, read once the server is up
        #: ([`Cycle.served_limits`]); every request is sized from it.
        self.limits: dict | None = None

    def log(self, message: str) -> None:
        print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)

    # -- 1. split and build ---------------------------------------------------------------

    def build_base(self) -> Path:
        base_dir = self.work / f"base-{self.args.fraction:g}"
        bundle = base_dir / "bundle"
        base_ids, held = split_entities(self.rung / "points.parquet", self.args.fraction, self.args.seed)
        self.result["base_rows"] = int(len(base_ids))
        self.result["holdout_rows"] = int(len(held))
        self.held = held
        if self.args.reuse_base and (bundle / "CURRENT").exists():
            self.log(f"reusing {bundle}")
            self.result["build"] = {"reused": True}
            return base_dir
        # **The split survives a failed build.** Writing rung 4's base inputs is a quarter of an
        # hour and 50 GB, and a build that dies after it — out of memory, out of disc — would
        # otherwise pay for it again. `--reuse-base` reuses a *prepared* base as well as a built
        # one, on the same test the split itself would apply: the points file exists and holds the
        # rows this fraction and seed ask for.
        prepared = base_dir / "points.parquet"
        reuse_inputs = (
            self.args.reuse_base
            and prepared.exists()
            and pq.ParquetFile(prepared).metadata.num_rows == len(base_ids)
        )
        if reuse_inputs:
            self.log(f"reusing the prepared base inputs at {base_dir}")
        else:
            if base_dir.exists():
                shutil.rmtree(base_dir)
            self.log(f"splitting: base {len(base_ids):,} rows, hold-out {len(held):,} rows")
            write_base_inputs(self.rung, base_dir, base_ids)
            # Whatever the split's last row group left in the pool goes back before the build,
            # which runs beside this process and needs the memory more.
            pa.default_memory_pool().release_unused()
        if self.args.state_extent:
            self.result["stated_extent"] = state_extent(
                base_dir / "corpus.toml", self.rung / "bundle"
            )
            self.log(f"stated the all-in frame in the base declaration: {self.result['stated_extent']}")
        stages = base_dir / "stage-timings.json"
        t0 = time.perf_counter()
        # **A zero-row base is a real case and it is the point of the f = 1.0 cell**: nothing is
        # held for the build at all, so this is `tessera build` over an empty points file, and
        # whether a deployment can start from one is the first thing this driver finds out.
        proc = subprocess.run(
            [
                str(self.binary),
                "build",
                # **The base's rows must be addressable by external id**, because the artifacts
                # published after the ingest name their members that way and most of those members
                # are base rows. The flag mints one per item from its source entity id
                # (`ExternalIdRow`), which is the form [`encode_batch`] sends for the hold-out.
                "--mint-external-ids",
                "--deployment",
                str(base_dir / "tessera.toml"),
                "--stage-timings-json",
                str(stages),
                "--stage-timings",
            ],
            cwd=base_dir,
            capture_output=True,
            text=True,
        )
        wall = time.perf_counter() - t0
        self.result["build"] = {
            "wall_s": round(wall, 2),
            "returncode": proc.returncode,
            "stdout": proc.stdout[-4000:],
            "stderr_tail": proc.stderr[-4000:],
            "stages": json.loads(stages.read_text()) if stages.exists() else None,
            "bundle_bytes": sum(p.stat().st_size for p in bundle.rglob("*") if p.is_file())
            if bundle.exists()
            else 0,
        }
        if proc.returncode != 0:
            self.result["blocked"] = {
                "at": "base build",
                "fraction": self.args.fraction,
                "refusal": proc.stderr[-2000:],
            }
        return base_dir

    # -- 2. ingest ------------------------------------------------------------------------

    @staticmethod
    def served_limits(status: dict) -> dict:
        """The pagination units every route publishes (ingest §2.1): `/control/status`'s
        `limits` block, one entry per route with its record count and byte cap.

        Every cap this driver sizes a request by is read from here and from nowhere else: the
        ingest route's `max_batch_rows` and `max_batch_bytes`, the publication route's
        `max_body_bytes` and `max_artifacts_per_request`, the growth route's `max_body_bytes`
        and `max_members_per_request`. A value carried by the driver, from a flag or a constant,
        would be a second number that can disagree with the one the route refuses over. To
        exercise the ingest split on a rung whose bodies are under 16 MiB, lower the server's own
        cap with `--ingest-config '{"ingest_max_batch_bytes": 262144}'`. A status without the
        block is a server this driver was not written for, and raises.
        """
        limits = status["limits"]
        for route, keys in {
            "ingest": ("max_batch_rows", "max_batch_bytes"),
            "publish": ("max_body_bytes", "max_artifacts_per_request"),
            "grow": ("max_body_bytes", "max_members_per_request"),
        }.items():
            for key in keys:
                int(limits[route][key])
        return limits

    def run_ingest(self, control: Control, source, label: str) -> dict:
        """Put `source`'s batches through `/control/ingest` at *C* concurrent callers.

        `source` yields `(first row index, body, rows)`. It is a **generator**, so the hold-out is
        encoded as it is sent rather than built in full first — and the pool is fed through a
        bounded window so a fast producer cannot put the whole corpus's encoded batches in memory
        in front of a slower server.
        """
        run_id = uuid.uuid4().hex[:8]
        acks: list[float] = []
        statuses: dict[str, int] = {}
        totals = {"accepted": 0, "minted": 0, "offered": 0, "batches": 0}
        lock = __import__("threading").Lock()

        def one(item):
            start, body, rows = item
            batch_id = f"{label}-{run_id}-{start}"
            session = requests.Session()
            r = dt = None
            # **A 429 is backpressure, not a failure**: the contract says retry, so the driver
            # does, and counts every one. A run that reported the 429 as a refusal would report
            # the server's own flow control as an error rate.
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
        args = self.args
        base_dir = self.build_base()
        if self.result.get("blocked"):
            self.log("BLOCKED at the base build; the refusal is in the result")
            return self.result
        self.result["driver_rss"] = {"after_base": driver_rss()}

        scratch = self.work / f"serve-{args.fraction:g}"
        # **A run that flushes writes into the bundle it serves.** Every publication adds a side
        # manifest, so a base is a *different* base after one cell has ingested into it — the next
        # `--reuse-base` run 409s on the hold-out its predecessor published. Serving a copy is what
        # makes the flag mean what it says; without it only the first cell over a given base is the
        # cell that was intended.
        bundle = base_dir / "bundle"
        if args.copy_base:
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
            return self.result
        self.result["open_s"] = round(time.perf_counter() - t0, 2)
        self.log(f"served pid={served.pid} open={self.result['open_s']} s")

        try:
            # The anchor view: the hold-out's rows are that row space's, and a bundle carrying
            # more than one refuses an unlabelled batch. Named from the declaration rather than
            # from `/v1/meta`'s order, which is creation order and not the anchor.
            declared = tomllib.loads((self.rung / "corpus.toml").read_text())
            views = [v["name"] for v in declared.get("view", [])]
            anchor = declared.get("allocation_view") or (views[0] if views else None)
            control = Control(
                served.control, served.credential("operator"),
                view=anchor if len(views) > 1 else None,
            )
            self.result["ingested_view"] = control.view
            session_cred = served.credential("session")
            ranks = json.loads(ranks_file(self.rung).read_text())
            all_terms = sorted(r["term"] for r in ranks)
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            m = serve_battery.meta(served.viewer, token)
            view = m["views"][0]["id"]
            quant = m["views"][0]["quantisation"]
            self.result["base_visible"] = serve_battery.viewport(
                served.viewer, token, view, 0,
                [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]], k=1
            )["counts"]["visible"]

            head = 3 * args.write_cycle_n if args.write_cycle else 0
            status = control.status()
            self.limits = self.served_limits(status)
            cap = int(self.limits["ingest"]["max_batch_bytes"])
            batch_rows = int(self.limits["ingest"]["max_batch_rows"])
            self.result["batch_rows"] = batch_rows
            hold = HoldOut(self.rung, self.held, cap, batch_rows, head_rows=head, log=self.log)
            self.log(
                f"ingesting {len(self.held):,} rows at C={args.concurrency}, bodies of at most "
                f"{batch_rows:,} rows under {cap:,} B (the served deployment's limits)"
            )
            before = status["write_executor"]
            self.result["ingest"] = self.run_ingest(control, hold.batches(), "cycle")
            self.result["ingest"]["membership_columns"] = hold.member_stats
            self.result["ingest"]["max_body_bytes"] = cap
            self.result["ingest"]["bodies_split"] = hold.body_stats["bodies_split"]
            self.result["ingest"]["largest_body_bytes"] = hold.body_stats["largest_body_bytes"]
            self.result["ingest"]["bodies_over_cap"] = hold.body_stats["over_cap"]
            self.result["executor_laps"] = executor_laps(
                before,
                control.status()["write_executor"],
                self.result["ingest"]["accepted"],
            )
            self.log(f"  {self.result['ingest']['items_per_s']} items/s")
            self.result["driver_rss"]["after_ingest"] = driver_rss()
            if args.stop_after_ingest:
                # **The attribution cell, not the cycle.** Everything after this measures
                # publication, flush and the fold; a run that only wants the executor's laps
                # pays ~an hour for figures it is not reading. The equivalence census is
                # therefore *absent* from such a run's result, not passed — see `stop_after`.
                self.result["stop_after"] = "ingest"
                return self.result

            # **Every artifact, after every point it depends on.** Before the flush, deliberately:
            # an ingested row is resolvable by its external id from the moment it is acked
            # (`Session::resolve_external_ids` consults the live map first), so the ordering the
            # ruling states — points before the artifacts that name them — is the only one there is.
            self.result["publish"] = self.publish_layers(control)
            self.result["driver_rss"]["after_publish"] = driver_rss()

            # **Each phase's failure is recorded and the run continues.** A cell that died at the
            # flush used to lose its ingest figures too, which are the expensive half; and a
            # failure here is as often a *result* — a shed stream, a refusal — as it is a bug in
            # the driver.
            def phase(name, fn):
                try:
                    self.result[name] = fn()
                except Exception as e:  # noqa: BLE001 — a driver failure is a recorded outcome
                    self.result[name] = {"failed": f"{type(e).__name__}: {e}"[:2000]}
                    self.log(f"  {name} FAILED: {type(e).__name__}: {e}")

            phase("flush", lambda: self.do_flush(control, served, session_cred, view, quant, all_terms))
            phase("layers_after_ingest", lambda: self.probe_layers_after_ingest(
                served, session_cred, view, quant, all_terms
            ))
            phase("fold", lambda: self.do_fold(control))
            phase("equivalence", lambda: self.do_equivalence(served, session_cred, view, quant, ranks))
            if args.write_cycle:
                phase("write_cycle", lambda: self.do_write_cycle(
                    control, served, session_cred, view, quant, all_terms, hold
                ))
        finally:
            self.result["status_at_end"] = safe(lambda: Control(served.control, served.credential("operator")).status())
            self.result["driver_rss"]["end"] = driver_rss()
            served.stop()
        return self.result

    # -- the artifacts, on the wire --------------------------------------------------------

    def publish_layers(self, control: Control) -> dict:
        """Publish every declared layer, or record why it was not. Its own figure.

        **The whole roster, and the whole of each artifact's membership**, base rows and ingested
        rows alike. An artifact exists because the layer declares it, so publishing only the ones
        whose members survived the split would make the two deployments differ in their *roster* as
        well as in their membership, which is a second variable in a test that has one.

        **Every declared layer is accounted for.** Each has an entry under `layers`, under
        `on_column`, or under `declined` with its reason: a column-route layer's membership rode
        the ingest batches (the module doc) and is recorded under `on_column` with what the hold-out
        carried; an attribute-membership layer has nothing to publish (an ingested row joins it
        through the column its batch carries); a layer with supplied content and no roster cannot
        be published; a roster whose publication failed carries the failure. Within a published
        layer, an artifact whose body alone would exceed the route's cap is declined per artifact
        and listed in the layer's `declined_artifacts`, so a census difference on the layer is
        attributable to named artifacts.

        Bodies are sent as they are assembled. `wall_s` is the sum of the requests' round trips,
        the service's cost, and `prepared_s` the time spent inside the body generator, the
        driver's; `phase_s` is the two together with whatever else the loop spent.
        """
        out: dict = {"layers": {}, "on_column": {}, "declined": {}}
        totals = {"artifacts": 0, "members": 0, "wall_s": 0.0, "requests": 0}
        work = self.work / f"publish-{self.args.fraction:g}"
        for layer in declared_layers(self.rung):
            name = layer["name"]
            if layer["route"] == "column":
                out["on_column"][name] = {
                    "reason": "no supplied content and no roster: the base build read the member "
                    "table over the base's rows, and every hold-out row carried its member list as "
                    "the ingest batch's column named for the layer (decision 0128)",
                    "member_rows": pq.ParquetFile(layer["members"]).metadata.num_rows,
                    "holdout": (self.result.get("ingest") or {}).get("membership_columns", {}).get(name),
                }
                self.log(f"  {name}: nothing to publish, membership travelled on the ingest column")
                continue
            if layer["attribute"] is not None:
                out["declined"][name] = {
                    "reason": f"membership is the `{layer['attribute']}` attribute column: the layer "
                    f"has no roster and no member table, and an ingested row joins it through the "
                    f"attribute its batch carries",
                }
                self.log(f"  {name}: nothing to publish, membership is the `{layer['attribute']}` attribute")
                continue
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
                out["declined"][name] = {
                    "reason": reason + "; not published",
                    "member_rows": pq.ParquetFile(members).metadata.num_rows
                    if members is not None and members.exists()
                    else None,
                }
                self.log(f"  {name}: NOT PUBLISHED, {reason.split(':')[0]}")
                continue
            if not layer["roster"].exists():
                out["declined"][name] = {"reason": f"roster {layer['roster'].name} is not in the rung directory"}
                self.log(f"  {name}: NOT PUBLISHED, {layer['roster'].name} absent")
                continue
            try:
                entry = self.publish_layer(control, name, layer, work / name.replace("/", "__"))
            except Exception as e:  # noqa: BLE001 — the failure is the layer's record
                out["declined"][name] = {"reason": f"{type(e).__name__}: {e}"[:1500]}
                self.log(f"  {name}: FAILED, {type(e).__name__}: {str(e)[:200]}")
                continue
            out["layers"][name] = entry
            totals["artifacts"] += entry["published_artifacts"]
            totals["members"] += entry["published_members"]
            totals["wall_s"] += entry["wall_s"]
            totals["requests"] += entry["requests"]
        totals["wall_s"] = round(totals["wall_s"], 2)
        totals["artifacts_per_s"] = (
            round(totals["artifacts"] / totals["wall_s"], 1) if totals["wall_s"] else None
        )
        totals["members_per_s"] = (
            round(totals["members"] / totals["wall_s"], 1) if totals["wall_s"] else None
        )
        out["totals"] = totals
        out["edges_declared"] = sum(e["edges_declared"] for e in out["layers"].values())
        out["edges_published"] = sum(e["edges_published"] for e in out["layers"].values())
        return out

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
        )
        statuses: dict[str, int] = {}
        refusal = None
        refusals = 0
        first_by_status: dict[str, dict] = {}
        published = {"artifacts": 0, "members": 0, "edges": 0}
        grown = {"requests": 0, "members": 0, "joined": 0}
        prepared_s = 0.0
        wall_s = 0.0
        requests_n = 0
        session = requests.Session()
        t_phase = time.perf_counter()
        bodies = publication.bodies()
        try:
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
                elif key is None and r.status_code == 201:
                    published["artifacts"] += artifacts
                    published["members"] += members_n
                    published["edges"] += edges
                else:
                    # **Never quiet.** Each status is logged with its detail the first time it
                    # appears, and every refusal is counted into the record and the summary line.
                    refusals += 1
                    if refusal is None:
                        refusal = {"level": level, "status": r.status_code, "body": r.text[:1500]}
                    if str(r.status_code) not in first_by_status:
                        first_by_status[str(r.status_code)] = {"level": level, "body": r.text[:1500]}
                        what = f"grow of {key!r}" if key is not None else f"{artifacts} artifact(s) in the batch"
                        self.log(
                            f"  {name}: REFUSED {r.status_code} at level {level} ({what}): {r.text[:300]}"
                        )
        finally:
            publication.cleanup()
        phase_s = time.perf_counter() - t_phase
        stats = publication.stats
        declined = stats.pop("declined_artifacts")
        entry = dict(stats)
        entry.update(
            {
                "requests": requests_n,
                "prepared_s": round(prepared_s, 2),
                "wall_s": round(wall_s, 2),
                "phase_s": round(phase_s, 2),
                "published_artifacts": published["artifacts"],
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
                "declined_artifacts": declined,
                "declined_members": sum(d["members"] for d in declined),
            }
        )
        if grown["members"] != grown["joined"]:
            self.log(
                f"  {name}: {grown['members'] - grown['joined']:,} of {grown['members']:,} grown "
                f"members did not join — the route already held them, or refused them"
            )
        self.log(
            f"  {name}: {published['artifacts']:,} of {stats['artifacts']:,} artifacts, "
            f"{published['members']:,} members in {wall_s:.1f} s ({entry['artifacts_per_s']} "
            f"artifacts/s, {entry['members_per_s']} members/s), {requests_n} requests, "
            f"{refusals} refused; {stats['grown_artifacts']:,} artifact(s) grown by "
            f"{grown['requests']:,} PATCH(es) carrying {grown['members']:,} members; "
            f"{published['edges']:,}/{stats['edges_declared']:,} parent edges, "
            f"{stats['edges_dropped_to_declined']:,} dropped to declined parents, "
            f"{len(declined)} artifact(s) declined over the cap; {stats['read_path']}, "
            f"driver {prepared_s:.1f} s"
        )
        return entry

    def probe_layers_after_ingest(self, served, session_cred, view, quant, all_terms) -> dict:
        """One zoom-0 viewport **with `layers: "all"`** after the flush, timed and allowed to fail.

        Its own measurement because it is the request that broke the first 3.6×10⁷ cell. The
        trigger is not the flush: it is the record change under a level — a publication here, a
        one-row growth before — which moves the level's version, after which the engine refuses the
        fold-written column and rebuilds the level's row form on the next layered request —
        94–113 s here, shed against `serve.stream_deadline_ms`. Reproduced with no flush in
        `probes/2026-09-03-growth-trigger/`; the fix is ruled in
        `docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`. Recorded rather than
        routed around.
        """
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]
        token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
        t0 = time.perf_counter()
        try:
            s = serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers="all", timeout=600
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

    def do_flush(self, control, served, session_cred, view, quant, all_terms) -> dict:
        before = control.status()["write_executor"]["flush"]["flushes"]
        expected = (self.result["base_visible"] or 0) + self.result["ingest"]["accepted"]
        t0 = time.perf_counter()
        code = control.flush().status_code
        request_s = time.perf_counter() - t0
        # **Or nothing left to flush.** Under the row trigger (write-path §4.1) a fast loader's
        # rows are published as they arrive, so the buffer can be empty when this request lands —
        # and a tick against an empty buffer publishes nothing and moves no counter. Waiting on
        # the counter alone then burns the whole `--flush-timeout` on a deployment that is already
        # fully visible, which is what this cell would otherwise report as a 900 s flush.
        def flush_landed() -> bool:
            flush = control.status()["write_executor"]["flush"]
            return flush["flushes"] > before or flush["buffered_items"] == 0

        published, publish_s = wait_for(flush_landed, timeout=self.args.flush_timeout)
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]

        # **`layers=None`, and that is not a detail.** A zoom-0 whole-extent viewport asking for
        # `layers: "all"` on a freshly-ingested 3.6×10⁷-row deployment was **shed mid-body** at
        # 113 s against a 60 s stream deadline: the layer probe's growth moved the mesh level's
        # version, so its row form is rebuilt from scratch on the first layered request after it.
        # That is a result about the read path after a record change (in `layers_after_ingest`),
        # not something a visibility poll should be measuring — what this needs is the masked
        # count, which the tiles frame carries on its own.
        def visible_now() -> int:
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            return serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers=None
            )["counts"]["visible"]

        reached, visibility_s = wait_for(
            lambda: visible_now() >= expected, timeout=self.args.flush_timeout, interval=0.25
        )
        return {
            "status": code,
            "request_s": round(request_s, 4),
            "published": published,
            "publish_s": round(publish_s, 3),
            "expected_visible": expected,
            "visible": visible_now(),
            "visibility_reached": reached,
            "visibility_s": round(visibility_s, 3),
        }

    def do_fold(self, control) -> dict:
        before = control.status()["compaction"]["folds"]
        code = control.compact().status_code
        done, wall = wait_for(
            lambda: control.status()["compaction"]["folds"] > before,
            timeout=self.args.fold_timeout,
            interval=1.0,
        )
        compaction = control.status()["compaction"]
        return {
            "status": code,
            "completed": done,
            "observed_s": round(wall, 2),
            "fold_s": compaction.get("last_secs"),
            "fold_peak_rss_bytes": compaction.get("last_rss_bytes"),
            "folds": compaction.get("folds"),
            "fold_failures": compaction.get("fold_failures"),
            "live_rows": compaction.get("live_rows"),
        }

    def do_equivalence(self, served, session_cred, view, quant, ranks) -> dict:
        """The folded deployment's census against the all-in build's, on the same boxes."""
        targets = [float(t) for t in self.args.targets.split(",")]
        token, _ = serve_battery.authorise(
            served.session, session_cred, sorted(r["term"] for r in ranks)
        )
        total = serve_battery.viewport(
            served.viewer, token, view, 0,
            [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]], k=1
        )["counts"]["visible"]
        ladder = serve_battery.compose_ladder(ranks, total or 1, targets)
        import random as _random

        rng = _random.Random(self.args.seed)
        boxes = []
        for zoom in (3, 6, 9):
            for box in serve_battery.candidate_boxes(quant, zoom, self.args.equivalence_boxes, rng):
                boxes.append((zoom, box))
        folded = census(served.viewer, served.session, session_cred, view, quant, ladder, boxes)
        folded_frame = quant

        # The all-in deployment, served beside it on its own ports and scratch: the three ports
        # after the folded deployment's, so a cycle takes six consecutive ports from `--port0`.
        scratch = self.work / "serve-allin"
        allin = Deployment(
            self.rung,
            self.rung / "bundle",
            scratch,
            (self.args.port0 + 3, self.args.port0 + 4, self.args.port0 + 5),
            self.binary,
        )
        allin.clear_scratch()
        allin.start()
        try:
            reference_token, _ = serve_battery.authorise(
                allin.session, allin.credential("session"), sorted(r["term"] for r in ranks)
            )
            all_in_meta = serve_battery.meta(allin.viewer, reference_token)
            all_in_frame = next(
                v for v in all_in_meta["views"] if v["id"] == view
            )["quantisation"]
            reference = census(
                allin.viewer, allin.session, allin.credential("session"), view, quant, ladder, boxes
            )
        finally:
            allin.stop()
        out = compare_census(folded, reference)
        # **The frames, side by side.** Under `extent = "auto"` they differ, and that difference
        # is what a box-level disagreement of a handful of rows is; recording them is what stops
        # the next reader attributing it to the write path.
        out["frames"] = {"folded": folded_frame, "all_in": all_in_frame}
        out["frames_equal"] = folded_frame == all_in_frame
        out["ladder"] = [{"target": r["target"], "terms": r["terms"]} for r in ladder]
        out["folded"] = folded
        out["all_in"] = reference
        return out

    def do_write_cycle(self, control, served, session_cred, view, quant, all_terms, hold) -> dict:
        """1,000 deletes, 1,000 suppressions, 1,000 re-ingests, a fold, and the census again.

        Addressed by `external_id` — the source entity id, as [`external_ids`] spells it — because
        that is the address an ingested row has that survives a delete: `tessera_id`s are per
        entity and a deleted holder does not block a re-ingest of the same external id (decision
        0047, edit is delete + re-ingest).
        """
        if hold.head is None or hold.head.num_rows < 2:
            return {"skipped": "hold-out too small for a write cycle"}
        ids = [
            base64.b64encode(int(e).to_bytes(8, "little")).decode()
            for e in hold.head.column("entity_id").to_pylist()
        ]
        n = min(self.args.write_cycle_n, len(ids) // 3)
        if n == 0:
            return {"skipped": "hold-out too small for a write cycle"}
        deletes = ids[:n]
        suppressions = ids[n : 2 * n]
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]

        def visible() -> int:
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            return serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers=None
            )["counts"]["visible"]

        start_visible = visible()
        out: dict = {"n": n, "visible_before": start_visible}

        assert self.limits is not None, "the limits block is read before any change is sent"
        per_page = int(self.limits["changes"]["max_changes_per_request"])
        for op, batch in (("delete", deletes), ("suppress", suppressions)):
            items = [{"external_id": external, "op": op} for external in batch]
            # Paged by the route's published record count, as publish and grow are; `r` is the
            # first refusal or the last acknowledgement, and `wall` the pages' total.
            wall = 0.0
            for start in range(0, len(items), per_page):
                r, page_wall = control.changes(items[start : start + per_page])
                wall += page_wall
                if r.status_code != 200:
                    break
            expected = start_visible - len(batch) if op == "delete" else None
            reached, visibility_s = wait_for(
                lambda: visible() <= (expected if expected is not None else start_visible - len(batch)),
                timeout=120,
                interval=0.25,
            )
            out[op] = {
                "status": r.status_code,
                "body": r.text[:600] if r.status_code != 200 else None,
                "wall_s": round(wall, 3),
                "visibility_s": round(visibility_s, 3),
                "visibility_reached": reached,
                "visible_after": visible(),
            }
            start_visible = out[op]["visible_after"]

        # Re-ingest: the deleted rows, under fresh batch ids. A deleted holder never blocks a
        # re-ingest (decision 0047), so these must be accepted rather than 409'd.
        # `deletes` is `ids[:n]`, so the rows to re-ingest are the head's first `n` — the same
        # bytes, under fresh batch ids, which is what makes this a re-ingest rather than a replay.
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
        before = control.status()["compaction"]["folds"]
        control.compact()
        done, wall = wait_for(
            lambda: control.status()["compaction"]["folds"] > before, timeout=self.args.fold_timeout, interval=1.0
        )
        compaction = control.status()["compaction"]
        out["fold"] = {
            "completed": done,
            "observed_s": round(wall, 2),
            "fold_s": compaction.get("last_secs"),
            "fold_peak_rss_bytes": compaction.get("last_rss_bytes"),
        }
        out["visible_after_cycle"] = visible()
        out["overlay"] = control.status()["overlay"]
        return out


def driver_rss() -> dict:
    """This process's `VmRSS` and `VmHWM`, in bytes, from `/proc/self/status`.

    The driver's resident set is a figure of every cell. It runs beside the server it loads, on
    the same box, and a driver that grows with the corpus stops the cell before the write path is
    measured. `peak` is the process's high-water mark and only rises.
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
