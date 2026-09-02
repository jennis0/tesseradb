"""Cold-start measurement: time-to-ready, then the first request of each kind on a fresh process."""
import base64, json, os, subprocess, sys, time, signal, requests

# The working directory the deployment copy, its `.env`, its cache and its WAL live in — never
# the rung's own directory, whose ports and cache a running demo already holds.
S = os.environ.get("COLD_START_WORK", os.getcwd())
BIN = os.environ.get("TESSERA_BIN", "/home/joe/code/tessera/target/release/tessera")
TOML = os.path.join(S, "tessera.toml")
VIEWER = "http://127.0.0.1:8211"
SESSION = "http://127.0.0.1:8212"
CRED = os.environ["TESSERA_MEDCPT_SESSION_CRED"]
TERMS = open("/home/joe/code/tessera/data/ladder/medcpt/branch-terms.txt").read().strip().split(",")
BBOX = [-17.894932670593263, -20.08621828079224, 20.183564109802248, 17.992278499603273]

def start(logname):
    log = open(os.path.join(S, logname), "w")
    p = subprocess.Popen([BIN, "serve", "--deployment", TOML], stdout=log, stderr=log,
                         cwd=S, start_new_session=True)
    t0 = time.monotonic()
    while True:
        if p.poll() is not None:
            raise RuntimeError("server exited: " + open(os.path.join(S, logname)).read()[-2000:])
        try:
            r = requests.get(VIEWER + "/readyz", timeout=2)
            if r.status_code == 200:
                return p, time.monotonic() - t0
        except Exception:
            pass
        time.sleep(0.05)

def stop(p):
    try:
        os.killpg(os.getpgid(p.pid), signal.SIGTERM)
    except Exception:
        p.terminate()
    try: p.wait(timeout=30)
    except Exception:
        os.killpg(os.getpgid(p.pid), signal.SIGKILL); p.wait()

def authorise():
    payload = base64.b64encode(json.dumps({"terms": TERMS}).encode()).decode()
    r = requests.post(SESSION + "/session/authorise", headers={"Authorization": "Bearer " + CRED},
                      json={"auth_data": payload}, timeout=900)
    r.raise_for_status()
    return r.json()["token"]

def viewport(tok, layers, zoom=0, k=0, highlight=None, timeout=900):
    body = {"view": "knn", "zoom": zoom, "bbox": BBOX, "k": k, "layers": layers}
    if highlight is not None:
        body["highlight"] = highlight
    r = requests.post(VIEWER + "/v1/viewport", headers={"Authorization": "Bearer " + tok},
                      json=body, timeout=timeout)
    raw = r.content
    if r.status_code != 200:
        raise RuntimeError("viewport %s: %r" % (r.status_code, raw[:400]))
    return len(raw)

def browse(tok, layer, timeout=900):
    r = requests.post(VIEWER + "/v1/artifacts/browse", headers={"Authorization": "Bearer " + tok},
                      json={"view": "knn", "layer": layer, "limit": 50}, timeout=timeout)
    if r.status_code != 200:
        raise RuntimeError("browse %s: %r" % (r.status_code, r.content[:400]))
    return r.json()

def timed(label, fn):
    t = time.monotonic(); out = fn(); el = time.monotonic() - t
    print("    %-46s %8.3f s" % (label, el)); sys.stdout.flush()
    return el, out

if __name__ == "__main__":
    KINDS = sys.argv[1:] or ["kmeans", "browse", "highlight"]
    # One artifact id for the highlight, resolved on a warm process before the timed runs.
    p, ready = start("warmup.log")
    try:
        tok = authorise()
        rows = browse(tok, "mesh/descriptors")["artifacts"]
        HL_ID = rows[0]["tessera_id"]
        print("highlight artifact:", HL_ID, rows[0].get("name"), rows[0]["masked_count"])
    finally:
        stop(p)

    def run_kind(kind, tok):
        if kind == "kmeans":
            return timed("viewport layers=[clusters/kmeans] zoom0", lambda: viewport(tok, ["clusters/kmeans"]))
        if kind == "mesh_viewport":
            return timed("viewport layers=[mesh/descriptors] zoom0", lambda: viewport(tok, ["mesh/descriptors"]))
        if kind == "browse":
            return timed("browse mesh/descriptors roots", lambda: browse(tok, "mesh/descriptors"))
        if kind == "highlight":
            hl = {"member_of": {"layer": "mesh/descriptors", "artifact": HL_ID}}
            return timed("viewport member_of highlight over mesh", lambda: viewport(tok, ["clusters/kmeans"], highlight=hl))
        raise SystemExit("unknown kind " + kind)

    for first in KINDS:
        print("=== fresh process, first request = %s" % first)
        p, ready = start("run-%s.log" % first)
        try:
            print("    %-46s %8.3f s" % ("time to /readyz 200", ready))
            el, _ = timed("authorise", authorise); tok = authorise()
            run_kind(first, tok)
            print("    -- warm repeat, then the other kinds --")
            run_kind(first, tok)
            for other in ["kmeans", "mesh_viewport", "browse", "highlight"]:
                if other != first:
                    run_kind(other, tok)
        finally:
            stop(p)
