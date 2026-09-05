"""What a merge costs the next request naming a large level.

Serves the rung-3 all-in bundle, warms, then ingests and flushes `--flushes` small batches (each
its own segment, all under the 16 MiB tier floor, so `tier_width` of them select a row-space
merge at the next tick). After every flush the zoom-0 `layers: "all"` request is timed — that is
the flush-extension route — and once the merge has published, the same request is timed twice:
the first is the merge arm (the held form no longer covers the row space, so the level is
projected whole on the request path), the second is the rebuilt form serving.

    TESSERA_BIN=target/release/tessera python3 probes/2026-09-05-merge-arm/merge_arm.py <scratch>
"""
import base64, json, os, sys, time, uuid
from pathlib import Path

sys.path.insert(0, "/home/joe/code/tessera/test_corpora")
import pyarrow as pa, pyarrow.ipc as ipc, pyarrow.parquet as pq, requests
from common import serve_battery
from common.deployment import Deployment
from common.ingest_cycle import Control, encode_batch, wire_columns, wait_for

R = Path("/home/joe/code/tessera")
rung = R / "data/ladder/medcpt"
scratch = Path(sys.argv[1])
FLUSHES = int(os.environ.get("FLUSHES", "4"))
ROWS = int(os.environ.get("ROWS_PER_FLUSH", "1000"))
out: dict = {"flushes": [], "rows_per_flush": ROWS}


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


ACCESS, ATTRIBUTES = wire_columns(rung)


def batch(points: pa.Table, start: int, n: int, offset: int) -> bytes:
    """`n` rows of the rung's own points, re-identified so they are new entities: the all-in bundle
    already holds every source entity, so an ingest under the source id would be an update."""
    rows = points.slice(start, n).combine_chunks()
    ids = pa.array([offset + i for i in range(n)], pa.uint64())
    rows = rows.set_column(rows.schema.get_field_index("entity_id"), "entity_id", ids)
    return encode_batch(rows, ACCESS, ATTRIBUTES)


d = Deployment(rung, Path(os.environ.get("BUNDLE", R / "data/ladder/.measure/medcpt36/allin/bundle")), scratch,
               (8181, 8182, 8183), Path(os.environ["TESSERA_BIN"]))
d.clear_scratch()
t0 = time.perf_counter(); d.start(); out["open_s"] = round(time.perf_counter() - t0, 1)
log(f"served pid={d.pid} open={out['open_s']} s")
try:
    control = Control(d.control, d.credential("operator"))
    cred = d.credential("session")
    terms = sorted(r["term"] for r in json.loads((rung / "branch-ranks.json").read_text()))
    token, _ = serve_battery.authorise(d.session, cred, terms)
    m = serve_battery.meta(d.viewer, token)
    view = m["views"][0]["id"]; q = m["views"][0]["quantisation"]
    full = [q["x_min"], q["y_min"], q["x_max"], q["y_max"]]

    def req(name):
        token, _ = serve_battery.authorise(d.session, cred, terms)
        t0 = time.perf_counter()
        try:
            s = serve_battery.viewport(d.viewer, token, view, 0, full, k=1, layers="all", timeout=900)
            r = {"served": True, "wall_s": round(time.perf_counter() - t0, 2),
                 "server_ms": round(s["server_ms"], 1), "visible": s["counts"]["visible"]}
        except Exception as e:  # the shed is the finding
            r = {"served": False, "wall_s": round(time.perf_counter() - t0, 2), "error": str(e)[:200]}
        log(f"{name}: {r}")
        return r

    def flush_stats():
        return control.status()["write_executor"]["flush"]

    def merges():
        return control.status()["write_executor"]["merges"]

    out["before"] = [req("before_1"), req("before_2")]
    # Only the rows the flushes need — the whole table is 13 GB resident and took the first run
    # ten minutes to load for 4,000 rows.
    needed = FLUSHES * ROWS
    points = next(pq.ParquetFile(rung / "points.parquet").iter_batches(batch_size=needed))
    points = pa.Table.from_batches([points])
    for i in range(FLUSHES):
        tag = f"merge-arm-{uuid.uuid4().hex[:8]}"
        r, _ = control.ingest(batch(points, i * ROWS, ROWS, 10**12 + i * ROWS), tag, requests.Session())
        if r.status_code != 200:
            out["ingest_refused"] = {"status": r.status_code, "body": r.text[:600]}
            log(f"ingest refused: {out['ingest_refused']}")
            break
        accepted = r.json().get("accepted")
        before = flush_stats()["flushes"]
        t0 = time.perf_counter(); control.flush()
        published, publish_s = wait_for(lambda: flush_stats()["flushes"] > before, timeout=600)
        entry = {"ingest_status": r.status_code, "accepted": accepted, "published": published,
                 "publish_s": round(publish_s, 2)}
        log(f"flush {i+1}: {entry}")
        entry["request"] = req(f"after_flush_{i+1}")
        out["flushes"].append(entry)
    merges_before = merges()
    t0 = time.perf_counter()
    merged, merge_wait_s = wait_for(lambda: merges() > merges_before, timeout=900, interval=1.0)
    out["merge"] = {"published": merged, "wait_s": round(merge_wait_s, 1), "merges": merges()}
    log(f"merge: {out['merge']}")
    out["after_merge"] = [req("after_merge_1"), req("after_merge_2")]
    out["status_at_end"] = flush_stats()
finally:
    import signal
    try:
        os.kill(d.pid, signal.SIGTERM)
    except Exception:
        pass
(scratch / "result.json").write_text(json.dumps(out, indent=1))
log("done")
