"""The whole-extent artifact request on GBIF, before and after: one capped server per phase.

    python3 measure.py <binary> <label> <scratch>

Phase `levels`: levels 0, 1 and 2 for each principal, each on a new token, then once more on a
second new token with the same terms. Phase `overlap`, on a fresh server: the 100% opening as the
bench sends it (counts, then the depth-7 points request and the level-0 artifact request
together), then a points request with no layer alongside a level-1 artifact request, each on a
new token. Writes `<label>.json` beside this file.
"""
import base64, json, struct, sys, threading, time
from pathlib import Path

import requests

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from test_corpora.common.deployment import Deployment, read_env_file, tomllib  # noqa: E402
from test_corpora.common.interactive_bench import principals  # noqa: E402

DEP = Path("/home/joe/code/tessera/data/ladder/gbif")
PORTS = (8991, 8992, 8993)
VIEWER, SESSION = f"http://127.0.0.1:{PORTS[0]}", f"http://127.0.0.1:{PORTS[1]}"
CRED = read_env_file(DEP / ".env")["TESSERA_GBIF_SESSION_CRED"]
PEOPLE = {p["label"]: p["terms"] for p in principals(json.loads((DEP / "country-ranks.json").read_text()))}
FULL = [0.00000762939453125, 0.00000762939453125, 0.9999923706054688, 0.9999923706054688]


def token(label):
    auth = base64.b64encode(json.dumps({"terms": PEOPLE[label]}).encode()).decode()
    r = requests.post(f"{SESSION}/session/authorise", headers={"authorization": f"Bearer {CRED}"},
                      json={"auth_data": auth}, timeout=900)
    r.raise_for_status()
    return r.json()["token"]


def post(tok, body):
    t0 = time.perf_counter()
    r = requests.post(f"{VIEWER}/v1/viewport", headers={"authorization": f"Bearer {tok}"}, json=body,
                      stream=True, timeout=1800)
    headers = time.perf_counter() - t0
    size = 0
    for chunk in r.iter_content(1 << 20):
        size += len(chunk)
    return {"status": r.status_code, "headers_s": round(headers, 3),
            "last_s": round(time.perf_counter() - t0, 3), "bytes": size}


def artifacts(level):
    return {"view": "geo", "zoom": 0, "bbox": FULL, "k": 0, "layers": ["taxonomy/tree"],
            "levels": [level], "computed": ["centroid", "box"], "artifact_budget": 84}


def marks(zoom):
    return {"view": "geo", "zoom": zoom, "bbox": FULL, "k": 500, "point_rows": [],
            "layers": ["taxonomy/tree"], "artifact_budget": 84, "levels": [0]}


def points(zoom):
    return {"view": "geo", "zoom": zoom, "bbox": FULL, "k": 500, "layers": []}


def counts(zoom=8):
    return {"view": "geo", "zoom": zoom, "bbox": FULL, "k": 0, "layers": []}


def together(*jobs):
    out = [None] * len(jobs)
    threads = []
    for i, (body, tok) in enumerate(jobs):
        def go(i=i, body=body, tok=tok):
            out[i] = post(tok, body)
        threads.append(threading.Thread(target=go))
        threads[-1].start()
        time.sleep(0.005)
    for t in threads:
        t.join()
    return out


def server(binary, scratch):
    settings = tomllib.loads((DEP / "tessera.toml").read_text())
    bundle = (DEP / settings["bundle"]["path"]).resolve()
    d = Deployment(DEP, bundle, scratch, PORTS, binary, cap_bytes=24 * 2**30, swap_bytes=2 * 2**30)
    d.clear_scratch()
    t = time.time()
    d.start(timeout=1800)
    print(f"server pid={d.pid} open_s={time.time() - t:.0f}", flush=True)
    return d


def levels_phase(out):
    for level in (0, 1, 2):
        for label in ("100%", "85%", "7%", "1%"):
            for which in ("new", "second session"):
                r = post(token(label), artifacts(level))
                r.update(level=level, principal=label, which=which)
                print(json.dumps(r), flush=True)
                out.append(r)


def overlap_phase(out):
    tok = token("100%")
    c = post(tok, counts())
    m, a = together((marks(7), tok), (artifacts(0), tok))
    out.append({"case": "bench opening, 100%", "counts": c, "marks": m, "artifacts": a})
    print(json.dumps(out[-1]), flush=True)
    tok = token("85%")
    p, a = together((points(7), tok), (artifacts(1), tok))
    out.append({"case": "points with no layer beside a level-1 build, 85%", "points": p, "artifacts": a})
    print(json.dumps(out[-1]), flush=True)
    tok = token("85%")
    out.append({"case": "points with no layer alone, 85%", "points": post(tok, points(7))})
    print(json.dumps(out[-1]), flush=True)


def main():
    binary, label, scratch = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    result = {"binary": str(binary), "levels": [], "overlap": []}
    for phase, run in (("overlap", overlap_phase), ("levels", levels_phase)):
        d = server(binary, scratch)
        try:
            run(result[phase])
        finally:
            d.stop()
    (Path(__file__).parent / f"{label}.json").write_text(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
