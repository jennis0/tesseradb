"""Boot a `tessera serve` for a measurement, on ports and scratch state of our own.

Two measurement drivers need a running server and neither should be the thing that decides how one
is started: `serve_battery.py` measures view latency against one, `ingest_cycle.py` ingests into
one. So the boot lives here, once.

**Never the rung's own `tessera.toml`.** That file names ports 8111–8113 and a cache and WAL inside
the rung directory, and another session is usually already serving the same bundle from it. A
measurement writes a *copy* — same bundle, its own ports, its own cache and WAL under a scratch
directory — which is the same discipline `probes/2026-09-02-serve-under-memory-cap` used and for
the same reason.

**Always a transient scope.** `systemd-run --user --scope -p MemoryMax=…` needs no
root on this box (the `memory` controller is delegated to the user slice), and it gives the cgroup
directory the battery reads `memory.events`, `memory.stat` and `memory.reclaim` from. The scope's
cgroup path is discovered from the served process's own `/proc/<pid>/cgroup` rather than
constructed, so a systemd that names it differently cannot silently produce a run with no proof of
cold.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib

import requests


def read_env_file(path: Path) -> dict[str, str]:
    """A deployment's `.env`, as a plain mapping. Absent file → empty."""
    out: dict[str, str] = {}
    if not path.is_file():
        return out
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        out[key.strip()] = value.strip()
    return out


class Deployment:
    """A scratch deployment file over an existing bundle, and the server it serves it with."""

    def __init__(
        self,
        source_dir: Path,
        bundle: Path,
        scratch: Path,
        ports: tuple[int, int, int],
        binary: Path,
        cap_bytes: int | None = None,
        env: dict[str, str] | None = None,
        ingest: dict | None = None,
    ):
        self.source_dir = Path(source_dir)
        self.bundle = Path(bundle)
        self.scratch = Path(scratch)
        self.ports = ports
        self.binary = Path(binary)
        self.cap_bytes = cap_bytes
        #: `[ingest]` keys written into the copy. Empty means the server's own defaults, which is
        #: what every cell that is not sweeping a write-path knob wants.
        self.ingest = dict(ingest or {})
        self.env = dict(os.environ)
        self.env.update(read_env_file(self.source_dir / ".env"))
        if env:
            self.env.update(env)
        self.proc: subprocess.Popen | None = None
        self.pid: int | None = None
        self.cgroup: Path | None = None
        self.scratch.mkdir(parents=True, exist_ok=True)
        self.toml = self.scratch / "tessera.toml"
        self._write_toml()

    @property
    def viewer(self) -> str:
        return f"http://127.0.0.1:{self.ports[0]}"

    @property
    def session(self) -> str:
        return f"http://127.0.0.1:{self.ports[1]}"

    @property
    def control(self) -> str:
        return f"http://127.0.0.1:{self.ports[2]}"

    @property
    def cache(self) -> Path:
        return self.scratch / "cache"

    def credential(self, which: str) -> str:
        """The `session` or `operator` credential's *value*, from the environment.

        A deployment file carries only the variable's **name** (`configuration.md`), so the value
        comes from the rung's `.env` or the process environment and never from the copy this class
        writes.
        """
        source = tomllib.loads((self.source_dir / "tessera.toml").read_text())["serve"]
        return self.env[source[f"{which}_credential_env"]]

    def _write_toml(self) -> None:
        source = tomllib.loads((self.source_dir / "tessera.toml").read_text())
        serve = source["serve"]
        body = f"""# Written by test_corpora/common/deployment.py for a measurement run. Not committed with a
# rung: the ports and the scratch paths are this run's, the bundle is the rung's, and the
# credential *values* are in the environment as `configuration.md` requires.

[bundle]
path  = "{self.bundle.resolve()}"
cache = "{(self.scratch / 'cache').resolve()}"
wal   = "{(self.scratch / 'wal.log').resolve()}"

[build]
schema = "{(self.source_dir / 'corpus.toml').resolve()}"

[plugin]
module = "{source.get('plugin', {}).get('module', 'builtin:passthrough')}"

[identity]
env = "{source.get('identity', {}).get('env', 'TESSERA_IDENTITY_KEY')}"

[disclosure]
token_max_lifetime = {source.get('disclosure', {}).get('token_max_lifetime', 3600)}

[serve]
viewer  = "127.0.0.1:{self.ports[0]}"
session = "127.0.0.1:{self.ports[1]}"
control = "127.0.0.1:{self.ports[2]}"
max_k   = {serve.get('max_k', 5000)}
session_credential_env  = "{serve['session_credential_env']}"
operator_credential_env = "{serve['operator_credential_env']}"
"""
        if self.ingest:
            body += "\n[ingest]\n" + "".join(
                f"{key} = {value!r}\n".replace("'", '"') for key, value in self.ingest.items()
            )
        self.toml.write_text(body)

    def start(self, log: Path | None = None, timeout: float = 900.0) -> None:
        log = log or (self.scratch / "serve.log")
        # **Always a transient scope, capped or not.** The cap is one property of it; what the
        # scope buys unconditionally is a cgroup directory, and `memory.reclaim` on that cgroup is
        # the only eviction available here that can drop pages the server holds **mapped**.
        # `posix_fadvise(DONTNEED)` cannot: `invalidate_mapping_pages` skips a page that is in
        # some process's page tables, which on a mapped-column design is nearly all of them. Run
        # outside a scope, an uncapped battery's cold samples are mostly page-warm and are
        # recorded as `eviction_failed`, which is a measurement of the harness rather than of the
        # server.
        scope = [
            "systemd-run",
            "--user",
            "--scope",
            "--collect",
            "--quiet",
            # **Swap off in the scope, capped or not.** `memory.reclaim` reclaims anonymous
            # memory as readily as file pages, so with swap available the battery's eviction
            # would push the engine's own heap to disk and every "cold page" figure would be part
            # swap-in. With it off, reclaim can only drop file pages, which is what the condition
            # names.
            "-p",
            "MemorySwapMax=0",
        ]
        if self.cap_bytes is not None:
            scope += ["-p", f"MemoryMax={self.cap_bytes}"]
        command = scope + ["--", str(self.binary), "serve", "--deployment", str(self.toml)]
        self.log_path = log
        with open(log, "wb") as handle:
            self.proc = subprocess.Popen(
                command,
                stdout=handle,
                stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,
                env=self.env,
                start_new_session=True,
            )
        self._wait_ready(timeout)
        self.pid = self._served_pid()
        self.cgroup = self._cgroup_of(self.pid)

    def _wait_ready(self, timeout: float) -> None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"tessera serve exited {self.proc.returncode} before answering /readyz; "
                    f"see {self.log_path}"
                )
            try:
                if requests.get(f"{self.viewer}/readyz", timeout=3).status_code == 200:
                    return
            except requests.exceptions.RequestException:
                pass
            time.sleep(0.5)
        raise TimeoutError(f"tessera serve never answered /readyz; see {self.log_path}")

    def _served_pid(self) -> int:
        """The **tessera** pid, which is never `self.proc.pid` — that is systemd-run's."""
        out = subprocess.run(
            ["pgrep", "-f", f"serve --deployment {self.toml}"],
            capture_output=True,
            text=True,
        ).stdout.split()
        # Our own `pgrep` invocation and the systemd-run wrapper both match the pattern; the
        # served process is the one whose `/proc/<pid>/comm` is the binary's name.
        #
        # **Compared truncated**, because `comm` is `TASK_COMM_LEN` — 15 characters plus a NUL —
        # and a longer binary name never matches its own. A measurement that copies the binary
        # aside under a descriptive name (`tessera-bt-rowtrigger`) then fails here with "could not
        # identify the served process" while the server is up and answering.
        want = self.binary.name[:15]
        for pid in out:
            try:
                comm = Path(f"/proc/{pid}/comm").read_text().strip()
            except OSError:
                continue
            if comm == want:
                return int(pid)
        raise RuntimeError("could not identify the served process under the transient scope")

    @staticmethod
    def _cgroup_of(pid: int) -> Path | None:
        try:
            line = Path(f"/proc/{pid}/cgroup").read_text().strip()
        except OSError:
            return None
        # cgroup v2 has one line, `0::<path>`.
        relative = line.split("::", 1)[-1].strip()
        path = Path("/sys/fs/cgroup") / relative.lstrip("/")
        return path if path.is_dir() else None

    def stop(self) -> None:
        """Stop **our** process by pid. Never `pkill -f tessera`: other sessions serve too."""
        for pid in filter(None, [self.pid, self.proc.pid if self.proc else None]):
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                continue
        if self.proc is not None:
            try:
                self.proc.wait(timeout=120)
            except subprocess.TimeoutExpired:
                for pid in filter(None, [self.pid, self.proc.pid]):
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        self.proc = None

    def clear_scratch(self) -> None:
        """Drop the cache and WAL, so the next boot is a fresh deployment over the same bundle."""
        for name in ("cache", "wal.log"):
            path = self.scratch / name
            if path.is_dir():
                shutil.rmtree(path)
            elif path.exists():
                path.unlink()
        for path in self.scratch.glob("wal-*"):
            path.unlink()

    def __enter__(self):
        self.start()
        return self

    def __exit__(self, *exc):
        self.stop()
        return False
