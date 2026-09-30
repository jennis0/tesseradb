"""A step timer, so a rung's manifest records cost per step rather than only a total."""

from __future__ import annotations

import time


class Steps(dict):
    """`name -> seconds`, filled by the context manager and written into the manifest."""

    def __init__(self):
        super().__init__()
        self.started = time.time()

    def step(self, name: str):
        return _Step(self, name)

    def total(self) -> float:
        return round(time.time() - self.started, 1)


class _Step:
    def __init__(self, steps: Steps, name: str):
        self.steps, self.name = steps, name

    def __enter__(self):
        self.at = time.time()
        print(f"[{self.name}] ...", flush=True)
        return self

    def __exit__(self, *exc):
        self.steps[self.name] = round(time.time() - self.at, 2)
        print(f"[{self.name}] {self.steps[self.name]}s", flush=True)
