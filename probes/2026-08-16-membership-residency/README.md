# Artifact membership: what it costs resident

**Date:** 2026-08-16 · **Harness:** `cargo run --release --bin membership_residency`
· **Host:** WSL2, 47 GB RAM, glibc malloc

`annotation-representation.md` §2 sizes artifact membership at **794 MB** for 10⁷ artifacts over 10⁹
rows, and §11.3 marks the residency unmeasured in the same breath. `artifact-delivery.md` §7 makes
it the item that could refute the shape: if resident cost is materially worse than serialised, the
fine-level case stops being servable and the deleted assignment column (rep §2.7) comes back for
that regime. This is the measurement.

## The result

**Resident cost is ~80–94 bytes per Roaring container, and it is flat.** Not per artifact, not per
member — per container. The ratio to serialised bytes follows from that and is 6.2× where
membership is contiguous, 7.8× where it is scattered.

| arm | artifacts | serialised MB | resident MB | ratio | containers | B/container |
|---|---:|---:|---:|---:|---:|---:|
| runs | 10 000 | 0.6 | 3.6 | 6.17 | 40 008 | 94.1 |
| runs | 100 000 | 5.8 | 35.9 | 6.17 | 400 108 | 94.0 |
| runs | 1 000 000 | 58.2 | 358.7 | 6.16 | 4 001 092 | 94.0 |
| **runs** | **10 000 000** | **581.8** | **3 586.7** | **6.16** | 40 010 858 | 94.0 |
| scattered | 10 000 | 9.6 | 74.7 | 7.79 | 996 819 | 78.5 |
| scattered | 100 000 | 95.9 | 746.5 | 7.79 | 9 967 953 | 78.5 |
| scattered | 1 000 000 | 958.8 | 7 464.7 | 7.79 | 99 676 754 | 78.5 |
| scattered | 10 000 000 | — | **OOM-killed** | — | — | — |

**The design's own point costs 3.6 GB resident** on the realistic arm, against 582 MB serialised.
**The pessimistic arm does not fit at that scale at all**: extrapolating the flat 7.79× gives ~75 GB
against 41 GB available, and the process is killed rather than merely slow.

## What the per-container constant means

A run container holding 25 members and an array container holding one cost the same ~80–94 bytes
resident. So **resident cost tracks container count, and container count tracks how scattered the
membership is in row space** — which is exactly the property Morton ordering and the signature sort
already buy. The realistic arm holds 4 containers per artifact where the scattered arm holds ~100,
and that one fact is the whole spread.

Two consequences, neither of which the ratio alone shows:

- **The row-space choice is doing more work than §2 credits it with.** §2 justifies row space on
  *storage* — 28–118× smaller than entity space. The residency result says the same contiguity pays
  again, in RAM, and at a constant nothing about the storage argument predicted.
- **Serialised bytes are 15.2 B/container on the realistic arm against 94.0 resident.** A form that
  could be used *in place* — mapped rather than deserialised — would therefore cost roughly what it
  costs on disk, a ~6× saving, and would shift the cost from anonymous memory the kernel cannot
  reclaim to page cache it can. That is the case for the frozen-format packaging option, and it is
  now a measured case rather than an aesthetic one.

## Multipliers this figure does not include

§11.3's warning stands and is not measured here: the figure **multiplies by slice, by level, and by
two during a replace**. And the engine currently holds *two* resident copies of every membership —
the entity-space store (`ArtifactStore`, the durable form) and the row-space projection
(`ArtifactRows`, per slice). At the design's point on the realistic arm that is 3.6 GB per copy
before any multiplier.

## What this does not measure, stated so it is not read as settled

- **Not a real clustering.** §2 measures 0.006–0.073 B/member on real membership against 0.61–1.03
  on the synthetic arm — a 14–170× spread — because where noise sits drives run count. The two arms
  here bracket that; a real clustering sits between them and closer to `runs`. What is measured is
  the **overhead term**, which is a property of container count rather than of what is in the
  containers, and that term is what §11.3 says was missing.
- **One allocator, one host.** glibc malloc on WSL2. A different allocator changes the per-container
  constant; it does not change that the constant is per container.
- **Nothing is timed.** This is a residency probe.

## Reproducing

```bash
cargo run --release --bin membership_residency -- --max-artifacts 1000000
cargo run --release --bin membership_residency -- --arm runs --artifacts 10000000
```

The sweep re-runs itself **one configuration per child process**, and that is a correction rather
than tidiness: a first revision swept in-process behind a fixed warm-up and the second arm's small
scales read *zero* resident bytes, because glibc had already grown the arena for the first arm and
the population fit inside memory the baseline had already counted. An RSS delta only means
anything against an allocator that has not already been handed the space.
