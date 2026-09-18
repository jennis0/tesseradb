# 0144 — A write is visible at a numbered publication, and a client may wait for it

**Date:** 2026-09-17 · **Status:** Settled (owner ruling, in conversation on the Python SDK's flush
wait; [`python-sdk.md`](../design/python-sdk.md) §11.1)

## Context

A write is acknowledged when it is durable and becomes visible at the next flush tick. A client
that wants to read what it wrote had one signal, the per-partition `segments_version`, which moves
only when a tick writes a segment. A tick that only fills values or only publishes artifact row
forms moves nothing a client may read, so the SDK's first commit wait ran to its timeout on those
commits, and its workaround read two executor counters the contract marks as unstable.

The comparable systems settle this the same way. Elasticsearch returns a sequence number on every
index request and takes `refresh=wait_for` on the request itself; Postgres returns the log
position of a commit and a replica exposes the position it has applied. The position advances only
on success, so "position reached" is "visible" without qualification.

## The decision

- **`publication`**, on `GET /control/status`: the number of publication cycles this executor has
  completed. It moves once per cycle that published, whatever it published (a segment, a
  values-only substitution, artifact row forms, or nothing over an empty buffer), and it never
  passes a cycle that was gated, failed on the pool or was discarded at its final check; such a
  cycle stays open with its request armed and the counter moves at the retry that succeeds. It is
  per process and starts at zero.
- **Every write acknowledgement carries `publication`**: the cycle its work becomes visible in,
  computed when the write was acknowledged. `POST /control/flush` answers the same number and
  remains the way to publish now.
- **`?wait=visible`** on every write route holds the answer until the counter has reached that
  number, pulling the tick forward as a flush request does, bounded by
  `serve.visible_wait_max_secs` (default 30). Past the bound the answer is the one the route would
  have sent, with `visible: false`. It is for a single writer such as a notebook; a bulk loader
  sends its pages unwaited and one flush at the end, since a waited page is a tick per page.
- **A replayed page says so**: `accepted: 0` and `replayed: true` on the ingest route, `filled: 0`
  and `replayed: true` on the values route, with `tessera_ids` the original allocation in full.

## What was declined

Counting ticks. A tick has fired before the publication it dispatched is applied, so a count that
moves at the tick names work still being written. Counting cycles that ended, whether or not they
published. A client that waited for such a number and found nothing published would have to read
the failure counters to know, and a client that did not would take the return as success.

## Consequences

Contracts §3.4's flush, status and every write route's row carry the field, the parameter and the
replay flag. A declaration and a deny are in force from their acknowledgement, so the number they
carry promises nothing further and no reader waits on it. The SDK's `commit()` sends its pages
unwaited and finishes with one flush carrying `wait=visible`.
