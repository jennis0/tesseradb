# 0011 — `/healthz` and `/readyz` leave the control plane

**Date:** 2026-08-01 · **Status:** Settled

## Decision

The health probes stay on the viewer and session listeners, where they already were. They are not
served from the control plane.

## Why

**The control plane should have no element a non-admin system needs to reach, so it can be
physically blocked.** A health probe is exactly such an element: load balancers and orchestrators
must reach it, and they are not administrators.

The larger gain came out second: with the probes gone, the control plane's authentication layer
needs **no exemption at all**. Every route on that plane is authenticated, including the 404
fallback, because the check is a `Router::layer` rather than a per-route list.

That matters because the failure it prevents has already happened once: `/control/status` leaked
the corpus high-water mark because its handler simply did not check. An exemption list is a place
for that to happen again.

## Cost, stated

One bit is given up: the control plane can no longer be health-checked independently of the other
two listeners.

## Evidence

Owner decision, stage 2.1 ledger. Contracts r11 §3.1. `crates/tessera-server/src/health.rs`.
