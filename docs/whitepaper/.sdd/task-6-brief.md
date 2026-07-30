### Task 6: Part IV — Making a map out of a bitmap

**Files:**
- Create: `docs/whitepaper/src/40-part-4.html`
- Read: `docs/whitepaper/facts/part-4.md`

**Interfaces:**
- Produces: figure IDs `fig-f9`, `fig-f10`, `fig-f11`, `fig-f12`.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/validate.py | grep -E "f9|f10|f11|f12"
```

- [ ] **Step 2: Write the four chapters**

Chapter 10 — level of detail and sampling after masking: priority nesting, direct evaluation as the main selection route rather than a fallback, and I7. State in one sentence that reversing the order blanks the sparsest principals' maps silently; do not dramatise it further. Chapter 11 — labels, clusters and the containment frontier: labels gate on `M_auth`, never on the filtered mask; filters move the frontier up, never down (I3, I12). Chapter 12 — two masks: filters narrow points without dissolving the map. Chapter 13 — compartmented partitions and required-set gating. Roughly 2,000–2,400 words.

- [ ] **Step 3: Build F9 — sampling after masking** *(static)*

A small diagram contrasting the two orderings: mask-then-sample against sample-then-mask, with the second annotated as the failure. Static by design — the prose carries this point.

- [ ] **Step 4: Build F10 — the containment frontier** *(static)*

The frontier across zoom levels, with a filter applied, showing it moving up and never down.

- [ ] **Step 5: Build F11 — two masks** *(interactive)*

`M_auth` fixed; a control varies `M_filter`. Points thin out while density, clusters and the frontier continue to behave. Carries the visual load for this part.

- [ ] **Step 6: Build F12 — compartments and required sets** *(static)*

Partitions with their required sets and the gate, showing how queries compose across them.

- [ ] **Step 7: Build, validate, render**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: four missing-figure failures remain; render passes.

- [ ] **Step 8: Read the screenshots, then checkpoint**
