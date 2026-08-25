# 0096 — Layers are usually one; several only for different kinds of feature; the picker offers the closure

**Date:** 2026-08-24 · **Status:** Settled (owner ruling on the design canvas; transcribed 2026-08-25
from [`client-components.md`](../design/client-components.md) §5.3 and §11).

## The decision

A map usually draws **one** annotation layer. Several are on together only when they are
**different kinds of feature** — a geographic corpus with districts, incidents and routes as three
layers is the case. **Stacking label layers over one clustering is not.**

What the layer picker offers is a layer **with its dependents**: a clustering's labels are a second
layer that `depends_on` it (`[layer.labels]` expands to exactly that), so one entry names the
closure and the store names every layer in it in the request. The picker never shows a count of a
layer's artifacts, because the wire never carries one.

## Why

Each named layer costs its own pass on the server; the store should ask for what is on and nothing
else. A picker that lets a user stack several label layers over one clustering produces a screen of
overlapping names that answers no task in §5.1, and the picker's job is the tasks.
