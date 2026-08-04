# 0045 — Inert configuration keys are deleted, not kept parsed

**Date:** 2026-08-04 · **Status:** Settled (owner ruling)

## Context

Two keys were parsed, validated, documented — and read by nothing:

- **`ingest.flush_max_items`.** LSM heritage: in an LSM the size trigger bounds memtable memory
  between flushes. Here that job belongs to `ingest_buffer_max_items` (the admission 429), and
  the trigger role was explicitly rejected — publishing on trip makes the publication period a
  function of ingest rate, and every publication rotates cache keys. What remained specified was
  "mark the buffer flush-ready and wait for the tick", and readiness has no consumer: the tick
  never skips a non-empty buffer, and a flush consumes everything buffered for its slice. It was
  never a cap on how much a flush processes. The knob had no possible effect, while a `config.rs`
  comment claimed the tick read it.
- **`ingest.commit_window_max_age_ms`.** Decision [0034](0034-the-window-does-not-linger.md)
  ruled there is no linger and kept the key parsed-inert "so that making it live is a small
  change".

## Decision

**Both keys are deleted. A configuration key exists only while something reads it.**

An inert key is a claim the system does not honour — the same defect class as present-tense
prose about absent machinery (decision [0013](0013-mark-specified-vs-implemented.md)), wearing a
TOML syntax. The "cheap to make live later" argument inverts: re-adding a key when its consumer
arrives costs one commit and misleads nobody in the interim; keeping it costs a standing lie plus
the inertness test that polices it.

This supersedes 0034's keep-parsed clause only. 0034's substance — a window closes when its
queue drains; there is no linger and no age bound — stands unchanged, and the measurement its
"what this leaves open" section names is still the trigger for revisiting a linger, which would
arrive *with* its key.

## Consequences

- A deployment's `tessera.toml` naming either key is refused with an error naming it — louder
  than the silent no-op it was buying before, which is the point.
- `EngineConfig` loses `flush_max_items`; the flush tick's only knob is `flush_max_age_secs`,
  and the buffer's only bound is `ingest_buffer_max_items`.
- The inertness test for the age key goes with the key it policed.

## Provenance

Owner, 2026-08-04, reviewing the write-path consolidation: *"I'm kind of struggling to
understand why it exists. Flush should process however much has been ingested — why would we
want a cap on that?"* — it wasn't a cap, and nothing else either.
