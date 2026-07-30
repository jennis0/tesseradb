#!/usr/bin/env python3
"""Ask 4: true wire calls with 5/10/100/1000 simultaneous users, two arms.

Boots a real `tessera serve`, authorises the sessions, and drives `tessera-bench load` against
it while sampling the server's RSS and CPU. The hot loop is Rust because a Python client is the
bottleneck at 1000 concurrent connections pulling multi-megabyte Arrow bodies; everything else is
here, reusing `reference/oracle/harness.py`'s proven config/boot/teardown machinery and
`bench_k_sweep.py`'s long-boot spawner rather than reimplementing them.

Two arms, differing only in the token file handed to the generator:

  Arm A -- N DISTINCT grant sets, so N distinct fragments and N distinct row projections. This is
  the memory question. Design §13.1 names the materialised mask as the first thing to break: at
  10^9 a dense mask is 125 MB and "a thousand live auth inputs is 125 GB". At the scales this
  suite runs (<=25M) a dense mask is ~3 MB, so 1000 sessions is ~3 GB and the wall is NOT reached
  -- what this arm yields is the per-session marginal cost and the RSS-vs-N slope, which
  extrapolate. Reported as a slope; do not read it as having found the ceiling.

  Arm B -- N sessions over a handful of grant sets, so fragments and row projections are shared.
  Isolates request-path throughput and lock contention from per-session memory. The suspected
  contention points, all on the hot path:
    * `Engine::row_projection_cache: Mutex<FxHashMap>`, locked on every viewport
    * `state.engine.meta()` per request -- a second `load_full` plus two `Vec` clones
    * `build_scalar_columns` -- a transpose with per-point `String` clones

The generator's own ceiling is measured first, against `/healthz`, and any viewport cell within
3x of it is flagged `generator_bound`: proving the client is not the bottleneck rather than
assuming it.

## Task 9 additions

The Arm A/B matrix above is `--criteria matrix` and covers exit criteria 1 (F4 gone) and, via the
`--admission-timeout-ms`-derived hang watchdog now wired into every load call, criterion 2 (no
hangs at c >> cores). Four more functions, each booting its OWN server (a different `[serve]`
config is boot-time, not runtime), cover the rest:

  * `run_cpu_saturation_cell` -- criterion 4: an open-loop cell driven near the throughput the
    matrix already found Arm B's ceiling to be, plus a small two-boot (`compute_threads` 1 vs the
    machine default) c=1 latency comparison on a multi-tile viewport.
  * `run_shed_cell` -- criterion 3: `compute_admission`/`compute_queue` forced low so the gate
    sheds deterministically; checks `shed_rate > 0`, `Retry-After` compliance, and that served
    (200) p99 stays bounded.
  * `run_panstorm_cell` -- criterion 5: warm sessions, `--pan-storm` client aborts every
    `--pan-storm-abort-ms`, throughput compared against a no-abort baseline at the same
    concurrency, plus a `/control/status` permit-leak check before/after.
  * `run_coldbuild_cell` -- criterion 6: one cold token authorised with (approximately) every
    descriptor in the bundle, so its row-projection build is the most expensive buildable at this
    scale without an artificial sleep in production code, hammered by many concurrent workers on
    the SAME key while a separate warm pool's traffic on OTHER keys is driven at the same time.

Criterion 7 (byte-identity) is not run from this script at all -- it is `cargo test -p
tessera-server --test http` plus the `reference/` oracle suite, both already exercised by every
earlier task in this workstream and re-run directly by the report, not through the HTTP load path.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "reference"))

import requests  # noqa: E402

from oracle import harness  # noqa: E402
from bench_k_sweep import read_dictionary_descriptors, spawn_with_long_boot_deadline  # noqa: E402

BENCH_BIN = REPO_ROOT / "target" / "release" / "tessera-bench"

# Mirrors `crates/tessera-server/src/config.rs`'s `DEFAULT_ADMISSION_TIMEOUT_MS`. Duplicated here
# (rather than parsed out of the Rust source) because it is a small, load-bearing constant this
# script needs BEFORE the server it describes has booted, to compute the hang-watchdog deadline
# and the shed/cold-build cells' own timeout overrides.
DEFAULT_ADMISSION_TIMEOUT_MS = 250


def sample_process(pid: int, stop: threading.Event, out: list) -> None:
    """Poll a process's RSS and CPU until told to stop.

    Server-side, deliberately: the generator and the server share a 12-core box, so reporting only
    one of them would hide contention that is real and that a reader needs to see.
    """
    stat_path = Path(f"/proc/{pid}/stat")
    status_path = Path(f"/proc/{pid}/status")
    ticks = 100.0
    prev_cpu = None
    prev_t = None
    while not stop.is_set():
        try:
            fields = stat_path.read_text().rsplit(") ", 1)[1].split()
            cpu = (int(fields[11]) + int(fields[12])) / ticks
            rss_kib = 0
            for line in status_path.read_text().splitlines():
                if line.startswith("VmRSS:"):
                    rss_kib = int(line.split()[1])
                    break
            now = time.monotonic()
            pct = None
            if prev_cpu is not None:
                pct = 100.0 * (cpu - prev_cpu) / max(now - prev_t, 1e-6)
            out.append({"t": now, "rss_kib": rss_kib, "cpu_pct": pct})
            prev_cpu, prev_t = cpu, now
        except (FileNotFoundError, IndexError, ValueError):
            pass
        stop.wait(0.25)


def sample_cpu_mean(pid: int, stop: threading.Event, samples: list) -> float:
    stop.set()
    cpu = [s["cpu_pct"] for s in samples if s["cpu_pct"] is not None]
    return sum(cpu) / len(cpu) if cpu else 0.0


def authorise(srv, terms: list[str]) -> str:
    import base64

    auth = json.dumps({"terms": terms}).encode()
    resp = requests.post(
        f"{srv.session_base}/session/authorise",
        headers={"Authorization": f"Bearer {harness.SESSION_CREDENTIAL}"},
        json={"auth_data": base64.b64encode(auth).decode()},
        timeout=120,
    )
    resp.raise_for_status()
    return resp.json()["token"]


def build_tokens(srv, descriptors: list[str], n: int, distinct: bool, w: int, seed: int) -> list[str]:
    """One token per virtual user.

    Arm A gives every user its own grant set, so no two share a fragment. Arm B draws from a small
    pool, so they do. The distinction is the whole experiment, so it is made here, explicitly,
    rather than left to the generator to infer.
    """
    rng = random.Random(seed)
    if distinct:
        tokens = []
        for i in range(n):
            local = random.Random(seed * 1_000_003 + i)
            terms = local.sample(descriptors, min(w, len(descriptors)))
            tokens.append(authorise(srv, terms))
        return tokens
    pool_size = min(4, n)
    pool = [
        authorise(srv, rng.sample(descriptors, min(w, len(descriptors))))
        for _ in range(pool_size)
    ]
    return [pool[i % pool_size] for i in range(n)]


def warm_viewport(srv, token: str, zoom: int, k: int) -> bool:
    """Fire one viewport request over the full extent so the token's row projection (and, on the
    fragment side, its mask) is built and cached before a timed cell starts. Returns whether it
    succeeded -- a 429 here (D-G single-flight racing a concurrent warm-up on the same token,
    which never happens in this script's own serial use, or a saturated gate) means the caller
    should not assume the session is actually warm."""
    try:
        resp = requests.post(
            f"{srv.viewer_base}/v1/viewport",
            headers={"Authorization": f"Bearer {token}"},
            json={"slice": "s0", "zoom": zoom, "bbox": [0, 0, 65536, 65536], "k": k},
            timeout=60,
        )
        return resp.status_code == 200
    except requests.RequestException:
        return False


def run_load(
    args,
    tokens: list[str],
    concurrency: int,
    run_dir: Path,
    healthz: bool,
    srv,
    scale: int,
    label_set: str,
    *,
    duration: float | None = None,
    open_rate: float | None = None,
    pan_storm: bool = False,
    abort_after_ms: int | None = None,
    hang_timeout_ms: int | None = None,
    cold_workers: int = 0,
    truncate_tokens: bool = True,
    tag: str = "",
) -> dict | None:
    suffix = f"-{tag}" if tag else ""
    tokens_file = run_dir / f"tokens-{concurrency}-{'h' if healthz else 'v'}{suffix}.txt"
    write_tokens = tokens[:concurrency] if truncate_tokens else tokens
    tokens_file.write_text("\n".join(write_tokens) + "\n")

    cmd = [
        str(BENCH_BIN), "load",
        "--viewer-url", srv.viewer_base,
        "--tokens", str(tokens_file),
        "--concurrency", str(concurrency),
        "--duration-s", str(duration if duration is not None else args.duration),
        "--threads", str(args.threads),
        "--bundle-scale", str(scale),
        "--bundle-label-set", label_set,
        "--k", str(args.k),
        "--zoom", str(args.zoom),
        "--run-dir", str(run_dir),
    ]
    if healthz:
        cmd.append("--healthz")
    if open_rate is not None:
        cmd += ["--rate", str(open_rate)]
    if pan_storm:
        cmd.append("--pan-storm")
    if abort_after_ms is not None:
        cmd += ["--abort-after-ms", str(abort_after_ms)]
    if hang_timeout_ms is not None:
        cmd += ["--hang-timeout-ms", str(hang_timeout_ms)]
    if cold_workers:
        cmd += ["--cold-workers", str(cold_workers)]

    proc = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"  load failed: {proc.stderr.strip()[:400]}")
        return None

    records = [json.loads(l) for l in (run_dir / "load.jsonl").read_text().splitlines() if l.strip()]
    return records[-1] if records else None


def run_matrix(args, descriptors: list[str], srv, proc, summary: dict) -> None:
    """The original Arm A/B sweep (criteria 1 and, via the hang watchdog wired into every load
    call, 2)."""
    levels = [int(c) for c in args.concurrency.split(",")]
    admission_timeout_ms = args.admission_timeout_ms or DEFAULT_ADMISSION_TIMEOUT_MS
    # D-E: 25x the 10 ms p99 target is the server's own admission timeout; +2 s is criterion 2's
    # own service bound on top of it. A sample that exceeds this on the CLIENT side is a positive
    # hang detection, not merely a slow response -- see `StormOptions::hang_timeout_ms`'s doc.
    hang_timeout_ms = admission_timeout_ms + 2000

    # The generator's own ceiling, first. Every viewport number below is read against this.
    print("\ncalibrating generator ceiling against /healthz")
    ceiling = {}
    warm = build_tokens(srv, descriptors, 1, False, args.w, args.seed)
    for c in levels:
        rec = run_load(args, warm * c, c, args.run_dir, True, srv, args.scale, args.label_set)
        if rec:
            rps = rec["params"]["throughput_rps"]
            ceiling[c] = rps
            print(f"  c={c:<5} healthz {rps:>10,.0f} rps  p99={rec['timing']['p99_ns']/1e6:>7.2f} ms")

    for arm in args.arms.split(","):
        arm = arm.strip().upper()
        distinct = arm == "A"
        label = "A (distinct principals)" if distinct else "B (shared fragments)"
        print(f"\nArm {label}")
        print(f"  {'conc':>6} {'rps':>10} {'p50_ms':>9} {'p99_ms':>9} {'srv_p99_ms':>11} "
              f"{'shed%':>7} {'hung':>5} {'rss_gib':>9} {'cpu%':>7}  flags")

        for c in levels:
            rss_before = 0
            try:
                for line in Path(f"/proc/{proc.pid}/status").read_text().splitlines():
                    if line.startswith("VmRSS:"):
                        rss_before = int(line.split()[1])
            except FileNotFoundError:
                pass

            t0 = time.monotonic()
            tokens = build_tokens(srv, descriptors, c, distinct, args.w, args.seed)
            auth_s = time.monotonic() - t0

            samples: list = []
            stop = threading.Event()
            sampler = threading.Thread(target=sample_process, args=(proc.pid, stop, samples))
            sampler.start()
            rec = run_load(
                args, tokens, c, args.run_dir, False, srv, args.scale, args.label_set,
                hang_timeout_ms=hang_timeout_ms,
            )
            stop.set()
            sampler.join()

            if not rec:
                continue

            rss_peak = max((s["rss_kib"] for s in samples), default=0)
            cpu = [s["cpu_pct"] for s in samples if s["cpu_pct"] is not None]
            cpu_mean = sum(cpu) / len(cpu) if cpu else 0.0
            rps = rec["params"]["throughput_rps"]
            shed_rate = rec["params"].get("shed_rate", 0.0)
            hung = rec["params"].get("requests_hung", 0)

            flags = list(rec.get("flags", []))
            if c in ceiling and rps > ceiling[c] / 3.0:
                # Within 3x of what the generator can do against a trivial endpoint: this cell
                # is measuring the client as much as the server.
                flags.append("generator_bound")

            print(f"  {c:>6} {rps:>10,.0f} {rec['timing']['median_ns']/1e6:>9.2f} "
                  f"{rec['timing']['p99_ns']/1e6:>9.2f} {rec['params']['server_us_p99']/1000:>11.2f} "
                  f"{shed_rate*100:>6.1f}% {hung:>5} "
                  f"{rss_peak/1048576:>9.2f} {cpu_mean:>7.0f}  {','.join(flags)}")

            summary["cells"].append({
                "arm": arm, "concurrency": c, "rps": rps,
                "p50_ms": rec["timing"]["median_ns"] / 1e6,
                "p99_ms": rec["timing"]["p99_ns"] / 1e6,
                "server_us_p99": rec["params"]["server_us_p99"],
                "max_wall_ms": rec["params"].get("max_wall_ms"),
                "requests_hung": hung,
                "requests_shed_429": rec["params"].get("requests_shed_429"),
                "shed_rate": shed_rate,
                "retry_after_violations": rec["params"].get("retry_after_violations"),
                "rss_peak_kib": rss_peak, "rss_before_kib": rss_before,
                "rss_delta_kib": rss_peak - rss_before,
                "authorise_s": auth_s, "server_cpu_pct_mean": cpu_mean,
                "distinct_tokens": rec["params"]["distinct_tokens"],
                "generator_ceiling_rps": ceiling.get(c),
                "flags": flags,
            })

            if rss_peak / 1048576 > args.rss_abort_gib:
                print(f"  RSS {rss_peak/1048576:.1f} GiB exceeded --rss-abort-gib "
                      f"{args.rss_abort_gib}; stopping this arm. The curve up to here IS the "
                      f"result.")
                break


def run_cpu_saturation_cell(args, descriptors: list[str], srv, proc, summary: dict) -> None:
    """Criterion 4: server CPU >= ~85% of AVAILABLE cores (cores - generator's own) under
    open-loop load near capacity, plus a c=1 latency comparison across `compute_threads` so the
    tile-loop parallelism's payoff on a single request is visible independent of concurrency."""
    print("\ncriterion 4a: CPU saturation, open-loop near capacity (Arm B, shared fragments)")
    available_cores = max(os.cpu_count() or 12, 1) - args.threads
    threshold_pct = 0.85 * available_cores * 100.0

    # Rate near what the matrix's own Arm B closed-loop sweep already found saturates -- read
    # back out of `summary` rather than re-measured, so this cell costs one run, not a search.
    b_cells = [c for c in summary["cells"] if c["arm"] == "B"]
    rate = max((c["rps"] for c in b_cells), default=20000.0) * 0.9
    tokens = build_tokens(srv, descriptors, 4, False, args.w, args.seed)
    tokens = [tokens[i % len(tokens)] for i in range(available_cores * 4)]

    samples: list = []
    stop = threading.Event()
    sampler = threading.Thread(target=sample_process, args=(proc.pid, stop, samples))
    sampler.start()
    rec = run_load(
        args, tokens, len(tokens), args.run_dir, False, srv, args.scale, args.label_set,
        open_rate=rate, tag="cpu-openloop",
    )
    # Fix-wave minor: stop-JOIN-read, matching the matrix loop's own pattern above (`stop.set()`
    # then `sampler.join()` then read `samples`) -- `sample_cpu_mean` used to be called before the
    # join, reading `samples` while the sampler thread might still be mid-append (benign in
    # practice under the GIL, since list.append is atomic, but the ordering was needlessly
    # inconsistent with the rest of the file and worth matching exactly).
    stop.set()
    sampler.join()
    cpu_mean = sample_cpu_mean(proc.pid, stop, samples)

    result = {"target_rate": rate, "available_cores": available_cores,
              "threshold_pct": threshold_pct, "cpu_mean_pct": cpu_mean}
    if rec:
        result["rps"] = rec["params"]["throughput_rps"]
        result["p99_ms"] = rec["timing"]["p99_ns"] / 1e6
        print(f"  rate={rate:,.0f} rps target  achieved={rec['params']['throughput_rps']:,.0f} rps  "
              f"cpu={cpu_mean:.0f}% (threshold {threshold_pct:.0f}% of {available_cores} cores)")
    summary["cpu_saturation_cell"] = result

    print("\ncriterion 4b: c=1 latency, multi-tile viewport, compute_threads 1 vs machine default")
    thread_result = {}
    for threads_label, compute_threads in (("threads=1", 1), ("threads=default", None)):
        tmp_dir = args.run_dir / f"server-threads-{compute_threads or 'default'}"
        tmp_dir.mkdir(exist_ok=True, parents=True)
        s, p, _boot_s = spawn_with_long_boot_deadline(
            args.bundle, tmp_dir, 1800.0, max(args.k, 200), compute_threads=compute_threads,
        )
        try:
            tok = build_tokens(s, descriptors, 1, False, args.w, args.seed)
            warm_viewport(s, tok[0], args.zoom, args.k)
            rec = run_load(
                args, tok, 1, args.run_dir, False, s, args.scale, args.label_set,
                duration=5.0, tag=f"c1-{compute_threads or 'default'}",
            )
            if rec:
                p50_ms = rec["timing"]["median_ns"] / 1e6
                thread_result[threads_label] = p50_ms
                print(f"  {threads_label:<16} c=1 p50={p50_ms:.3f} ms  "
                      f"server_p50={rec['params']['server_us_p50']/1000:.3f} ms")
        finally:
            harness.stop_server(p)
    summary["thread_scaling_cell"] = thread_result


def run_shed_cell(args, descriptors: list[str], summary: dict) -> None:
    """Criterion 3: forced-low admission (`compute_admission`/`compute_queue`) so the gate sheds
    deterministically once warm sessions are driven past it."""
    print("\ncriterion 3: shed activates at the bound (forced-low admission)")
    tmp_dir = args.run_dir / "server-shed"
    tmp_dir.mkdir(exist_ok=True, parents=True)
    srv, proc, boot_s = spawn_with_long_boot_deadline(
        args.bundle, tmp_dir, 1800.0, max(args.k, 200),
        compute_admission=args.shed_compute_admission,
        compute_queue=args.shed_compute_queue,
        # Fix-wave minor: this was previously omitted here while the hang-timeout computation a
        # few lines below already reads `args.admission_timeout_ms` -- a latent desync if
        # `--admission-timeout-ms` is ever passed alongside `--criteria shed` (the server would
        # boot at the default timeout while the client's watchdog assumed the overridden one).
        admission_timeout_ms=args.admission_timeout_ms,
    )
    try:
        levels = [int(c) for c in args.concurrency.split(",")]
        concurrency = args.shed_concurrency or max(levels)
        tokens = build_tokens(srv, descriptors, 4, False, args.w, args.seed)
        tokens = [tokens[i % len(tokens)] for i in range(concurrency)]
        # Warm every distinct token serially BEFORE the timed window, one request at a time (the
        # forced-low gate would shed a second concurrent warm-up anyway) -- so the shed the timed
        # window observes is admission shed (D-B), not D-G's building-shed on top of it.
        for t in set(tokens):
            warm_viewport(srv, t, args.zoom, args.k)

        hang_timeout_ms = (args.admission_timeout_ms or DEFAULT_ADMISSION_TIMEOUT_MS) + 5000
        rec = run_load(
            args, tokens, concurrency, args.run_dir, False, srv, args.scale, args.label_set,
            hang_timeout_ms=hang_timeout_ms, tag="shed",
        )
        result = {
            "compute_admission": args.shed_compute_admission,
            "compute_queue": args.shed_compute_queue,
            "concurrency": concurrency,
        }
        if rec:
            result.update({
                "shed_rate": rec["params"]["shed_rate"],
                "requests_shed_429": rec["params"]["requests_shed_429"],
                "requests_ok": rec["params"]["requests_ok"],
                "retry_after_violations": rec["params"]["retry_after_violations"],
                "requests_hung": rec["params"]["requests_hung"],
                "served_p50_ms": rec["timing"]["median_ns"] / 1e6,
                "served_p99_ms": rec["timing"]["p99_ns"] / 1e6,
                "max_wall_ms": rec["params"]["max_wall_ms"],
            })
            print(f"  compute_admission={args.shed_compute_admission} compute_queue="
                  f"{args.shed_compute_queue} c={concurrency}: shed_rate="
                  f"{rec['params']['shed_rate']*100:.1f}% ok={rec['params']['requests_ok']} "
                  f"retry_after_violations={rec['params']['retry_after_violations']} "
                  f"served_p99={rec['timing']['p99_ns']/1e6:.2f}ms hung={rec['params']['requests_hung']}")
        summary["shed_cell"] = result
    finally:
        harness.stop_server(proc)


def run_panstorm_cell(args, descriptors: list[str], summary: dict) -> None:
    """Criterion 5: warm sessions, client aborts every `--pan-storm-abort-ms` at c >= cores,
    completed-request throughput compared against a no-abort baseline, plus a `/control/status`
    permit-leak check before/after."""
    print("\ncriterion 5: pan-storm (warm sessions, client aborts)")
    tmp_dir = args.run_dir / "server-panstorm"
    tmp_dir.mkdir(exist_ok=True, parents=True)
    srv, proc, boot_s = spawn_with_long_boot_deadline(
        args.bundle, tmp_dir, 1800.0, max(args.k, 200),
        compute_threads=args.compute_threads, compute_admission=args.compute_admission,
        compute_queue=args.compute_queue, admission_timeout_ms=args.admission_timeout_ms,
    )
    try:
        cores = os.cpu_count() or 12
        concurrency = args.panstorm_concurrency or cores
        pool = build_tokens(srv, descriptors, 4, False, args.w, args.seed)
        tokens = [pool[i % len(pool)] for i in range(concurrency)]
        for t in set(tokens):
            warm_viewport(srv, t, args.zoom, args.k)

        status_before = srv.status()["compute"]

        baseline = run_load(
            args, tokens, concurrency, args.run_dir, False, srv, args.scale, args.label_set,
            tag="panstorm-baseline",
        )
        storm = run_load(
            args, tokens, concurrency, args.run_dir, False, srv, args.scale, args.label_set,
            pan_storm=True, abort_after_ms=args.pan_storm_abort_ms, tag="panstorm",
        )

        status_after = srv.status()["compute"]

        result = {
            "concurrency": concurrency,
            "abort_after_ms": args.pan_storm_abort_ms,
            "status_before": status_before,
            "status_after": status_after,
            "permit_leak": status_after["waiting"] != 0 or status_after["in_flight"] != 0,
        }
        if baseline and storm:
            baseline_rps = baseline["params"]["throughput_rps"]
            storm_rps = storm["params"]["throughput_rps"]
            ratio = storm_rps / baseline_rps if baseline_rps else 0.0
            result.update({
                "baseline_rps": baseline_rps,
                "storm_rps": storm_rps,
                "ratio": ratio,
                "storm_aborted": storm["params"]["requests_aborted"],
                "storm_hung": storm["params"]["requests_hung"],
            })
            print(f"  c={concurrency} abort_after={args.pan_storm_abort_ms}ms: "
                  f"baseline={baseline_rps:,.0f} rps  storm={storm_rps:,.0f} rps  "
                  f"ratio={ratio:.2f}  aborted={storm['params']['requests_aborted']} "
                  f"hung={storm['params']['requests_hung']}")
        print(f"  gate status before={status_before}  after={status_after}")
        summary["panstorm_cell"] = result
    finally:
        harness.stop_server(proc)


def run_coldbuild_cell(args, descriptors: list[str], summary: dict) -> None:
    """Criterion 6: one cold token whose row-projection build is the most expensive buildable at
    this scale without an artificial sleep (near-every descriptor granted, so the union covers
    close to the whole corpus), hammered by many concurrent same-key workers while a separate
    warm pool's traffic on OTHER keys runs at the same time. `admission_timeout_ms` is also
    lowered (`--coldbuild-admission-timeout-ms`) since the build cannot be made arbitrarily slow
    at 2.42M scale and this is the other lever available to get clearly past it -- documented in
    the report as the approximation it is."""
    print("\ncriterion 6: cold-build storm (D-G non-blocking single-flight)")
    tmp_dir = args.run_dir / "server-coldbuild"
    tmp_dir.mkdir(exist_ok=True, parents=True)
    srv, proc, boot_s = spawn_with_long_boot_deadline(
        args.bundle, tmp_dir, 1800.0, max(args.k, 200),
        admission_timeout_ms=args.coldbuild_admission_timeout_ms,
    )
    try:
        cold_w = args.coldbuild_cold_w or len(descriptors)
        t0 = time.monotonic()
        cold_token = build_tokens(srv, descriptors, 1, True, cold_w, args.seed + 999)[0]
        cold_authorise_s = time.monotonic() - t0

        warm_pool = args.coldbuild_warm_pool
        warm_tokens = build_tokens(srv, descriptors, warm_pool, False, args.w, args.seed)
        for t in warm_tokens:
            warm_viewport(srv, t, args.zoom, args.k)

        levels = [int(c) for c in args.concurrency.split(",")]
        cold_workers = args.coldbuild_cold_workers or min(200, max(levels))
        warm_workers = warm_pool * 4
        concurrency = cold_workers + warm_workers
        tokens = [cold_token] + warm_tokens

        hang_timeout_ms = args.coldbuild_admission_timeout_ms + 10_000
        rec = run_load(
            args, tokens, concurrency, args.run_dir, False, srv, args.scale, args.label_set,
            cold_workers=cold_workers, truncate_tokens=False, hang_timeout_ms=hang_timeout_ms,
            tag="coldbuild",
        )
        result = {
            "cold_w": cold_w,
            "cold_authorise_s": cold_authorise_s,
            "admission_timeout_ms": args.coldbuild_admission_timeout_ms,
            "cold_workers": cold_workers,
            "warm_workers": warm_workers,
        }
        if rec:
            cb = rec["params"].get("cold_build", {})
            result.update({
                "cold_build": cb,
                "requests_hung": rec["params"]["requests_hung"],
                "retry_after_violations": rec["params"]["retry_after_violations"],
            })
            print(f"  cold_w={cold_w} ({cold_authorise_s:.1f}s to authorise) "
                  f"admission_timeout_ms={args.coldbuild_admission_timeout_ms} "
                  f"cold_workers={cold_workers} warm_workers={warm_workers}")
            print(f"  cold key: {cb.get('cold_requests_ok')} ok / {cb.get('cold_requests_shed')} "
                  f"shed / max_wall={cb.get('cold_max_wall_ms'):.2f}ms  "
                  f"warm keys: {cb.get('warm_requests_ok')} ok, p99="
                  f"{(cb.get('warm_p99_ms') or 0):.2f}ms  hung={rec['params']['requests_hung']}")
        summary["coldbuild_cell"] = result
    finally:
        harness.stop_server(proc)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", type=Path, required=True)
    ap.add_argument("--scale", type=int, required=True)
    ap.add_argument("--label-set", default="categories-subclass")
    ap.add_argument("--concurrency", default="5,10,100,1000")
    ap.add_argument("--arms", default="B,A", help="B=shared fragments, A=distinct fragments")
    ap.add_argument("--duration", type=float, default=10.0)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--w", type=int, default=100, help="grant width per principal")
    ap.add_argument("--k", type=int, default=30)
    ap.add_argument("--zoom", type=int, default=8)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--rss-abort-gib", type=float, default=30.0)
    ap.add_argument("--run-dir", type=Path, default=Path("/tmp/tessera-bench/runs/concurrency"))

    # Task 9: which of the seven-criterion cells to run. `matrix` alone reproduces the original
    # Arm A/B sweep byte-for-byte in shape (just with the shed-rate column and hang watchdog
    # added); the other four are new, separately-booted cells.
    ap.add_argument("--criteria", default="matrix,cpu,shed,panstorm,coldbuild",
                     help="comma-separated subset of: matrix,cpu,shed,panstorm,coldbuild")

    # D-B/D-E admission-gate overrides for the MAIN matrix boot. `None` (default) means "use the
    # server's own defaults" -- see `write_config_with_max_k`'s doc.
    ap.add_argument("--compute-threads", type=int, default=None)
    ap.add_argument("--compute-admission", type=int, default=None)
    ap.add_argument("--compute-queue", type=int, default=None)
    ap.add_argument("--admission-timeout-ms", type=int, default=None)

    ap.add_argument("--pan-storm-abort-ms", type=int, default=50)
    ap.add_argument("--panstorm-concurrency", type=int, default=None,
                     help="default: os.cpu_count()")

    ap.add_argument("--shed-compute-admission", type=int, default=1)
    ap.add_argument("--shed-compute-queue", type=int, default=0)
    ap.add_argument("--shed-concurrency", type=int, default=None,
                     help="default: max(--concurrency)")

    ap.add_argument("--coldbuild-admission-timeout-ms", type=int, default=20,
                     help="lowered from the 250ms default so the cold build (bounded by what's "
                          "buildable at this scale without an artificial sleep) is clearly >> it")
    ap.add_argument("--coldbuild-cold-w", type=int, default=None,
                     help="grant width for the cold token; default: every descriptor")
    ap.add_argument("--coldbuild-cold-workers", type=int, default=None,
                     help="default: min(200, max(--concurrency))")
    ap.add_argument("--coldbuild-warm-pool", type=int, default=8)

    args = ap.parse_args()

    if not BENCH_BIN.exists():
        print(f"missing {BENCH_BIN} -- cargo build --release -p tessera-bench", file=sys.stderr)
        return 1

    criteria = {c.strip() for c in args.criteria.split(",") if c.strip()}
    args.run_dir.mkdir(parents=True, exist_ok=True)
    tmp_dir = args.run_dir / "server"
    tmp_dir.mkdir(exist_ok=True)

    descriptors = read_dictionary_descriptors(args.bundle)
    print(f"bundle {args.bundle} -- {len(descriptors)} descriptors")

    summary = {"scale": args.scale, "label_set": args.label_set, "cells": []}

    if "matrix" in criteria or "cpu" in criteria:
        print("booting server (matrix/cpu)...")
        srv, proc, boot_s = spawn_with_long_boot_deadline(
            args.bundle, tmp_dir, 1800.0, max(args.k, 200),
            compute_threads=args.compute_threads, compute_admission=args.compute_admission,
            compute_queue=args.compute_queue, admission_timeout_ms=args.admission_timeout_ms,
        )
        summary["boot_s"] = boot_s
        print(f"  up in {boot_s:.1f}s (pid {proc.pid})")
        try:
            if "matrix" in criteria:
                run_matrix(args, descriptors, srv, proc, summary)
            if "cpu" in criteria:
                run_cpu_saturation_cell(args, descriptors, srv, proc, summary)
        finally:
            harness.stop_server(proc)

    if "shed" in criteria:
        run_shed_cell(args, descriptors, summary)

    if "panstorm" in criteria:
        run_panstorm_cell(args, descriptors, summary)

    if "coldbuild" in criteria:
        run_coldbuild_cell(args, descriptors, summary)

    out = args.run_dir / "concurrency-summary.json"
    out.write_text(json.dumps(summary, indent=2))
    print(f"\nwrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
