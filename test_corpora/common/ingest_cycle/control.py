from __future__ import annotations

import time
import urllib.parse

import requests


def quote(segment: str) -> str:
    return urllib.parse.quote(segment, safe="")

# ---------------------------------------------------------------------------------------------
# The control plane
# ---------------------------------------------------------------------------------------------


class Control:
    def __init__(self, base: str, cred: str, view: str | None = None):
        self.base = base
        self.headers = {"Authorization": f"Bearer {cred}"}
        # `x-tessera-view` where the bundle has more than one; one view is the header's absence.
        self.view = view

    def status(self) -> dict:
        r = requests.get(f"{self.base}/control/status", headers=self.headers, timeout=60)
        r.raise_for_status()
        return r.json()

    def ingest(self, body: bytes, batch_id: str, session: requests.Session, timeout=600):
        t0 = time.perf_counter()
        r = session.post(
            f"{self.base}/control/ingest",
            headers=self.headers
            | {"x-tessera-batch-id": batch_id, "Content-Type": "application/vnd.apache.arrow.stream"}
            | ({"x-tessera-view": self.view} if self.view else {}),
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0

    def changes(self, items: list[dict], timeout=600):
        t0 = time.perf_counter()
        r = requests.post(
            f"{self.base}/control/changes", headers=self.headers, json=items, timeout=timeout
        )
        return r, time.perf_counter() - t0

    def flush(self):
        return requests.post(f"{self.base}/control/flush", headers=self.headers, timeout=60)

    def compact(self):
        return requests.post(f"{self.base}/control/compact", headers=self.headers, timeout=60)

    def drop_view(self, group: str, key: str):
        """`DELETE /control/views/{group}/{key}`: the key's view in every group sharing it."""
        return requests.delete(
            f"{self.base}/control/views/{quote(group)}/{quote(key)}",
            headers=self.headers,
            timeout=120,
        )

    def create_view(self, group: str, key: str, record: dict):
        """`PUT /control/views/{group}/{key}`: the roster record, which creates the view empty."""
        return requests.put(
            f"{self.base}/control/views/{quote(group)}/{quote(key)}",
            headers=self.headers,
            json=record,
            timeout=120,
        )

    def register_layer(self, declaration: dict):
        return requests.put(
            f"{self.base}/control/layers", headers=self.headers, json=declaration, timeout=120
        )

    def grow(self, layer: str, body: bytes, session: requests.Session, timeout=1800):
        """`PATCH /control/layers/{name}/artifacts`: more members for artifacts already held."""
        t0 = time.perf_counter()
        r = session.patch(
            f"{self.base}/control/layers/{urllib.parse.quote(layer, safe='')}/artifacts",
            headers=self.headers | {"Content-Type": "application/json"},
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0

    def publish(self, layer: str, body: bytes, session: requests.Session, timeout=1800):
        """`PUT /control/layers/{name}/artifacts`, with the body already serialised as bytes by
        [`Publication`] rather than through `json=`. The layer name is percent-encoded, since
        this rung's names are path-shaped and would otherwise 404 at the router.
        """
        t0 = time.perf_counter()
        r = session.put(
            f"{self.base}/control/layers/{urllib.parse.quote(layer, safe='')}/artifacts",
            headers=self.headers | {"Content-Type": "application/json"},
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0


def wait_for(predicate, timeout: float, interval: float = 0.5) -> tuple[bool, float]:
    """Poll `predicate` until true. Returns `(reached, seconds)`."""
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < timeout:
        if predicate():
            return True, time.perf_counter() - t0
        time.sleep(interval)
    return False, time.perf_counter() - t0
