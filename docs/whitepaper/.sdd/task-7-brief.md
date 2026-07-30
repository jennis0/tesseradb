### Task 7: Part V — Time and change

**Files:**
- Create: `docs/whitepaper/src/50-part-5.html`
- Read: `docs/whitepaper/facts/part-5.md`

**Interfaces:**
- Produces: figure IDs `fig-f13`, `fig-f14`.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/validate.py | grep -E "f13|f14"
```

- [ ] **Step 2: Write the two chapters**

Chapter 14 — ingest: the WAL and its ack contract, buffer, watermark and overlay, then segments, merging and compaction. Chapter 15 — denial: generations, pins, and the three retirement rules, stated exactly as lifecycle §3 has them — deletion denies retire by the epoch ledger; suppressions retire only on unsuppress and never touch postings; predicate-change entries retire at their compaction fold. State that conflating them is fail-open, and that pins fix geometry but never authorisation: a suppression applies to a pinned request the moment it is accepted. Roughly 1,400–1,800 words.

- [ ] **Step 3: Build F13 — the write path** *(animated)*

A timeline animating write → WAL → ack → buffer → watermark → overlay → segment → merge. Respect `prefers-reduced-motion`: when set, render the final state statically with stage labels rather than animating.

- [ ] **Step 4: Build F14 — three retirement rules** *(interactive, load-bearing)*

One scripted interleaving with a stepper. Three lanes, one per rule, showing each retiring by its own mechanism. A "conflate the rules" toggle collapses them to a single mechanism and surfaces the resulting fail-open — the point where a viewer sees something they should not. Verify the scripted sequence against `facts/part-5.md` before building; a wrong interleaving here is a public misstatement of the lifecycle.

- [ ] **Step 5: Build, validate, render**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: two missing-figure failures remain; render passes.

- [ ] **Step 6: Read the screenshots, then checkpoint**
