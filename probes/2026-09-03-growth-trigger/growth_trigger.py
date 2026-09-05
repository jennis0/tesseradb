"""Does one row of growth into the mesh level, with no flush, cost the next all-layers request the
full rebuild? Serves the rung-3 all-in bundle, warms, measures the request, grows the level by one
probe row (then deletes it, no flush), measures again, twice."""
import json, sys, time, uuid, base64, os
from pathlib import Path

sys.path.insert(0, "/home/joe/code/tessera/test_corpora")
import requests, pyarrow.parquet as pq
from common import serve_battery
from common.deployment import Deployment
from common.ingest_cycle import Control, probe_batch

R = Path("/home/joe/code/tessera")
rung = R / "data/ladder/medcpt"
scratch = Path(sys.argv[1])
out = {}
def log(m): print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)

d = Deployment(rung, R / "data/ladder/.measure/medcpt36/allin/bundle", scratch,
               (8181, 8182, 8183), Path(os.environ.get("TESSERA_BIN", R / "target/release/tessera")))
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
            r = {"served": True, "wall_s": round(time.perf_counter() - t0, 2), "server_ms": s["server_ms"]}
        except Exception as e:
            r = {"served": False, "wall_s": round(time.perf_counter() - t0, 2), "error": str(e)[:300]}
        out[name] = r; log(f"{name}: {r}")

    req("before_1"); req("before_2")
    keys = pq.read_table(rung / "mesh-descriptors.parquet", columns=["key"]).column("key").to_pylist()[:3]
    pid = f"growth-probe-{uuid.uuid4().hex[:12]}"
    r, _ = control.ingest(probe_batch(pq.read_table(rung / "points.parquet").slice(0, 1), pid, "mesh/descriptors", keys), pid, requests.Session())
    out["grow"] = {"status": r.status_code, "body": r.text[:300]}; log(f"grow: {out['grow']}")
    w, _ = control.changes([{"external_id": base64.b64encode(pid.encode()).decode(), "op": "delete"}])
    out["withdraw"] = w.status_code
    st = control.status()["write_executor"]["flush"]; out["flushes_after_grow"] = st["flushes"]; log(f"flush counter {st}")
    req("after_growth_1"); req("after_growth_2")
    out["status"] = control.status()
finally:
    d.stop() if hasattr(d, "stop") else None
    import os, signal
    try: os.kill(d.pid, signal.SIGTERM)
    except Exception: pass
(scratch / "result.json").write_text(json.dumps(out, indent=1))
log("done")
