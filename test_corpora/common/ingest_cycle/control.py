from __future__ import annotations

import time
import urllib.parse

import requests

# ---------------------------------------------------------------------------------------------
# The control plane
# ---------------------------------------------------------------------------------------------


class Control:
    def __init__(self, base: str, cred: str, view: str | None = None):
        self.base = base
        self.headers = {"Authorization": f"Bearer {cred}"}
        # **`x-tessera-view` where the bundle has more than one.** A batch carries one row space,
        # and which one it belongs to is not inferable from its columns, so a multi-view deployment
        # refuses an unlabelled batch outright (contracts §3.4). One view is the header's absence,
        # which is what every rung below rung 5 sends.
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

    def register_layer(self, declaration: dict):
        return requests.put(
            f"{self.base}/control/layers", headers=self.headers, json=declaration, timeout=120
        )

    def grow(self, layer: str, body: bytes, session: requests.Session, timeout=1800):
        """`PATCH /control/layers/{name}/artifacts` (decision 0127): more members for artifacts the
        level already holds, the body already serialised, as [`Control.publish`] sends its own."""
        t0 = time.perf_counter()
        r = session.patch(
            f"{self.base}/control/layers/{urllib.parse.quote(layer, safe='')}/artifacts",
            headers=self.headers | {"Content-Type": "application/json"},
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0

    def publish(self, layer: str, body: bytes, session: requests.Session, timeout=1800):
        """`PUT /control/layers/{name}/artifacts`, with the body already serialised.

        Sent as bytes under an explicit content type rather than through `json=`: the body is
        assembled once as bytes by [`Publication`], and handing `requests` a dict would serialise
        a 10⁶-member artifact a second time.

        **The layer name is percent-encoded**, because the route matches one path segment and this
        rung's names are path-shaped: `clusters/kmeans` unencoded is a 404 at the router rather
        than a refusal from the handler, which reads as an empty publication rather than as an
        error.
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
