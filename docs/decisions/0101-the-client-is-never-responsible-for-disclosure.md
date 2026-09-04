# 0101 — The client is never responsible for disclosure; its obligations are truthfulness

**Date:** 2026-08-24 · **Status:** Settled (owner direction on the four-customer re-cut;
transcribed 2026-08-25 from [`client-components.md`](../design/client-components.md) §1).

## The decision

The server decides what a principal may have before any byte leaves it, whatever the client asked
for. **The client is never responsible for disclosure.** What a client can get wrong is
**truthfulness** — presenting a sample as a set, a stale view as current, a refusal as an empty
corpus, a masked count as a size — and the obligations the client design carries are all of that
kind (client-interaction §10). Two things are of a different kind and are treated as such:
**credentials** — where a token comes from and where it may be held (the session credential never
reaches a browser; `session-url` is on no C1 or C2 surface) — and **the leak register**, which any
wire addition the client asks for must pass. Everything else is ergonomics, and is reported rather
than refused.

## Why

Putting any disclosure obligation on the client would mean the security of the system depended on
code the principal runs; the whole design ([architecture §4](../design/architecture.md)) is that
it does not. Naming truthfulness as the client's subject keeps the client's strictness where it
belongs — in the types (`Count` versus `Masked`) and the display states — and stops the
fail-closed posture of the server being imitated in a component that has nothing to close.
