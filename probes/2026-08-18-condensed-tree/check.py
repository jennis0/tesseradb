"""The check Stage 5 exists for, on the real condensed tree.

**A principal holding only the term that covers a parent's stray members sees that parent and no
child of it** — and the build's containment report named that split in advance.

Why it is the check. HDBSCAN's children are subsets of their parents and do not exhaust them: a
fifth to a quarter of a parent's points fall out as noise at each split. So a parent keeps members
no child holds, and a viewer who can see those members and nothing else has a positive masked count
on the parent and **zero on every child**. The parent is served alone. Any construction that
assumed a covering hierarchy — a rollup that unions the children and calls the result the parent, a
cut that requires a child before it will draw an ancestor — gets this wrong, and gets it wrong
invisibly, because every number it reports is individually plausible.

**The tree is real; the grant is the instrument.** The hierarchy comes from
`notebooks/arxiv-corpus.ipynb`, which is HDBSCAN's own condensed tree over the arXiv UMAP geometry.
The term is minted here, over exactly one parent's stray members, because no arXiv category happens
to coincide with a split's noise — and the point of the check is the tree's shape, not the term's
provenance.

Usage, from the repository root, with the notebook's output already written:

    python3 probes/2026-08-18-condensed-tree/check.py <notebook-out-dir> [<work-dir>]
"""

import json
import pathlib
import subprocess
import sys
import time
import urllib.request

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

SRC = pathlib.Path(sys.argv[1])
WORK = pathlib.Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/condensed-tree-check")
LAYER = "clusters/hdbscan"
VIEWER_PORT, SESSION_PORT, CONTROL_PORT = 38801, 38802, 38803


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


# ---------------------------------------------------------------------------------------------
# 1. Find a split worth testing, from the notebook's own output
# ---------------------------------------------------------------------------------------------
log("reading the tree")
artifacts = pq.read_table(SRC / "artifacts.parquet").to_pydict()
members = pq.read_table(SRC / "members.parquet").to_pydict()

parent_of = {}
for layer, key, parent in zip(artifacts["layer"], artifacts["stable_key"], artifacts["parent_key"]):
    if layer == LAYER and parent is not None:
        parent_of[key] = parent
children_of = {}
for child, parent in parent_of.items():
    children_of.setdefault(parent, []).append(child)

# Membership, for the clusters only — a variation row is a generating set, not a membership.
by_key = {}
for layer, key, variation, member in zip(
    members["layer"], members["stable_key"], members["variation"], members["member"]
):
    if layer == LAYER and variation is None:
        by_key.setdefault(key, set()).add(int(member))

# The layer's own existence criterion, read from the declaration rather than assumed — the two
# would otherwise drift, and this probe would then be testing a configuration nobody ships.
declaration = (SRC / "layers.toml").read_text()
block = declaration[declaration.index(f'name = "{LAYER}"'):]
MIN_FRACTION = float(block[block.index("min_fraction ="):].split("=")[1].split("}")[0])

# **The parent to test**, and every clause of this filter is load-bearing:
#
# - its stray share must clear the layer's criterion, or the parent is withheld for a reason that
#   has nothing to do with the frontier and the check proves nothing. This is not a workaround: the
#   criterion is a separate control, and isolating the frontier means satisfying it.
# - its children must be big enough that "no child is served" is a real assertion rather than an
#   artefact of small numbers.
candidates = []
for parent, children in children_of.items():
    covered = set().union(*(by_key[c] for c in children))
    stray = by_key[parent] - covered
    share = len(stray) / max(1, len(by_key[parent]))
    if (share >= MIN_FRACTION * 1.5 and len(stray) >= 200
            and min(len(by_key[c]) for c in children) >= 100):
        candidates.append((len(stray), parent, children, stray, share))
if not candidates:
    sys.exit(
        f"no split in this tree keeps {MIN_FRACTION:.0%} of its parent away from every child while "
        "having children big enough to assert on; rerun the notebook larger"
    )
stray_count, parent, children, stray, stray_share = max(candidates)
log(
    f"parent {parent}: {len(by_key[parent]):,} members, {len(children)} children, "
    f"{stray_count:,} ({stray_share:.1%}) held by no child — clears the layer's "
    f"{MIN_FRACTION:.0%} floor"
)

# ---------------------------------------------------------------------------------------------
# 2. Mint a term over exactly that stray set, and rebuild
# ---------------------------------------------------------------------------------------------
WORK.mkdir(parents=True, exist_ok=True)
pairs = pq.read_table(SRC / "pairs.parquet").to_pydict()
terms = pq.read_table(SRC / "terms.parquet").to_pydict()
STRAY_TERM = max(terms["term_id"]) + 1

ent = list(pairs["entity_id"]) + sorted(stray)
tid = list(pairs["term_id"]) + [STRAY_TERM] * len(stray)
order = np.lexsort((np.array(ent), np.array(tid)))
pq.write_table(
    pa.table({
        "entity_id": pa.array(np.array(ent, dtype=np.uint32)[order], pa.uint32()),
        "term_id": pa.array(np.array(tid, dtype=np.uint32)[order], pa.uint32()),
    }),
    WORK / "pairs.parquet",
)
log(f"minted term {STRAY_TERM} over the {len(stray):,} stray members")

bundle = WORK / "bundle"
subprocess.run(["rm", "-rf", str(bundle)], check=True)
log("building")
subprocess.run(
    [
        "./target/release/tessera", "build",
        "--points", str(SRC / "points.parquet"),
        "--pairs", str(WORK / "pairs.parquet"),
        "--schema", str(SRC / "schema.toml"),
        "--layers", str(SRC / "layers.toml"),
        "--artifacts", str(SRC / "artifacts.parquet"),
        "--artifact-members", str(SRC / "members.parquet"),
        "--values", f"archive={SRC / 'archive.parquet'}",
        "--values", f"primary_category={SRC / 'primary_category.parquet'}",
        "--out", str(bundle), "--slice", "s0",
        "--extent", "0,65536,0,65536", "--mint-id-key",
    ],
    check=True, capture_output=True,
)

# **The report named the split in advance.** This is the second half of the claim: the shape that
# produces the surprising answer below is visible at build time, before any viewer meets it.
report = json.loads((bundle / "reports" / "containment.json").read_text())
named = [s for s in report["splits"]["by_stray_members"] if s["parent"] == parent]
assert report["violations"] == [], f"the tree has containment violations: {report['violations']}"
assert named, f"the report did not name {parent} among its {report['splits']['listed']} splits"
assert named[0]["stray_members"] == stray_count, (
    f"the report says {named[0]['stray_members']} stray, the tree says {stray_count}"
)
log(f"the build's report named {parent}: {named[0]['stray_members']:,} stray of {named[0]['members']:,}")

# ---------------------------------------------------------------------------------------------
# 3. Serve it and ask as that principal
# ---------------------------------------------------------------------------------------------
config = WORK / "serve.toml"
config.write_text(f"""
[bundle]
path = "{bundle}"
cache = "{WORK}/cache"
wal = "{WORK}/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:{VIEWER_PORT}"
session = "127.0.0.1:{SESSION_PORT}"
control = "127.0.0.1:{CONTROL_PORT}"
max_k = 5000
# The plane credentials are named here and supplied through the environment: `tessera serve`
# refuses to start rather than defaulting them, and refuses an inline value outright.
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
""".lstrip())

import os
env = dict(os.environ,
           TESSERA_SESSION_CRED="probe-session-credential",
           TESSERA_OPERATOR_CRED="probe-operator-credential")
server = subprocess.Popen(
    ["./target/release/tessera", "serve", "-c", str(config)],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env,
)
try:
    for _ in range(300):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{VIEWER_PORT}/readyz", timeout=1)
            break
        except Exception:
            time.sleep(0.2)
    else:
        raise SystemExit("the server never became ready")

    def post(port, path, body, bearer=None):
        headers = {"content-type": "application/json"}
        if bearer:
            headers["authorization"] = f"Bearer {bearer}"
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}{path}", data=json.dumps(body).encode(), headers=headers
        )
        return urllib.request.urlopen(req, timeout=120)

    import base64

    def artifacts_for(term_ids, budget=None):
        cred = base64.b64encode(json.dumps({"terms": [str(t) for t in term_ids]}).encode()).decode()
        # The session plane's own bearer is the operator-configured shared credential; the token it
        # mints is the per-principal one the viewer plane takes.
        token = json.loads(post(SESSION_PORT, "/session/authorise", {"auth_data": cred},
                                bearer=env["TESSERA_SESSION_CRED"]).read())["token"]
        req = {"slice": "s0", "zoom": 0, "bbox": [0, 65536, 0, 65536], "k": 1,
               "layers": [LAYER]}
        if budget is not None:
            req["artifact_budget"] = budget
        r = urllib.request.Request(
            f"http://127.0.0.1:{VIEWER_PORT}/v1/viewport",
            data=json.dumps(req).encode(),
            headers={"content-type": "application/json",
                     "authorization": f"Bearer {token}"},
        )
        return read_artifact_frame(urllib.request.urlopen(r, timeout=300).read())

    def read_artifact_frame(body):
        """Pull the artifacts frame (kind 5) out of the streamed response."""
        import io, struct
        import pyarrow.ipc as ipc
        out, at = [], 0
        while at < len(body):
            kind = body[at]
            (length,) = struct.unpack_from("<I", body, at + 1)
            payload = body[at + 5: at + 5 + length]
            at += 5 + length
            if kind == 5:
                table = ipc.open_stream(io.BytesIO(payload)).read_all().to_pydict()
                out = list(zip(table["layer"], table["stable_key"], table["masked_count"]))
        return out

    log("asking as a principal holding only the stray term")
    served = artifacts_for([STRAY_TERM])
    keys = {k for _, k, _ in served}

    # ---- the assertion the stage exists for -------------------------------------------------
    assert parent in keys, (
        f"the parent {parent} was NOT served to a principal who can see "
        f"{stray_count:,} of its members. A cut that requires a child before it will draw an "
        f"ancestor would blank this region entirely."
    )
    drawn_children = keys & set(children)
    assert not drawn_children, (
        f"children {sorted(drawn_children)} were served to a principal who can see none of their "
        f"members — the masked count is not being applied per artifact"
    )
    count = next(c for _, k, c in served if k == parent)
    assert count == stray_count, (
        f"the parent's masked count is {count:,}, not the {stray_count:,} members this principal "
        f"can actually see"
    )
    log(f"served {parent} alone, masked count {count:,}, none of its {len(children)} children")

    # The budget cannot take it away: there is nothing to climb to that this principal can see.
    for budget in (1, 2, 10):
        keys_b = {k for _, k, _ in artifacts_for([STRAY_TERM], budget=budget)}
        assert parent in keys_b, f"budget {budget} blanked the parent"
    log("and no budget takes it away")

    # The control: a principal who can see the children sees them instead of the parent.
    everything = artifacts_for(sorted(set(terms["term_id"])) + [STRAY_TERM])
    broad = {k for _, k, _ in everything}
    assert broad & set(children), "a broad principal should reach this parent's children"
    log(f"a broad principal is served {len(broad & set(children))} of its {len(children)} children")

finally:
    server.terminate()
    server.wait(timeout=30)

result = {
    "layer": LAYER,
    "parent": parent,
    "parent_members": len(by_key[parent]),
    "children": len(children),
    "stray_members": stray_count,
    "masked_count_for_stray_principal": count,
    "children_served_to_stray_principal": 0,
    "report_named_the_split": True,
    "splits_total": report["splits"]["total"],
    "splits_non_covering": report["splits"]["non_covering"],
}
(WORK / "result.json").write_text(json.dumps(result, indent=2))
print("\n" + json.dumps(result, indent=2))
print("\nPASS")
