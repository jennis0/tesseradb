### Task 8: Part VI — What was measured, and what is still open

**Files:**
- Create: `docs/whitepaper/src/60-part-6.html`
- Read: `docs/whitepaper/facts/part-6.md`

**Interfaces:**
- Produces: figure IDs `fig-f15`, `fig-f16`. After this task the validator reports zero failures.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/validate.py | grep -E "f15|f16"
```

- [ ] **Step 2: Write the two chapters**

Chapter 16 — the billion-row table: exact masked counting at 0.1–0.3 ms across every zoom depth, including one call counting a masked billion rows in 191 µs, with the measurement conditions stated (WSL2, 12 cores, 39 GB; timings indicative). Then the counterweight: permuting a 69M-item mask into row space costs 8.8 s, which is why it is cached per (token, slice, pin) and must never reach the per-viewport path. Chapter 17 — still open: real-label policy shape, sharded placement, retroactive revocation, and the leak register. Roughly 1,200–1,600 words.

- [ ] **Step 3: Build F15 — the billion-row table** *(static)*

The depth / rows-per-tile / masked-count table from `probes/results.md` §6, rendered as a figure with the conditions in the caption. Values must match the source exactly — transcribe, do not round.

- [ ] **Step 4: Build F16 — the leak register** *(interactive)*

C1–C16 as a filterable table, presented as a property of the design rather than an appendix to it. Each entry states the channel and why it is accepted. Text filter input plus a category filter.

- [ ] **Step 5: Build, validate, render**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: validator prints `PASS` with zero failures; render passes.

- [ ] **Step 6: Checkpoint — the paper is now complete but unreviewed**
