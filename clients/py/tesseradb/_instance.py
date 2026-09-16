"""The local instance: the deployment file, its secrets, and the `tessera serve` child (§7).

`commit()` starts `tessera serve` as a child of the kernel over the directory's `tessera.toml`.
The three planes are on loopback at port 0, so the kernel does not pick ports and race another
process for them; the child says which ports it bound by printing one JSON line on stdout, and
this module reads it.

Not built yet: that announce line. `serve` binds the addresses the file names and announces
nothing (python-sdk.md §11.2 A), so [`start`] below times out against the binary on `main` and
says so, naming the line it waited for. There is no port-guessing fallback: guessing is the race
the port-0 arrangement exists to remove, and a wrong guess would hand the notebook a URL that
answers for somebody else's server.

The child is killed by its pid, at `close()` and at interpreter exit. Never by name: a kill by
process name has taken out another session's server on this machine.
"""

from __future__ import annotations

import atexit
import json
import os
import queue
import secrets
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from ._sources import Refusal
from ._toml import dumps

#: How long `start` waits for the announce line before it gives up, in seconds, unless
#: `TESSERADB_SERVE_TIMEOUT` names another.
SERVE_TIMEOUT = 30.0


def serve_timeout() -> float:
    return float(os.environ.get("TESSERADB_SERVE_TIMEOUT", SERVE_TIMEOUT))

#: The deployment's token lifetime, which `[disclosure]` requires and has no backstop default.
TOKEN_MAX_LIFETIME = 3600

IDENTITY_ENV = "TESSERA_IDENTITY_KEY"

_running: dict[int, subprocess.Popen] = {}


class ServeRefused(RuntimeError):
    """The child did not announce three bound addresses."""


@dataclass
class Listening:
    """The three planes' bound addresses, as the child announced them."""

    viewer: str
    session: str
    control: str


def write_deployment(directory: Path, cors_origins: list[str] | None = None) -> Path:
    """`tessera.toml`, as `tessera build` and `tessera serve` both read it (SA §7).

    Every path resolves against this file's own directory, so the database directory moves whole.
    """
    serve = {
        "viewer": "127.0.0.1:0",
        "session": "127.0.0.1:0",
        "control": "127.0.0.1:0",
        # Relative, so the directory travels whole. `config::load` resolves `bundle.path`,
        # `bundle.cache`, `bundle.wal` and `build.schema` against this file's own directory and
        # these two against the process's working directory, so the child is started in the
        # database directory (see `start`).
        "session_credential_file": ".tessera/session.cred",
        "operator_credential_file": ".tessera/operator.cred",
    }
    if cors_origins:
        serve["cors_origins"] = list(cors_origins)
    document = {
        "bundle": {
            "path": "bundle",
            "cache": ".tessera/cache",
            "wal": ".tessera/wal.log",
        },
        "build": {"schema": "schema.toml"},
        "identity": {"env": IDENTITY_ENV},
        "plugin": {"module": "builtin:passthrough"},
        "disclosure": {"token_max_lifetime": TOKEN_MAX_LIFETIME},
        "serve": serve,
    }
    # The engine's cache directory is opened rather than created, so a deployment file naming one
    # that does not exist refuses to start with an IO error.
    (directory / ".tessera" / "cache").mkdir(parents=True, exist_ok=True)
    path = directory / "tessera.toml"
    path.write_text(dumps(document), encoding="utf-8")
    return path


def notebook_origins() -> list[str]:
    """The viewer plane's origin list, from `TESSERA_NOTEBOOK_ORIGIN`.

    The notebook page's origin is the front end's, unknown at start and not enumerable for a
    webview. Not built yet: `serve.cors_loopback`, which would admit any page served from a
    loopback address and is a disclosure ruling (§11.2 B). Until it exists a widget from an
    unlisted origin is refused by the browser.
    """
    origin = os.environ.get("TESSERA_NOTEBOOK_ORIGIN", "").strip()
    return [o for o in origin.split(",") if o] if origin else []


def secrets_for(directory: Path) -> tuple[str, str]:
    """The session credential and the identity key, generated once per database.

    Both are written under `.tessera/` with owner-only permissions. The identity key is named by
    `[identity].env` rather than by a path, so it is passed to the child in its environment; the
    file is where this database keeps it between processes.
    """
    private = directory / ".tessera"
    private.mkdir(parents=True, exist_ok=True)
    session = _secret(private / "session.cred", lambda: secrets.token_urlsafe(32))
    _secret(private / "operator.cred", lambda: secrets.token_urlsafe(32))
    identity = _secret(private / "identity.key", lambda: secrets.token_hex(16))
    identity_file = private / "identity.toml"
    if not identity_file.exists():
        identity_file.write_text(f'[identity]\nkey = "{identity}"\n', encoding="utf-8")
        identity_file.chmod(0o600)
    return session, identity


def _secret(path: Path, mint) -> str:
    if path.exists():
        return path.read_text(encoding="utf-8").strip()
    value = mint()
    path.write_text(value + "\n", encoding="utf-8")
    path.chmod(0o600)
    return value


def start(
    binary: str,
    deployment: Path,
    identity_key: str,
    timeout: float | None = None,
) -> tuple[subprocess.Popen, Listening]:
    """Start `tessera serve` and read the addresses it bound."""
    environment = dict(os.environ)
    environment[IDENTITY_ENV] = identity_key
    child = subprocess.Popen(
        [binary, "serve", "--deployment", str(deployment)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=environment,
        cwd=str(Path(deployment).parent),
    )
    _running[child.pid] = child
    errors = _drain(child.stderr)
    try:
        listening = read_announce(
            child.stdout, serve_timeout() if timeout is None else timeout, lambda: "".join(errors)
        )
    except ServeRefused:
        stop(child)
        raise
    _drain(child.stdout)
    return child, listening


def read_announce(stdout, timeout: float, stderr_text) -> Listening:
    """Read the child's stdout until one line is the announce line (§11.2 A).

    The line is JSON carrying `event: "listening"` and the three planes' bound addresses. Lines
    that are not that are skipped: the child's own logging shares the stream.
    """
    lines: queue.Queue = queue.Queue()
    reader = threading.Thread(target=_read_lines, args=(stdout, lines), daemon=True)
    reader.start()
    deadline = time.monotonic() + timeout
    seen: list[str] = []
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            line = lines.get(timeout=min(remaining, 0.25))
        except queue.Empty:
            continue
        if line is None:
            break
        seen.append(line)
        announce = _announce(line)
        if announce is not None:
            return announce
    raise ServeRefused(
        "tessera serve announced no listening line within "
        f"{timeout:g}s. The SDK waits for one line of JSON on stdout carrying "
        '\'"event": "listening"\' and the viewer, session and control addresses '
        "(python-sdk.md §11.2 A, not built on main). What the child wrote:\n"
        + "".join(f"  stdout: {line}" for line in seen[-20:])
        + _tail(stderr_text())
    )


def _announce(line: str) -> Listening | None:
    line = line.strip()
    if not line.startswith("{"):
        return None
    try:
        document = json.loads(line)
    except ValueError:
        return None
    if not isinstance(document, dict) or document.get("event") != "listening":
        return None
    missing = [k for k in ("viewer", "session", "control") if not document.get(k)]
    if missing:
        raise ServeRefused(
            f"tessera serve's listening line names no {', '.join(missing)} address: {line}"
        )
    return Listening(
        viewer=str(document["viewer"]),
        session=str(document["session"]),
        control=str(document["control"]),
    )


def _read_lines(stream, lines: queue.Queue) -> None:
    for line in stream:
        lines.put(line)
    lines.put(None)


def _drain(stream) -> list[str]:
    """Keep reading a pipe after it has said what was wanted, so the child never blocks on it."""
    kept: list[str] = []

    def run() -> None:
        for line in stream:
            kept.append(line)
            del kept[:-200]

    threading.Thread(target=run, daemon=True).start()
    return kept


def _tail(text: str) -> str:
    if not text:
        return ""
    return "\n" + "".join(f"  stderr: {line}\n" for line in text.strip().splitlines()[-20:])


def stop(child: subprocess.Popen, timeout: float = 5.0) -> None:
    """Stop the child by its pid: `SIGTERM`, then `SIGKILL` if it is still there."""
    _running.pop(child.pid, None)
    if child.poll() is not None:
        return
    try:
        os.kill(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        child.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.kill(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=timeout)


@atexit.register
def _stop_everything() -> None:
    for child in list(_running.values()):
        stop(child)


def find_binary() -> str:
    """The `tessera` binary: `TESSERA_BIN`, `PATH`, or a checkout's target directory (§7).

    At release a platform wheel carries it (§11.2 E).
    """
    named = os.environ.get("TESSERA_BIN")
    if named:
        if not Path(named).exists():
            raise Refusal(f"TESSERA_BIN names {named}, which does not exist")
        return named
    from shutil import which

    found = which("tessera")
    if found:
        return found
    here = Path(__file__).resolve()
    for parent in here.parents:
        for profile in ("release", "debug"):
            candidate = parent / "target" / profile / "tessera"
            if candidate.exists():
                return str(candidate)
    raise Refusal(
        "no `tessera` binary on PATH, at TESSERA_BIN, or in a checkout's target directory. "
        "Build it with `cargo build --release -p tessera-cli`"
    )
