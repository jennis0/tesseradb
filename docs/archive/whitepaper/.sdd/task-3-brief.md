### Task 3: Part I — The empty quadrant

**Files:**
- Create: `docs/archive/whitepaper/src/10-part-1.html`
- Read: `docs/archive/whitepaper/facts/part-1.md`

**Interfaces:**
- Consumes: `TF.onThemeChange`, `TF.svg`, `TF.fmt` from Task 2; CSS tokens `--cat-1` … `--cat-6`.
- Produces: figure IDs `fig-f1`, `fig-f2`, `fig-f2b`.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/archive/whitepaper/validate.py | grep -E "f1|f2b"
```
Expected: `FAIL: missing figure: fig-f1`, `fig-f2`, `fig-f2b`.

- [ ] **Step 2: Write the three chapters**

Chapter 1 — three viewers, one viewport, three different maps. Chapter 2 — where the field draws the line: a count over records you cannot read is a disclosure, not a filtered view; cite the surveyed systems that permit exactly this. Chapter 3 — the ceiling: nobody serves 10⁹ identifiable, filterable, labelled points at all; systems reaching 10⁷ fix the sample before any user exists or ship every point to the client; systems reaching 10⁹ bin to rasters and discard identity. Land the two axes as independent.

Every competitor claim takes its citation from `facts/part-1.md`. Roughly 1,400–1,800 words.

- [ ] **Step 3: Build F1 — three viewers, one viewport** *(interactive)*

A fixed synthetic point cloud (generate deterministically in-page from a seeded PRNG; no data files). Three persona buttons. Switching persona re-renders points, the displayed item count, cluster bubbles and a density shading — all four change together. Caption states the point: it is not only which dots appear, but every number derived from them.

- [ ] **Step 4: Build F2 — where the line is drawn** *(static)*

Two query plans side by side. Left: scan the corpus, aggregate, then filter to the rows the viewer may read — with the aggregate annotated as the leak. Right: aggregate inside the mask. Label the leaked quantity explicitly.

- [ ] **Step 5: Build F2b — the empty quadrant** *(interactive)*

Scatter plot. x: demonstrated interactive scale, log, 10³ → 10⁹. y: access-control granularity, three bands — none / dataset / row. One point per surveyed system, positioned from `facts/part-1.md` only. Systems reaching high scale by binning carry a marker whose legend entry reads "identity discarded". Hover or focus a point to reveal its citation. The top-right region is shaded and labelled as empty. Keyboard-accessible: points are focusable and reveal the same tooltip on focus.

- [ ] **Step 6: Build, validate, render**

```bash
cd /home/joe/code/tessera
python3 docs/archive/whitepaper/build.py && python3 docs/archive/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/archive/whitepaper/render_check.py
```
Expected: the three Part I figure failures are gone; fourteen remain; render passes.

- [ ] **Step 7: Read the screenshots**

Open `shots/wide-light.png` and `shots/narrow-dark.png`. Confirm F2b is legible at 390px and that the palette holds in dark. Fix and re-run before proceeding.

- [ ] **Step 8: Checkpoint**
