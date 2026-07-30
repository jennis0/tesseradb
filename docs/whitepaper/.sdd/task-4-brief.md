### Task 4: Part II — The shape of the data

**Files:**
- Create: `docs/whitepaper/src/20-part-2.html`
- Read: `docs/whitepaper/facts/part-2.md`

**Interfaces:**
- Produces: figure IDs `fig-f3`, `fig-f4`, `fig-f5`.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/validate.py | grep -E "f3|f4|f5"
```
Expected: three missing-figure failures.

- [ ] **Step 2: Write the three chapters**

Chapter 4 — two coordinate systems: entity space carries permissions, row space carries geometry, and an explicit permutation relates them. Chapter 5 — Morton ranking: why a square on screen becomes a contiguous range of rows. Chapter 6 — Roaring and the cost model: cost is containers touched, not cardinality, and therefore cost scales with screen area rather than corpus size. State plainly that this is the scale answer and that the rest of the design is written against it. Roughly 1,800–2,200 words.

- [ ] **Step 3: Build F3 — two coordinate systems** *(static)*

Two parallel rails, entity space and row space, with permutation arrows between them. Trace one highlighted item through both so the reader sees that an item has two different indices for two different reasons.

- [ ] **Step 4: Build F4 — the Morton explorer** *(interactive, load-bearing)*

A grid with the Z-order curve drawn through it. A draggable, resizable viewport rectangle. Below, a bar showing the decomposition into contiguous row ranges, updating live as the rectangle moves. A toggle switches the underlying layout to row-major, and the range count visibly explodes — display the count for both so the comparison is a number, not an impression. Pointer and keyboard controls both move and resize the rectangle.

- [ ] **Step 5: Build F5 — Roaring anatomy** *(interactive, load-bearing)*

A bitmap rendered as a strip of 2¹⁶-chunk containers, each labelled by type: array, bitmap, or run. Two masks, and an intersect control. A counter shows containers touched against total cardinality, so the reader sees the gap directly. This is the figure the scale claim rests on — its numbers must match the cost model in `facts/part-2.md`.

- [ ] **Step 6: Build, validate, render**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: eleven missing-figure failures remain; render passes.

- [ ] **Step 7: Read the screenshots**

Confirm F4's drag interaction has visible affordances and that the range-count comparison is readable at 390px. Wide figures must be inside `.fig-scroll`, not overflowing the body.

- [ ] **Step 8: Checkpoint**
