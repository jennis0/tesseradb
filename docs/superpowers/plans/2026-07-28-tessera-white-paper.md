# Tessera White Paper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and publish a self-contained interactive HTML white paper explaining how Tessera works, for a public technical reader with no database-internals background.

**Architecture:** Prose and figures are authored as ordered HTML fragments under `docs/whitepaper/src/`, concatenated by a build script into a single `docs/tessera-white-paper.html` that the Artifact tool publishes. A validator enforces the Artifact CSP and structural rules; a Playwright harness renders the built page at two widths in both themes and fails on console errors or horizontal body scroll. Every prose task is gated on a cited fact sheet produced in Task 1, so no claim about a third-party system or a measured number is written from memory.

**Tech Stack:** Hand-authored HTML/CSS/SVG plus vanilla ES2020 (no framework, no charting library — the Artifact CSP blocks all external hosts). Python 3.10 for the build script, validator and render harness only; these are documentation tooling and are not part of Tessera.

## Global Constraints

Every task's requirements implicitly include this section.

- **Source of truth for claims:** `docs/design/*.md` for design and prior art, `probes/results.md` and `probes/phase0-memo.md` for measurements. `docs/design/` is skipped by default file-search tooling — pass the path explicitly.
- **Precedence:** the architecture design is the specification; where the contracts spec and the system architecture differ, contracts spec §0.3 governs.
- **No claim from memory.** Every competitor claim and every performance number must trace to a fact sheet entry from Task 1, which cites document and section.
- **Preserve demonstrated vs marketed scale.** deepscatter's billion-point artefact is a *static* star catalogue; Nomic's marketing says "billions" while its largest published map is 11M. Collapsing these distinctions is a factual error.
- **Synthetic-policy caveat stated plainly**, not buried: Phase 0 ran synthetic policies over a real 2.42M-paper arXiv corpus, scaled to 10⁹.
- **Invariants must match `docs/design/architecture.md` §4 in substance.** I2, I3, I7, I9, I10, I12 and the three retirement rules (lifecycle §3) all appear in the paper.
- **British spelling** throughout (authorisation, visualisation, colour).
- **No external resources.** No CDN scripts, external stylesheets, remote fonts, remote images, `fetch`/XHR/WebSocket. Assets inlined or embedded as `data:` URIs. Prose may contain `<a href="https://...">` citation links — those are navigation, not resource loads, and are permitted.
- **No document-level tags** in source fragments: the Artifact tool supplies `<!doctype>`, `<html>`, `<head>` and `<body>`. Fragments contain page content only, with `<style>` and `<script>` inline in that content.
- **Theme-aware:** correct under `@media (prefers-color-scheme: dark)` *and* under explicit `:root[data-theme="dark"]` / `:root[data-theme="light"]` overrides, with the explicit override winning in both directions.
- **Target length:** 8,000–10,000 words of prose plus sixteen figures.
- **The repository is not under git.** There is nothing to commit to. Wherever this plan says "checkpoint", it means: run the build, run the validator, run the render harness, and confirm all three pass before moving on. Do not run `git init`.

---

## File Structure

| Path | Responsibility |
|---|---|
| `docs/whitepaper/build.py` | Concatenates `src/` fragments in order into the published file |
| `docs/whitepaper/validate.py` | Static checks: CSP compliance, no document tags, figure IDs present, theme rules present |
| `docs/whitepaper/render_check.py` | Playwright harness: renders built page at 1280px and 390px, light and dark; fails on console errors or horizontal body scroll; writes screenshots |
| `docs/whitepaper/src/00-style.html` | The entire design system: tokens, type scale, layout, figure chrome, theme overrides |
| `docs/whitepaper/src/01-header.html` | `<title>`, title block, standfirst, the status note |
| `docs/whitepaper/src/1x-part-*.html` | One fragment per part (six), prose plus its figures |
| `docs/whitepaper/src/90-footer.html` | Citations block, closing note |
| `docs/whitepaper/src/lib/figure.js` | Shared figure runtime: theme observer, SVG helpers, number formatting |
| `docs/whitepaper/facts/part-*.md` | Cited fact sheets from Task 1 (claim → document → section) |
| `docs/tessera-white-paper.html` | Build output. The file passed to the Artifact tool. Never edited by hand |

Fragments are concatenated in lexical filename order, which is why filenames are numbered.

---

### Task 1: Fact extraction

Produces the cited fact sheets every later prose task depends on. Dispatch all six subagents in a single message so they run concurrently.

**Files:**
- Create: `docs/whitepaper/facts/part-1.md` … `part-6.md`

**Interfaces:**
- Produces: six markdown fact sheets. Each entry is one line: `- CLAIM — source: <file> §<section>` with a verbatim supporting quote where the claim is a number or a third-party assertion.

- [ ] **Step 1: Create the facts directory**

```bash
mkdir -p /home/joe/code/tessera/docs/whitepaper/facts
```

- [ ] **Step 2: Dispatch six fact-extraction subagents concurrently**

Each subagent gets this prompt shape, with `<PART>`, `<DOCS>` and `<TOPICS>` filled in from the table below:

> Read `<DOCS>` (pass paths explicitly — `docs/design/` is skipped by default search tooling). Extract every fact needed to write about `<TOPICS>`. Return a markdown fact sheet. Each entry is one line: `- CLAIM — source: <file> §<section>`, plus a verbatim quote for any number or any assertion about a third-party system. Preserve the distinction between demonstrated and marketed capability wherever the source draws it. Do not write prose, do not summarise into narrative, do not include anything you cannot cite. If a claim you expect to find is absent from the sources, say so explicitly rather than supplying it.

| Part | DOCS | TOPICS |
|---|---|---|
| 1 | `docs/evidence/prior-art/prior-art-synthesis.md`, `docs/evidence/prior-art/prior-art-2-visual-analytics.md`, `docs/evidence/prior-art/prior-art-3-databases.md`, `docs/evidence/prior-art/prior-art-4-authorization-disclosure.md` | Aggregate leakage in systems with per-document security; demonstrated vs marketed scale for every surveyed visualisation system; row-level access control coverage across the category; PostgreSQL RLS and DuckDB measurements |
| 2 | `docs/design/architecture.md` §5, §10.3, §10.4, `probes/results.md` §4, §5, §6, `probes/optimisations.md` | Entity space vs row space and the permutation; Morton ranking and tile-to-range contiguity; Roaring container structure; the O(containers touched) cost model |
| 3 | `docs/design/architecture.md` §6, §2.6, §4, `docs/design/contracts.md` (plugin ABI), `probes/results.md` §3, §5, `probes/optimisations.md` | The authorisation plugin boundary; DNF terms, the term index, postings and union; mask construction; signature-sorted allocation and I9 permanence; the end-to-end request path |
| 4 | `docs/design/architecture.md` §7, §8, §12, §4 | Level of detail, priority nesting, direct evaluation, I7; label gating and the containment frontier, I3/I12; the two-mask model; compartmented partitions and required-set gating |
| 5 | `docs/design/concurrency-lifecycle.md`, `docs/design/architecture.md` §11 | WAL and the ack contract; buffer, watermark and overlay; segments, merging, compaction; generations and pins; the three retirement rules and why conflating them is fail-open |
| 6 | `probes/results.md`, `probes/phase0-memo.md`, `docs/design/architecture.md` §13, §16, Appendix C | The 10⁹ measurements with their conditions; the 8.8 s permutation cost; what Phase 0 did and did not establish; open questions; the leak register C1–C16 |

- [ ] **Step 3: Verify each fact sheet is citable**

For each of the six files, confirm every line carries a `source:` reference. Spot-check three numeric claims per sheet against the named section by reading it directly. A fact sheet with uncited lines is rejected — send it back to the subagent rather than repairing it yourself.

- [ ] **Step 4: Checkpoint**

Confirm six files exist and are non-empty. Nothing to build yet.

---

### Task 2: Scaffolding — build, validate, render

Independent of Task 1; may proceed while the fact subagents run. Delivers a page that builds, validates and renders correctly with placeholder content, so every later task inherits a working verification loop.

**Files:**
- Create: `docs/whitepaper/build.py`, `docs/whitepaper/validate.py`, `docs/whitepaper/render_check.py`
- Create: `docs/whitepaper/src/00-style.html`, `docs/whitepaper/src/01-header.html`, `docs/whitepaper/src/90-footer.html`, `docs/whitepaper/src/lib/figure.js`

**Interfaces:**
- Produces: `python3 docs/whitepaper/build.py` → writes `docs/tessera-white-paper.html`. `python3 docs/whitepaper/validate.py` → exit 0 on pass, prints one line per failure. `python3 docs/whitepaper/render_check.py` → exit 0 on pass, writes screenshots to the scratchpad.
- Produces: CSS custom property contract consumed by every later figure — `--bg`, `--fg`, `--muted`, `--rule`, `--accent`, `--surface`, and categorical series tokens `--cat-1` … `--cat-6`. Figures use only these tokens; no literal colours in figure code.
- Produces: `docs/whitepaper/src/lib/figure.js` exporting `onThemeChange(fn)`, `svg(tag, attrs)`, and `fmt(n)`.

- [ ] **Step 1: Load the design skills**

Invoke the `dataviz` skill and the `artifact-design` skill before writing any CSS. Take the categorical palette values for `--cat-1` … `--cat-6` from the `dataviz` skill's `references/palette.md`, and run its palette validator against the light and dark values. Do not invent a palette.

- [ ] **Step 2: Set up the render environment**

Browser binaries are already cached under `~/.cache/ms-playwright`; only the driver is missing.

```bash
python3 -m venv /tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/pip install playwright
```

Do not run `playwright install` unless the harness reports a missing browser — the binaries are present.

- [ ] **Step 3: Write the build script**

```python
#!/usr/bin/env python3
"""Concatenate whitepaper source fragments into the published file."""
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC = ROOT / "src"
OUT = ROOT.parent / "tessera-white-paper.html"


def main() -> None:
    fragments = sorted(p for p in SRC.glob("*.html"))
    if not fragments:
        raise SystemExit("no fragments found in %s" % SRC)
    parts = []
    for frag in fragments:
        parts.append("<!-- %s -->" % frag.name)
        parts.append(frag.read_text(encoding="utf-8").rstrip())
    OUT.write_text("\n".join(parts) + "\n", encoding="utf-8")
    print("wrote %s (%d fragments, %d bytes)" % (OUT, len(fragments), OUT.stat().st_size))


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Write the validator**

```python
#!/usr/bin/env python3
"""Static checks on the built whitepaper: CSP, structure, theming."""
import re
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "tessera-white-paper.html"
FIGURE_IDS = [
    "fig-f1", "fig-f2", "fig-f2b", "fig-f3", "fig-f4", "fig-f5",
    "fig-f6", "fig-f7", "fig-f8", "fig-f9", "fig-f10", "fig-f11",
    "fig-f12", "fig-f13", "fig-f14", "fig-f15", "fig-f16",
]


def main() -> int:
    html = OUT.read_text(encoding="utf-8")
    fails = []

    for tag in ("<!doctype", "<html", "<head", "<body"):
        if tag in html.lower():
            fails.append("document-level tag present: %s" % tag)

    # Resource loads must never reference an external host. Anchor hrefs may.
    for m in re.finditer(r'(?:\bsrc\s*=\s*|\burl\(\s*|@import\s+)["\']?(https?:)?//', html, re.I):
        fails.append("external resource load at offset %d" % m.start())
    if re.search(r"<link\b", html, re.I):
        fails.append("<link> element present")
    for banned in ("fetch(", "XMLHttpRequest", "WebSocket"):
        if banned in html:
            fails.append("network API present: %s" % banned)

    if "<title>" not in html:
        fails.append("missing <title>")

    for fid in FIGURE_IDS:
        if 'id="%s"' % fid not in html:
            fails.append("missing figure: %s" % fid)

    for rule in ("prefers-color-scheme: dark", '[data-theme="dark"]', '[data-theme="light"]'):
        if rule not in html:
            fails.append("missing theme rule: %s" % rule)

    for f in fails:
        print("FAIL: %s" % f)
    print("PASS" if not fails else "%d failure(s)" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
```

Note: `FIGURE_IDS` lists seventeen entries because F2b is an additional figure alongside F1–F16 — the paper has sixteen numbered figures using seventeen slots. Every ID must exist by Task 8; until then the validator is expected to report the not-yet-written ones, which is the failing-test signal each prose task clears.

- [ ] **Step 5: Write the render harness**

```python
#!/usr/bin/env python3
"""Render the built whitepaper and fail on console errors or body overflow."""
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

OUT = Path(__file__).resolve().parent.parent / "tessera-white-paper.html"
SHOTS = Path("/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/shots")
WRAPPER = """<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"></head><body>
%s
</body></html>"""


def main() -> int:
    SHOTS.mkdir(parents=True, exist_ok=True)
    page_html = WRAPPER % OUT.read_text(encoding="utf-8")
    wrapped = SHOTS / "wrapped.html"
    wrapped.write_text(page_html, encoding="utf-8")

    fails = []
    with sync_playwright() as p:
        browser = p.chromium.launch()
        for width, label in ((1280, "wide"), (390, "narrow")):
            for scheme in ("light", "dark"):
                ctx = browser.new_context(
                    viewport={"width": width, "height": 900}, color_scheme=scheme
                )
                page = ctx.new_page()
                errors = []
                page.on("console", lambda m: errors.append(m.text) if m.type == "error" else None)
                page.on("pageerror", lambda e: errors.append(str(e)))
                page.goto(wrapped.as_uri())
                page.wait_for_timeout(1200)
                overflow = page.evaluate(
                    "() => document.scrollingElement.scrollWidth - document.scrollingElement.clientWidth"
                )
                if overflow > 1:
                    fails.append("%s/%s: body overflows by %dpx" % (label, scheme, overflow))
                for e in errors:
                    fails.append("%s/%s: console error: %s" % (label, scheme, e))
                page.screenshot(path=str(SHOTS / ("%s-%s.png" % (label, scheme))), full_page=True)
                ctx.close()
        browser.close()

    for f in fails:
        print("FAIL: %s" % f)
    print("PASS" if not fails else "%d failure(s)" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 6: Run the build to verify it fails correctly**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/build.py
```

Expected: exits non-zero with "no fragments found in .../src" — there are no source files yet. This confirms the harness detects an empty state rather than silently writing an empty page. Do not run the validator yet; with no build output it would raise `FileNotFoundError` rather than report failures.

- [ ] **Step 7: Write the design system**

Create `docs/whitepaper/src/00-style.html` containing a single `<style>` block that defines:

- `:root` light tokens and a `@media (prefers-color-scheme: dark)` dark block, both re-declared under `:root[data-theme="light"]` and `:root[data-theme="dark"]` so the explicit override wins in both directions.
- Tokens: `--bg`, `--fg`, `--muted`, `--rule`, `--accent`, `--surface`, `--cat-1` … `--cat-6`, taken from the `dataviz` palette.
- A measure-limited prose column (`max-width: 34rem`) with figures allowed to break out to a wider band.
- `figure { }` chrome: caption styling, a `.fig-scroll { overflow-x: auto }` wrapper class for wide figures, and a `.fig-controls` row for interactive widgets.
- A type scale, and `img, svg { max-width: 100% }`.

- [ ] **Step 8: Write the header and footer fragments**

`01-header.html` contains `<title>Tessera — a permission-masked point service</title>`, the title block, a one-paragraph standfirst, and the status note: the architecture as specified, Phase 0 measured on a synthetic-policy corpus scaled to 10⁹, Phase 1 in progress. `90-footer.html` contains an empty citations `<section>` to be filled in Task 10.

- [ ] **Step 9: Write the figure runtime**

Create `docs/whitepaper/src/lib/figure.js`. It is not a `.html` fragment, so the build script will not pick it up — inline its contents inside a `<script>` block at the end of `00-style.html`. It exports, on a global `TF` object: `onThemeChange(fn)` (a `matchMedia` listener plus a `MutationObserver` on `documentElement`'s `data-theme`), `svg(tag, attrs)` (namespaced element factory), and `fmt(n)` (thousands separators, and `µs`/`ms` suffixing).

- [ ] **Step 10: Build and verify the scaffolding passes its own checks**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py
python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```

Expected: build succeeds; validate reports exactly seventeen "missing figure" failures and nothing else; render_check passes with no console errors and no overflow. Read one screenshot to confirm the type and theme look right.

- [ ] **Step 11: Checkpoint**

The seventeen missing-figure failures are the expected failing state. Every later task removes some of them.

---

### Task 3: Part I — The empty quadrant

**Files:**
- Create: `docs/whitepaper/src/10-part-1.html`
- Read: `docs/whitepaper/facts/part-1.md`

**Interfaces:**
- Consumes: `TF.onThemeChange`, `TF.svg`, `TF.fmt` from Task 2; CSS tokens `--cat-1` … `--cat-6`.
- Produces: figure IDs `fig-f1`, `fig-f2`, `fig-f2b`.

- [ ] **Step 1: Verify the failing state**

```bash
cd /home/joe/code/tessera && python3 docs/whitepaper/validate.py | grep -E "f1|f2b"
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
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: the three Part I figure failures are gone; fourteen remain; render passes.

- [ ] **Step 7: Read the screenshots**

Open `shots/wide-light.png` and `shots/narrow-dark.png`. Confirm F2b is legible at 390px and that the palette holds in dark. Fix and re-run before proceeding.

- [ ] **Step 8: Checkpoint**

---

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

---

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

---

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

---

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

---

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

---

### Task 9: Independent accuracy and invariant review

The paper makes public claims about third-party systems and about measured performance. This task exists because misstating I2 or I7 publicly is the worst available failure mode.

**Files:**
- Modify: whichever `docs/whitepaper/src/*.html` fragments the review identifies

**Interfaces:**
- Consumes: the built `docs/tessera-white-paper.html`.
- Produces: a review report, and fixes applied to the source fragments.

- [ ] **Step 1: Dispatch three reviewers concurrently**

Give each the built HTML path and no stake in the paper being right.

Reviewer A — invariants: "Read `docs/design/architecture.md` §4 and `docs/design/concurrency-lifecycle.md` §3. Then read the built paper. Report every place where a statement about I2, I3, I7, I9, I10, I12 or the three retirement rules differs in substance from the source, and every place the paper implies a guarantee the design does not make. Quote both the paper and the source."

Reviewer B — third-party claims: "Read `docs/evidence/prior-art/prior-art-*.md`. For every claim the paper makes about a system other than Tessera, verify it against those documents and report any claim that is unsupported, overstated, or collapses the distinction between demonstrated and marketed capability. Report claims with no traceable source as failures."

Reviewer C — measurements: "Read `probes/results.md` and `probes/phase0-memo.md`. Verify every number in the paper against them, including units and measurement conditions. Report any number that is wrong, rounded misleadingly, quoted without its conditions, or presented as more general than the source supports. Confirm the synthetic-policy caveat is stated plainly rather than buried."

- [ ] **Step 2: Triage the findings yourself**

Do not accept reviewer summaries at face value. For each finding, read the cited source section directly and decide. Invariant-bearing decisions stay with you, not the reviewer.

- [ ] **Step 3: Apply the fixes to the source fragments, not the build output**

- [ ] **Step 4: Rebuild and re-verify**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```

- [ ] **Step 5: Checkpoint**

---

### Task 10: Polish and publish

**Files:**
- Modify: `docs/whitepaper/src/00-style.html`, `docs/whitepaper/src/90-footer.html`

**Interfaces:**
- Produces: a published Artifact URL.

- [ ] **Step 1: Fill in the citations section**

`90-footer.html` gets the citation list assembled from the six fact sheets, each linking to its primary source where the prior-art documents give one.

- [ ] **Step 2: Word count check**

```bash
cd /home/joe/code/tessera && python3 -c "
import re,pathlib
h=pathlib.Path('docs/tessera-white-paper.html').read_text()
h=re.sub(r'<(script|style)[^>]*>.*?</\1>','',h,flags=re.S)
print(len(re.sub(r'<[^>]+>',' ',h).split()))"
```
Expected: 8,000–10,000. If materially under, the prose is thin — say so rather than padding.

- [ ] **Step 3: Accessibility and motion pass**

Confirm every interactive figure is keyboard-operable and focus-visible; every figure has a caption that states its point in prose, so the paper survives with images unavailable; `prefers-reduced-motion` is honoured by F13.

- [ ] **Step 4: Final full verification**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: both print `PASS`. Read all four screenshots.

- [ ] **Step 5: Publish**

Call the Artifact tool with `file_path: docs/tessera-white-paper.html`, a `description` of one sentence, and `favicon: "🧩"`. Keep the favicon stable across any later redeploy.

- [ ] **Step 6: Report the URL and state plainly what was verified and what was not**

---

## Notes on this plan

- **No git.** The repository is not under version control, so there are no commit steps. Each checkpoint is build + validate + render, all three passing.
- **Task 1 and Task 2 are independent** and may run concurrently; every task from 3 onward is strictly ordered, because each removes specific validator failures.
- **The validator's missing-figure failures are the failing-test signal.** Each prose task begins by confirming its own failures are present and ends by confirming they are gone. This is the closest honest analogue to a red-green cycle for a document.
- **Screenshots are read, not just generated.** A render harness that passes while the page looks wrong is the main way this plan could produce something bad.
