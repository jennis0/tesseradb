"""Boot a `tessera serve` for a measurement, on ports and scratch state of its own, for
`serve_battery.py` and `ingest_cycle.py`.

Never the rung's own `tessera.toml`, since another session usually serves the same bundle from
it: a measurement writes a copy with its own ports, cache and WAL under a scratch directory.

Always inside a `systemd-run --user --scope` transient scope, capped or not: it needs no root,
and gives a cgroup directory that `memory.events`, `memory.stat` and `memory.reclaim` read from.
"""

from __future__ import annotations

import json
import os
import secrets
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

#: The `[serve]` keys of a rung that name this machine. Every other key is copied into the
#: measurement's own deployment file.
MACHINE_SPECIFIC_SERVE = frozenset(
    {
        "viewer",
        "session",
        "control",
        "dev_cors_origins",
        "cors_origins",
        "cors_loopback",
        "session_credential_file",
        "operator_credential_file",
    }
)


#: The variable the server reads the identity key from where `[identity]` names none:
#: `tessera_config::DEFAULT_IDENTITY_ENV`.
DEFAULT_IDENTITY_ENV = "TESSERA_IDENTITY_KEY"


def toml_lines(table: dict) -> str:
    """A flat TOML table's `key = value` lines. JSON and TOML spell every scalar and array here
    the same way."""
    return "".join(f"{key} = {json.dumps(value)}\n" for key, value in table.items())


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


def minted_credentials(source_dir: Path) -> dict[str, str]:
    """A value for every credential and identity-key variable this deployment names that the
    environment and the deployment's own `.env` do not carry, minted for this run only.
    """
    deployment = tomllib.loads((source_dir / "tessera.toml").read_text())
    serve = deployment["serve"]
    env = dict(os.environ) | read_env_file(source_dir / ".env")
    minted = {
        serve[f"{which}_credential_env"]: secrets.token_urlsafe(32)
        for which in ("session", "operator")
        if not env.get(serve[f"{which}_credential_env"])
    }
    # An `[identity].env` that is absent, not a string or blank names the default variable.
    named = deployment.get("identity", {}).get("env")
    identity = named if isinstance(named, str) and named.strip() else DEFAULT_IDENTITY_ENV
    if not env.get(identity):
        minted[identity] = secrets.token_hex(16)
    return minted


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
        serve: dict | None = None,
    ):
        self.source_dir = Path(source_dir)
        self.bundle = Path(bundle)
        self.scratch = Path(scratch)
        self.ports = ports
        self.binary = Path(binary)
        self.cap_bytes = cap_bytes
        #: `[ingest]` keys written into the copy. Empty means the server's own defaults.
        self.ingest = dict(ingest or {})
        #: Extra `[serve]` keys written into the copy, beside the ports and the credentials.
        self.serve = dict(serve or {})
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
        """The `session` or `operator` credential's value, from the environment, never from the
        copy this class writes, which carries only the variable's name.
        """
        source = tomllib.loads((self.source_dir / "tessera.toml").read_text())["serve"]
        return self.env[source[f"{which}_credential_env"]]

    def _write_toml(self) -> None:
        source = tomllib.loads((self.source_dir / "tessera.toml").read_text())
        serve = {
            key: value
            for key, value in source["serve"].items()
            if key not in MACHINE_SPECIFIC_SERVE
        }
        serve["viewer"] = f"127.0.0.1:{self.ports[0]}"
        serve["session"] = f"127.0.0.1:{self.ports[1]}"
        serve["control"] = f"127.0.0.1:{self.ports[2]}"
        serve.update(self.serve)
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
{toml_lines(source.get("disclosure") or {"token_max_lifetime": 3600})}
[serve]
{toml_lines(serve)}"""
        if self.ingest:
            body += "\n[ingest]\n" + toml_lines(self.ingest)
        self.toml.write_text(body)

    def start(self, log: Path | None = None, timeout: float = 900.0) -> None:
        log = log or (self.scratch / "serve.log")
        # Always a transient scope, capped or not: its cgroup gives `memory.reclaim`, the only
        # eviction here that can drop pages the server holds mapped.
        scope = [
            "systemd-run",
            "--user",
            "--scope",
            "--collect",
            "--quiet",
            # Swap off, so `memory.reclaim` can only drop file pages rather than swap anonymous
            # memory out.
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
        """The served `tessera` pid, which is never `self.proc.pid` — that is systemd-run's."""
        out = subprocess.run(
            ["pgrep", "-f", f"serve --deployment {self.toml}"],
            capture_output=True,
            text=True,
        ).stdout.split()
        # `pgrep` also matches the systemd-run wrapper; the served process is the one whose
        # `/proc/<pid>/comm` matches the binary's name, truncated to `TASK_COMM_LEN`.
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
        """Stop this process by pid. Never `pkill -f tessera`: other sessions serve too."""
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
