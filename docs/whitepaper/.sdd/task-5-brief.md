### Task 5: Part III — Turning permission into arithmetic

**Files:**
- Create: `docs/whitepaper/src/30-part-3.html`
- Read: `docs/whitepaper/facts/part-3.md`

**Interfaces:**
- Produces: figure IDs `fig-f6`, `fig-f7`, `fig-f8`.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/validate.py | grep -E "f6|f7|f8"
```

- [ ] **Step 2: Write the three chapters**

Chapter 7 — the plugin boundary: grants become a boolean expression, which becomes terms, which index postings, whose union is one bitmap, built once per session and reused. Name the lineage — boolean expression indexing, partial evaluation — per the project's terminology convention. Chapter 8 — signature-sorted entity IDs: the measured compression, and why I9 makes the assignment permanent and unretrofittable. Chapter 9 — a viewport query end to end in five steps. Roughly 1,800–2,200 words.

- [ ] **Step 3: Build F6 — grants become a bitmap** *(interactive)*

Toggleable grant chips. Each toggle updates, in sequence: the DNF terms, the selected posting lists, their union, and the resulting mask. Show the term count changing so the reader sees the expansion is bounded.

- [ ] **Step 4: Build F7 — signature sorting, before and after** *(static)*

The same logical mask under two ID assignments: arbitrary, and signature-sorted. Container diagrams side by side with the measured compression ratio from `facts/part-3.md`. Caption states that this cannot be changed later.

- [ ] **Step 5: Build F8 — one query, five steps** *(interactive, load-bearing)*

A stepper through viewport → Morton ranges → row bitmap → AND with `M_auth` → count. Each stage highlights as it becomes active, carrying forward the artefact the previous stage produced. Forward and back controls; keyboard operable.

- [ ] **Step 6: Build, validate, render**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: eight missing-figure failures remain; render passes.

- [ ] **Step 7: Read the screenshots, then checkpoint**
