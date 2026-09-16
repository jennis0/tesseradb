"""The local instance: the deployment file, its secrets, and the `tessera serve` child (§7).

`commit()` starts `tessera serve` as a child of the kernel over the directory's `tessera.toml`.
The three planes are on loopback at port 0, so the kernel picks no ports and the child says which
ones it bound by printing one JSON line on stdout. The line is written once all three planes are
listening, so the announced viewer address is already answering when it arrives.

There is no fallback that guesses a port. A guess is a second process's port as readily as this
one's, and the notebook would then hold a URL answering for somebody else's server. A child that
announces nothing is a refusal carrying what the child wrote.

The child is killed by its pid, at `close()` and at interpreter exit, never by process name: a
kill by name reaches every other server on the machine.
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

from ._refusal import Refusal
from ._toml import dumps

#: How long `start` waits for the announce line before it gives up, in seconds, unless
#: `TESSERADB_SERVE_TIMEOUT` names another.
SERVE_TIMEOUT = 30.0

#: How many of the child's lines are kept for a refusal message.
KEPT_LINES = 200


def serve_timeout() -> float:
    return float(os.environ.get("TESSERADB_SERVE_TIMEOUT", SERVE_TIMEOUT))


#: The deployment's token lifetime, which `[disclosure]` requires and has no backstop default.
TOKEN_MAX_LIFETIME = 3600

IDENTITY_ENV = "TESSERA_IDENTITY_KEY"

_running: dict[int, subprocess.Popen] = {}


class ServeRefused(Refusal):
    """The child did not announce three bound addresses."""


@dataclass
class Listening:
    """The three planes' bound addresses, as the child announced them."""

    viewer: str
    session: str
    control: str


def write_deployment(directory: Path, cors_origins: list[str] | None = None) -> Path:
    """`tessera.toml`, as `tessera build` and `tessera serve` both read it (SA §7).

    Every path in it resolves against this file's own directory, so the database directory serves
    from wherever it is copied to.
    """
    serve = {
        "viewer": "127.0.0.1:0",
        "session": "127.0.0.1:0",
        "control": "127.0.0.1:0",
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
    """The session credential, the operator credential and the identity key, once per database.

    All three are written under `.tessera/`, which is owner-only, and each file is created
    owner-only rather than created and then narrowed: between a write and a `chmod` the secret is
    readable by anyone on the machine. The identity key is named by `[identity].env` rather than
    by a path, so it is passed to the child in its environment; the file is where this database
    keeps it between processes. The operator credential is generated beside the session one
    because the deployment file requires both.
    """
    private = directory / ".tessera"
    private.mkdir(parents=True, exist_ok=True)
    private.chmod(0o700)
    session = _secret(private / "session.cred", lambda: secrets.token_urlsafe(32))
    _secret(private / "operator.cred", lambda: secrets.token_urlsafe(32))
    identity = _secret(private / "identity.key", lambda: secrets.token_hex(16))
    _secret(private / "identity.toml", lambda: f'[identity]\nkey = "{identity}"\n')
    return session, identity


def _secret(path: Path, mint) -> str:
    if path.exists():
        return path.read_text(encoding="utf-8").strip()
    value = mint()
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as file:
        file.write(value if value.endswith("\n") else value + "\n")
    return value.strip()


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
        return child, read_announce(
            child.stdout, serve_timeout() if timeout is None else timeout, lambda: "".join(errors)
        )
    except ServeRefused:
        stop(child)
        raise


def read_announce(stdout, timeout: float, stderr_text) -> Listening:
    """Read the child's stdout until one line is the announce line (§11.2 A).

    The line is JSON carrying `event: "listening"` and the three planes' bound addresses. Lines
    that are not that are skipped, the child's own logging having shared the stream before now.

    One reader holds the stream for the life of the child: it stops queueing lines once the
    announce line is found and keeps reading, so a child that logs after it has started never
    fills the pipe and blocks.
    """
    reader = _Reader(stdout)
    reader.start()
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            line = reader.lines.get(timeout=min(remaining, 0.25))
        except queue.Empty:
            continue
        if line is None:
            break
        announce = _announce(line)
        if announce is not None:
            reader.queueing = False
            return announce
    raise ServeRefused(
        "tessera serve announced no listening line within "
        f"{timeout:g}s. The SDK waits for one line of JSON on stdout carrying "
        '\'"event": "listening"\' and the viewer, session and control addresses. '
        "What the child wrote:\n"
        + "".join(f"  stdout: {line}" for line in reader.kept[-20:])
        + _tail(stderr_text())
    )


class _Reader(threading.Thread):
    """One thread per stream: it queues lines while `queueing`, and keeps the last of them."""

    def __init__(self, stream) -> None:
        super().__init__(daemon=True)
        self.stream = stream
        self.lines: queue.Queue = queue.Queue()
        self.kept: list[str] = []
        self.queueing = True

    def run(self) -> None:
        for line in self.stream:
            self.kept.append(line)
            del self.kept[:-KEPT_LINES]
            if self.queueing:
                self.lines.put(line)
        self.lines.put(None)


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


def _drain(stream) -> list[str]:
    """Keep reading a pipe the SDK does not parse, so the child never blocks writing to it."""
    reader = _Reader(stream)
    reader.queueing = False
    reader.start()
    return reader.kept


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


def find_binary() -> tuple[str, str]:
    """The `tessera` binary and where it came from (§7).

    `TESSERA_BIN` when set, else the first `tessera` on `PATH`, else a checkout's target
    directory, release before debug. At release a platform wheel carries it (§11.2 E).
    """
    named = os.environ.get("TESSERA_BIN")
    if named:
        if not Path(named).exists():
            raise Refusal(f"TESSERA_BIN names {named}, which does not exist")
        return named, "TESSERA_BIN"
    from shutil import which

    found = which("tessera")
    if found:
        return found, "PATH"
    for parent in Path(__file__).resolve().parents:
        for profile in ("release", "debug"):
            candidate = parent / "target" / profile / "tessera"
            if candidate.exists():
                return str(candidate), f"this checkout's target/{profile}"
    raise Refusal(
        "no `tessera` binary at TESSERA_BIN, on PATH, or in a checkout's target directory. "
        "Build it with `cargo build --release -p tessera-cli`"
    )
