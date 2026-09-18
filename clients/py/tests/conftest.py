"""What every test that needs a served database uses: the binary, the corpus and the fixture.

A test of the paged commit runs against a real `tessera serve` over a real bundle. There is no
double: what is being tested is that the SDK's pages are what the control plane takes, so a fake
control plane would test the SDK against the SDK's own reading of the contract.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

from tesseradb import _instance


def binary() -> str:
    try:
        return _instance.find_binary()[0]
    except Exception as why:  # noqa: BLE001, the skip message is the whole point
        pytest.skip(f"no tessera binary: {why}")


def notebook_corpus() -> Path:
    """`data/notebook/`, which is gitignored and shared by every worktree of this checkout."""
    named = os.environ.get("TESSERA_NOTEBOOK_DATA")
    if named:
        return Path(named)
    here = Path(__file__).resolve()
    common = subprocess.run(
        ["git", "rev-parse", "--git-common-dir"],
        cwd=here.parent,
        capture_output=True,
        text=True,
    )
    roots = []
    if common.returncode == 0:
        roots.append(Path(common.stdout.strip()).resolve().parent)
    roots.append(here.parents[3])
    for root in roots:
        if (root / "data" / "notebook" / "schema.toml").exists():
            return root / "data" / "notebook"
    pytest.skip("data/notebook/ is not in this checkout; set TESSERA_NOTEBOOK_DATA")


@pytest.fixture
def corpus() -> Path:
    binary()
    return notebook_corpus()


@pytest.fixture
def served(tmp_path, corpus):
    """A committed, served database over the notebook corpus, closed when the test ends.

    The fixture is a factory rather than a database: a test that wants a second one, or one over a
    frame instead of the corpus, takes the same teardown.
    """
    started = []

    def build(declare) -> "object":
        from tesseradb._database import create

        db = create(tmp_path / f"db{len(started)}")
        started.append(db)
        declare(db)
        report = db.commit()
        assert report.ok, report.output
        return db

    yield build
    for db in started:
        db.close()


# ---------------------------------------------------------------------------- the viewer plane


def post(url: str, token: str, body: dict, stream: bool = False) -> bytes:
    import json
    import urllib.error
    import urllib.request

    request = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        method="POST",
        headers={"authorization": f"Bearer {token}", "content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.read()
    except urllib.error.HTTPError as refused:
        raise AssertionError(
            f"{url} refused {refused.code}: {refused.read().decode(errors='replace')}"
        ) from None


def viewport(db, view: str, bbox, zoom: int = 0, k: int = 512, filters: dict | None = None,
             terms=None) -> dict:
    """One `/v1/viewport`, decoded to the tiles frame's masked counts and the trailer's points.

    The response is a sequence of tagged, length-prefixed frames (contracts §3.2): kind 1 is the
    tiles stream, which carries the exact masked counts, and kind 4 is the JSON trailer, whose
    presence marks the response complete.
    """
    import io
    import json as _json

    import pyarrow.ipc as ipc

    body: dict = {"view": view, "zoom": zoom, "bbox": list(bbox), "k": k, "layers": "all"}
    if filters is not None:
        body["filters"] = filters
    content = post(db.viewer_url + "/v1/viewport", db.token(terms).token, body)
    counts: dict = {}
    trailer: dict = {}
    ids: list[str] = []
    at = 0
    while at + 5 <= len(content):
        kind = content[at]
        length = int.from_bytes(content[at + 1 : at + 5], "little")
        payload = content[at + 5 : at + 5 + length]
        at += 5 + length
        if kind == 1:
            table = ipc.open_stream(io.BytesIO(payload)).read_all()
            for name in ("visible", "matched", "served"):
                if name in table.column_names:
                    counts[name] = sum(int(v) for v in table.column(name).to_pylist())
        elif kind == 3:
            table = ipc.open_stream(io.BytesIO(payload)).read_all()
            if "tessera_id" in table.column_names:
                ids += [str(v) for v in table.column("tessera_id").to_pylist()]
        elif kind == 4:
            trailer = _json.loads(payload.decode())
    return {"counts": counts, "trailer": trailer, "ids": ids, "complete": bool(trailer)}


def item(db, tessera_id: str, terms=None) -> dict:
    import json as _json

    return _json.loads(
        post(f"{db.viewer_url}/v1/items/{tessera_id}", db.token(terms).token, {})
    )


def browse(db, view: str, layer: str, terms=None, **extra) -> dict:
    import json as _json

    body = {"view": view, "layer": layer, **extra}
    return _json.loads(post(db.viewer_url + "/v1/artifacts/browse", db.token(terms).token, body))


def categories(db, column: str, terms=None, **query) -> dict:
    """`GET /v1/categories/{column}`: what this column's codes stand for (contracts §3.2).

    The bare form pages the value set, which is how a vocabulary declared at a running service is
    read back: `/v1/meta` names the vocabulary a category reads and carries none of its values.
    """
    import json as _json
    import urllib.parse
    import urllib.request

    url = f"{db.viewer_url}/v1/categories/{urllib.parse.quote(column, safe='')}"
    if query:
        url += "?" + urllib.parse.urlencode(query)
    request = urllib.request.Request(
        url, headers={"authorization": f"Bearer {db.token(terms).token}"}
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        return _json.loads(response.read())
