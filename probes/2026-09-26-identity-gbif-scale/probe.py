"""The identity measurements on a served GBIF bundle: `in` and `eq` on the unique `gbifid`, an
ingest of held-out and duplicate rows, a runtime `unique` declaration, a flush and a fold.

    python3 probe.py --corpus data/ladder/gbif --results results.json --phases open,lookups,...

Each phase appends its figures to `--results` as soon as it ends. The server runs under
`MemoryMax=--cap`, `MemorySwapMax=2G`, and is sampled once a second: `VmRSS`, `RssAnon` and the
scope's `memory.current`. A cold figure is taken on a freshly started server after the unique
index's files were dropped from the page cache with `posix_fadvise(POSIX_FADV_DONTNEED)`; the
residency before and after is `fincore`'s. Under WSL2 the host may still hold the bytes, so a cold
read is a read from the host's cache, not necessarily from the disk.
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import random
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq

HEAD, RECORDS, PAGE_END, TRAILER = 6, 7, 8, 4
BINARY = Path(__file__).resolve().parents[2] / "target" / "release" / "tessera"


# ----------------------------------------------------------------------------------- the server


def status_kib(pid: int) -> dict:
    out = {}
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            key = line.split(":", 1)[0]
            if key in ("VmRSS", "RssAnon", "RssFile", "VmHWM"):
                out[key] = int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError):
        pass
    return out


class Server:
    """`tessera serve` in `corpus`, capped, with a sampler thread."""

    def __init__(self, corpus: Path, cap: str):
        self.corpus, self.cap = corpus, cap
        env = dict(os.environ)
        for line in (corpus / ".env").read_text().splitlines():
            if "=" in line:
                key, value = line.split("=", 1)
                env[key] = value
        self.env = env
        self.session_cred = env["TESSERA_GBIF_SESSION_CRED"]
        self.operator_cred = env["TESSERA_GBIF_OPERATOR_CRED"]
        text = (corpus / "tessera.toml").read_text()
        self.viewer = "http://" + text.split('viewer  = "')[1].split('"')[0]
        self.session = "http://" + text.split('session = "')[1].split('"')[0]
        self.control = "http://" + text.split('control = "')[1].split('"')[0]
        self.samples: list[list] = []
        self.child = None

    def start(self) -> dict:
        cmd = ["systemd-run", "--user", "--scope", "--collect", "-p", f"MemoryMax={self.cap}",
               "-p", "MemorySwapMax=2G", "--", str(BINARY), "serve"]
        log = open(self.corpus / "serve.log", "ab")
        began = time.time()
        self.child = subprocess.Popen(cmd, cwd=self.corpus, env=self.env, stdout=subprocess.PIPE,
                                      stderr=log, start_new_session=True)
        print(f"serve pid {self.child.pid}", flush=True)
        self.began = began
        self.samples = []
        self.stop_sampling = threading.Event()
        threading.Thread(target=self._sample, args=(self.child, self.stop_sampling),
                         daemon=True).start()
        line = self.child.stdout.readline()
        announced = time.time() - began
        if not line:
            raise SystemExit(f"tessera serve exited {self.child.wait()}; see {self.corpus}/serve.log")
        while True:
            try:
                with urllib.request.urlopen(self.viewer + "/readyz", timeout=5) as r:
                    if r.status == 200:
                        break
            except (urllib.error.URLError, ConnectionError):
                pass
            time.sleep(0.2)
        ready = time.time() - began
        return {"announced_s": round(announced, 1), "ready_s": round(ready, 1),
                "memory_at_ready": self.memory()}

    def _sample(self, child: subprocess.Popen, stop: threading.Event) -> None:
        group = None
        while not stop.is_set() and child.poll() is None:
            if group is None or not group.name.endswith(".scope"):
                try:
                    rel = Path(f"/proc/{child.pid}/cgroup").read_text().strip().split("::")[1]
                    group = Path("/sys/fs/cgroup") / rel.lstrip("/")
                except (FileNotFoundError, IndexError):
                    group = None
            kib = status_kib(child.pid)
            current = None
            if group is not None:
                try:
                    current = int((group / "memory.current").read_text()) >> 10
                except (FileNotFoundError, ValueError):
                    pass
            self.samples.append([round(time.time() - self.began, 1), kib.get("VmRSS"),
                                 kib.get("RssAnon"), current])
            time.sleep(1.0)

    def memory(self) -> dict:
        kib = status_kib(self.child.pid)
        return {"rss_gib": round(kib.get("VmRSS", 0) / 2**20, 2),
                "anon_gib": round(kib.get("RssAnon", 0) / 2**20, 2),
                "hwm_gib": round(kib.get("VmHWM", 0) / 2**20, 2)}

    def peaks_since(self, t0: float) -> dict:
        window = [s for s in self.samples if s[0] >= t0 - self.began - 1]
        return {
            "peak_rss_gib": round(max((s[1] or 0) for s in window) / 2**20, 2) if window else None,
            "peak_anon_gib": round(max((s[2] or 0) for s in window) / 2**20, 2) if window else None,
            "peak_scope_gib": round(max((s[3] or 0) for s in window) / 2**20, 2) if window else None,
        }

    def stop(self) -> None:
        if self.child and self.child.poll() is None:
            os.kill(self.child.pid, signal.SIGTERM)
            self.child.wait(timeout=120)
        self.stop_sampling.set()

    # --------------------------------------------------------------------------- requests

    def request(self, url: str, body: bytes | None, headers: dict, method: str = "POST",
                timeout: float = 3600):
        req = urllib.request.Request(url, data=body, method=method, headers=headers)
        began = time.time()
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                data = r.read()
                return r.status, dict(r.headers), data, time.time() - began
        except urllib.error.HTTPError as e:
            return e.code, dict(e.headers), e.read(), time.time() - began

    def authorise(self, terms: list[str]) -> str:
        auth = base64.b64encode(json.dumps({"terms": terms}).encode()).decode()
        code, _, data, _ = self.request(
            self.session + "/session/authorise", json.dumps({"auth_data": auth}).encode(),
            {"authorization": f"Bearer {self.session_cred}", "content-type": "application/json"})
        assert code == 200, (code, data[:300])
        return json.loads(data)["token"]

    def control_call(self, path: str, body: bytes | None = None, method: str = "POST",
                     headers: dict | None = None, timeout: float = 36000):
        h = {"authorization": f"Bearer {self.operator_cred}"}
        h.update(headers or {})
        return self.request(self.control + path, body, h, method, timeout)

    def status(self) -> dict:
        code, _, data, _ = self.control_call("/control/status", method="GET")
        assert code == 200, (code, data[:300])
        return json.loads(data)

    def items(self, token: str, payload: dict) -> dict:
        """One whole read, cursor after cursor: the head, every row's `tessera_id` and `gbifid`
        where named, the server's own time summed over the responses' trailers, and the wall
        time. A body over the viewer plane's limit is sent with curl, which reads the answer the
        server gives before it closes the connection."""
        headers = {"authorization": f"Bearer {token}", "content-type": "application/json"}
        encoded = json.dumps(payload).encode()
        if len(encoded) > 2 * 1024 * 1024:
            return {"status": over_limit(self.viewer + "/v1/items", encoded, headers),
                    "request_bytes": len(encoded)}
        body = dict(payload)
        began = time.time()
        head = None
        stream_us = 0
        rows = 0
        values: list = []
        requests = 0
        while True:
            code, _, data, _ = self.request(self.viewer + "/v1/items", json.dumps(body).encode(),
                                            headers)
            requests += 1
            if code != 200:
                return {"status": code, "detail": data[:400].decode(errors="replace"),
                        "request_bytes": len(encoded)}
            got_head, batches, trailer = decode_items(data)
            head = head or got_head
            stream_us += trailer["stream_us"]
            for batch in batches:
                rows += batch.num_rows
                if "gbifid" in batch.schema.names:
                    values.extend(batch.column("gbifid").to_pylist())
            if trailer["next"] is None:
                break
            body = dict(payload)
            body.pop("count", None)
            body["cursor"] = trailer["next"]
        return {"status": 200, "head": head, "rows": rows, "values": values,
                "requests": requests, "server_ms": stream_us / 1000,
                "wall_s": time.time() - began, "request_bytes": len(encoded)}


def over_limit(url: str, body: bytes, headers: dict) -> str:
    path = Path("/tmp/claude-1000") / f"probe-body-{os.getpid()}.json"
    path.write_bytes(body)
    try:
        cmd = ["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", "--data-binary", f"@{path}",
               *[x for k, v in headers.items() for x in ("-H", f"{k}: {v}")], url]
        return subprocess.run(cmd, capture_output=True, text=True).stdout.strip()
    finally:
        path.unlink()


def decode_items(body: bytes):
    head, trailer, batches, pending = None, None, [], None
    at = 0
    while len(body) - at >= 5:
        kind, length = struct.unpack_from("<BI", body, at)
        payload = body[at + 5: at + 5 + length]
        at += 5 + length
        if kind == HEAD:
            head = json.loads(payload)
        elif kind == RECORDS:
            pending = ipc.open_stream(payload).read_next_batch()
        elif kind == PAGE_END:
            batches.append(pending)
        elif kind == TRAILER:
            trailer = json.loads(payload)
        else:
            raise ValueError(f"unknown frame kind {kind}")
    assert trailer is not None, "no trailer"
    return head, batches, trailer


# ----------------------------------------------------------------------------------- the cache


def unique_files(corpus: Path, field: str) -> list[Path]:
    return sorted((corpus / "bundle").glob(f"v*/partitions/*/entities/unique/{field}/*"))


def drop_cache(paths: list[Path]) -> dict:
    """Drops each file's pages from the page cache and reports the residency before and after."""
    def resident() -> int:
        total = 0
        for p in paths:
            out = subprocess.run(["fincore", "-b", "-n", "-o", "RES", str(p)],
                                 capture_output=True, text=True).stdout.strip()
            total += int(out or 0)
        return total
    before = resident()
    for p in paths:
        fd = os.open(p, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)
    return {"files": len(paths), "bytes": sum(p.stat().st_size for p in paths),
            "resident_before": before, "resident_after": resident()}


# ----------------------------------------------------------------------------------- the phases


def record(results_path: Path, key: str, value) -> None:
    results = json.loads(results_path.read_text()) if results_path.exists() else {}
    results[key] = value
    results_path.write_text(json.dumps(results, indent=1, default=str) + "\n")
    print(f"--- {key}: {json.dumps(value, default=str)[:800]}", flush=True)


def sample_values(corpus: Path, n: int, seed: int) -> pa.Table:
    """`n` rows' `gbifid` and `countrycode`, from row groups of `points.parquet` chosen at
    random across the file."""
    f = pq.ParquetFile(corpus / "points.parquet")
    rng = random.Random(seed)
    groups = sorted(rng.sample(range(f.metadata.num_row_groups),
                               min(f.metadata.num_row_groups, max(8, n // 20_000))))
    table = f.read_row_groups(groups, columns=["gbifid", "countrycode"])
    pick = np.array(sorted(rng.sample(range(table.num_rows), n)))
    return table.take(pa.array(pick))


def one_percent_term(corpus: Path) -> tuple[str, int, int]:
    ranks = json.loads((corpus / "country-ranks.json").read_text())
    total = sum(r["pairs"] for r in ranks)
    best = min(ranks, key=lambda r: abs(r["pairs"] - total / 100))
    return best["term"], best["pairs"], total


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", type=Path, required=True)
    ap.add_argument("--results", type=Path, required=True)
    ap.add_argument("--cap", default="24G")
    ap.add_argument("--phases", default="open,lookups,ingest,flush,declare,fold,reopen")
    ap.add_argument("--sizes", default="1000,100000,1000000")
    ap.add_argument("--warm", type=int, default=3)
    ap.add_argument("--batch-rows", type=int, default=0,
                    help="rows an ingest batch; 0 takes the server's own row cap")
    ap.add_argument("--declare", default="",
                    help="a field to declare unique at the running service, in the declare phase")
    args = ap.parse_args()
    corpus = args.corpus.resolve()
    phases = args.phases.split(",")
    server = Server(corpus, args.cap)
    all_terms = [r["term"] for r in json.loads((corpus / "country-ranks.json").read_text())]
    small, small_rows, total_rows = one_percent_term(corpus)

    def fresh(cold_field: str | None = None) -> dict:
        server.stop()
        opened = server.start()
        if cold_field:
            opened["dropped"] = drop_cache(unique_files(corpus, cold_field))
        return opened

    try:
        opened = server.start()
        if "open" in phases:
            status = server.status()
            record(args.results, "open", {**opened, "entity_id_high_water":
                                          status.get("entity_id_high_water"),
                                          "limits": status.get("limits")})

        if "lookups" in phases:
            sizes = [int(x) for x in args.sizes.split(",")]
            sample = sample_values(corpus, max(sizes), seed=7)
            gbifids = sample.column("gbifid").to_pylist()
            countries = sample.column("countrycode").to_pylist()
            principals = {"all": all_terms, "one": [small]}
            out = {"one_percent_term": small, "one_percent_rows": small_rows,
                   "total_rows": total_rows}

            def summary(runs: list[dict]) -> dict:
                return {"server_ms": round(statistics.median(r["server_ms"] for r in runs), 2),
                        "wall_s": round(statistics.median(r["wall_s"] for r in runs), 4)}

            for size in sizes:
                values = gbifids[:size]
                expected = {"all": sorted(values),
                            "one": sorted(v for v, c in zip(values, countries[:size]) if c == small)}
                for name, terms in principals.items():
                    key = f"in_{size}_{name}"
                    counted = {"view": "geo", "fields": ["gbifid"], "count": True,
                               "filters": {"gbifid": {"in": values}}}
                    plain = {"view": "geo", "fields": [], "filters": {"gbifid": {"in": values}}}
                    if len(json.dumps(counted)) > 2 * 1024 * 1024:
                        token = server.authorise(terms)
                        out[key] = server.items(token, counted)
                        record(args.results, key, out[key])
                        continue
                    dropped = fresh("gbifid")["dropped"]
                    token = server.authorise(terms)
                    cold = server.items(token, plain)
                    warm = [server.items(token, plain) for _ in range(args.warm)]
                    checked = server.items(token, counted)
                    counted_warm = [server.items(token, counted) for _ in range(args.warm)]
                    out[key] = {
                        "request_bytes": cold["request_bytes"], "dropped": dropped,
                        "expected": len(expected[name]), "rows": cold["rows"],
                        "matched": checked["head"]["matched"],
                        "visible": checked["head"]["visible"],
                        "rows_equal_expected": sorted(checked["values"]) == expected[name],
                        "cold": {"server_ms": cold["server_ms"], "wall_s": round(cold["wall_s"], 4),
                                 "requests": cold["requests"]},
                        "warm": summary(warm),
                        "with_count_and_gbifid": summary(counted_warm),
                    }
                    record(args.results, key, out[key])

            # One value by `eq`: for the whole-corpus principal, and for the small principal a
            # value held by an item it cannot see, which must answer as a value nobody holds.
            hidden = next(v for v, c in zip(gbifids, countries) if c != small)
            for name, terms, value in (("all", all_terms, gbifids[0]),
                                       ("one_hidden", [small], hidden)):
                payload = {"view": "geo", "filters": {"gbifid": {"eq": value}}, "fields": ["gbifid"]}
                dropped = fresh("gbifid")["dropped"]
                token = server.authorise(terms)
                runs = [server.items(token, payload) for _ in range(1 + args.warm)]
                key = f"eq_{name}"
                out[key] = {"value": value, "rows": runs[0]["values"], "dropped": dropped,
                            "cold": {"server_ms": runs[0]["server_ms"],
                                     "wall_s": round(runs[0]["wall_s"], 4)},
                            "warm": summary(runs[1:])}
                record(args.results, key, out[key])
            record(args.results, "lookups", out)

        if "ingest" in phases:
            limits = server.status()["limits"]["ingest"]
            batch_rows = args.batch_rows or limits["max_batch_rows"]
            holdout = pq.read_table(corpus / "holdout.parquet")
            duplicates = pq.read_table(corpus / "duplicates.parquet")
            out = {"limits": limits, "batch_rows": batch_rows,
                   "holdout_rows": holdout.num_rows, "duplicate_rows": duplicates.num_rows}
            fresh("gbifid")
            t0 = time.time()
            accepted = refused = 0
            answers: dict = {}
            for i, start in enumerate(range(0, holdout.num_rows, batch_rows)):
                code, _, data, _ = server.control_call(
                    "/control/ingest", arrow_body(holdout.slice(start, batch_rows)),
                    headers={"content-type": "application/vnd.apache.arrow.stream",
                             "x-tessera-batch-id": f"holdout-{i}"})
                answers[code] = answers.get(code, 0) + 1
                if code == 200:
                    accepted += min(batch_rows, holdout.num_rows - start)
                else:
                    refused += min(batch_rows, holdout.num_rows - start)
                    out.setdefault("holdout_refusals", []).append(data[:400].decode(errors="replace"))
            wall = time.time() - t0
            out.update(holdout_accepted=accepted, holdout_refused=refused,
                       holdout_answers=answers, holdout_wall_s=round(wall, 1),
                       holdout_rows_per_s=round(accepted / wall, 1),
                       memory=server.peaks_since(t0))
            record(args.results, "ingest", out)

            t0 = time.time()
            codes: dict = {}
            latencies = []
            for i in range(duplicates.num_rows):
                code, _, data, took = server.control_call(
                    "/control/ingest", arrow_body(duplicates.slice(i, 1)),
                    headers={"content-type": "application/vnd.apache.arrow.stream",
                             "x-tessera-batch-id": f"duplicate-{i}"})
                codes[code] = codes.get(code, 0) + 1
                latencies.append(took)
                if i == 0:
                    out["duplicate_first_detail"] = data[:600].decode(errors="replace")
            wall = time.time() - t0
            latencies.sort()
            out.update(duplicate_answers=codes, duplicate_wall_s=round(wall, 1),
                       duplicate_p50_ms=round(latencies[len(latencies) // 2] * 1000, 2),
                       duplicate_p99_ms=round(latencies[int(len(latencies) * 0.99)] * 1000, 2))
            record(args.results, "ingest", out)

        if "flush" in phases:
            before = server.status()
            t0 = time.time()
            code, _, data, took = server.control_call("/control/flush?wait=visible")
            after = server.status()
            flushed = {"status": code, "answer": json.loads(data or b"null"),
                       "wait_s": round(took, 1), "memory": server.peaks_since(t0),
                       "live_rows_before": before["compaction"]["live_rows"],
                       "live_rows_after": after["compaction"]["live_rows"],
                       "high_water_before": before["entity_id_high_water"],
                       "high_water_after": after["entity_id_high_water"],
                       "buffered_before": before["write_executor"]["flush"]["buffered_items"],
                       "flushes_after": after["write_executor"]["flush"]["flushes"],
                       "flush_stages": after["write_executor"]["flush_stages"]}
            record(args.results, "flush", flushed)
            # A sample of the held-out rows is served, and each duplicate's value is held once.
            token = server.authorise(all_terms)
            for name, path, n in (("holdout", "holdout.parquet", 100_000),
                                  ("duplicates", "duplicates.parquet", 10_000)):
                ids = pq.read_table(corpus / path, columns=["gbifid"]).column("gbifid")
                ids = ids.to_pylist()[:: max(1, len(ids) // n)][:n]
                got = server.items(token, {"view": "geo", "fields": ["gbifid"], "count": True,
                                           "filters": {"gbifid": {"in": ids}}})
                flushed[f"{name}_checked"] = {
                    "asked": len(ids), "status": got["status"],
                    "matched": got.get("head", {}).get("matched"), "rows": got.get("rows"),
                    "each_once": sorted(got.get("values", [])) == sorted(ids),
                    "detail": got.get("detail"), "wall_s": round(got.get("wall_s", 0), 1)}
                record(args.results, "flush", flushed)

        if "declare" in phases and args.declare:
            before = shutil.disk_usage(corpus).free
            t0 = time.time()
            code, _, data, took = server.control_call(
                "/control/attributes",
                json.dumps({"name": args.declare, "title": "Occurrence ID", "type": "keyword",
                            "unique": True}).encode(),
                method="PUT", headers={"content-type": "application/json"})
            record(args.results, "declare", {
                "field": args.declare, "status": code,
                "answer": data[:3000].decode(errors="replace"), "wall_s": round(took, 1),
                "memory": server.peaks_since(t0), "free_before_gib": before >> 30,
                "free_after_gib": shutil.disk_usage(corpus).free >> 30,
                "index_files": {str(p.relative_to(corpus)): p.stat().st_size
                                for p in unique_files(corpus, args.declare)}})

        if "fold" in phases:
            before = server.status().get("compaction")
            t0 = time.time()
            free_before = shutil.disk_usage(corpus).free
            least = free_before
            ended = lambda c: c["folds"] + c["fold_refusals"] + c["fold_failures"]  # noqa: E731
            code, _, _, _ = server.control_call("/control/compact")
            seen = []
            while True:
                time.sleep(2)
                least = min(least, shutil.disk_usage(corpus).free)
                now = server.status()["compaction"]
                if ended(now) > ended(before):
                    seen.append(now)
                    break
            record(args.results, "fold", {
                "status": code, "before": before, "after": seen[-1] if seen else None,
                "transitions": seen[:20], "wall_s": round(time.time() - t0, 1),
                "memory": server.peaks_since(t0), "free_before_gib": free_before >> 30,
                "free_least_gib": least >> 30,
                "unique_files": {str(p.relative_to(corpus)): p.stat().st_size
                                 for p in unique_files(corpus, "gbifid")}})

        if "reopen" in phases:
            opened = fresh()
            token = server.authorise(all_terms)
            holdout_ids = pq.read_table(corpus / "holdout.parquet",
                                        columns=["gbifid"]).column("gbifid").to_pylist()[:1000]
            got = server.items(token, {"view": "geo", "fields": [], "count": True,
                                       "page_rows": 1, "pages": 1,
                                       "filters": {"gbifid": {"in": holdout_ids}}})
            record(args.results, "reopen", {**opened, "holdout_1000_matched":
                                            got["head"]["matched"],
                                            "visible": got["head"]["visible"]})
    finally:
        server.stop()
        record(args.results, "server_samples_tail", server.samples[-5:])
    return 0


def arrow_body(table: pa.Table) -> bytes:
    """The ingest columns: the view's coordinates, the access labels and every attribute."""
    table = table.rename_columns(["access" if c == "countrycode" else c
                                  for c in table.column_names])
    sink = io.BytesIO()
    with ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    return sink.getvalue()


if __name__ == "__main__":
    sys.exit(main())
