# Task 2 report — scaffolding: build, validate, render, design system

Status: **DONE**. Build passes, validator reports exactly the seventeen expected
"missing figure" failures and nothing else, render harness passes in all four
viewport/theme combinations.

## Files created

| Path | What it is |
|---|---|
| `docs/whitepaper/build.py` | Concatenates `src/*.html` (sorted) into `docs/tessera-white-paper.html`. Transcribed verbatim from the brief. |
| `docs/whitepaper/validate.py` | Static checks: document-level tags, external resource loads, `<link>`, network APIs, `<title>`, the seventeen figure IDs, the three theme rules. Transcribed verbatim. |
| `docs/whitepaper/render_check.py` | Playwright harness: wraps the fragment in a document, loads it at 1280/390 × light/dark, fails on console errors or body horizontal overflow, writes four full-page screenshots. Transcribed verbatim. |
| `docs/whitepaper/src/00-style.html` | The design system: one `<style>` block (tokens, layout, type, figure chrome, tables, focus, motion, responsive) plus one `<script>` block holding an inline copy of the figure runtime. |
| `docs/whitepaper/src/01-header.html` | `<title>`, opens `<article class="wp">`, eyebrow, `<h1>`, standfirst, status note. |
| `docs/whitepaper/src/90-footer.html` | Empty `<section id="citations">` for Task 10, closes `</article>`. |
| `docs/whitepaper/src/lib/figure.js` | Reference copy of the figure runtime. Not globbed by the build; kept byte-identical to the inline copy in `00-style.html`. |

Build output: `docs/tessera-white-paper.html` (3 fragments, 23,883 bytes).

## Palette provenance and validator result

The categorical slots are the **first six slots of the `dataviz` skill's
reference categorical theme**, `references/palette.md`, taken verbatim in both
modes. Nothing was invented or re-stepped.

The surfaces differ from the skill's defaults (this paper's neutrals are
cool-shifted toward the blue accent rather than the skill's warm-neutral
`#fcfcfb` / `#1a1a19`), so the validator was re-run against **this paper's**
surfaces, as the skill requires.

```
$ node scripts/validate_palette.js "#2a78d6,#eb6834,#1baf7a,#eda100,#e87ba4,#008300" --mode light --surface "#f8f9fa"

Palette (light, surface #f8f9fa, categorical): 6 slots
  [PASS] Lightness band         all 6 inside L 0.43–0.77
  [PASS] Chroma floor           all 6 >= 0.1
  [PASS] CVD separation         worst adjacent #eda100↔#1baf7a ΔE 9.1 (protan) · tritan 5.8
  [PASS] Normal-vision floor    worst adjacent #e87ba4↔#eda100 ΔE 19.6 (normal)
  [WARN] Contrast vs surface    below 3:1 — relief required (visible labels or table view):
                                [["#1baf7a",2.67],["#eda100",2.05],["#e87ba4",2.55]]
  → ALL CHECKS PASS

$ node scripts/validate_palette.js "#3987e5,#d95926,#199e70,#c98500,#d55181,#008300" --mode dark --surface "#17181a"

Palette (dark, surface #17181a, categorical): 6 slots
  [PASS] Lightness band         all 6 inside L 0.48–0.67
  [PASS] Chroma floor           all 6 >= 0.1
  [PASS] CVD separation         worst adjacent #c98500↔#199e70 ΔE 8.4 (protan) · tritan 8.7
  [PASS] Normal-vision floor    worst adjacent #d55181↔#c98500 ΔE 19.3 (normal)
  [PASS] Contrast vs surface    all 6 >= 3:1
  → ALL CHECKS PASS
```

**One standing obligation for figure authors.** The light-mode WARN is not
dismissable. `--cat-3` (2.67:1), `--cat-4` (2.05:1) and `--cat-5` (2.55:1) sit
below 3:1 against the light surface. Any figure that uses those slots must
carry the **relief**: visible direct labels on the marks, or an accompanying
table. Do not re-step the hexes to dodge it — the palette is documented and the
ordering is the CVD-safety mechanism.

**Two further rules inherited from the skill.** Assign slots in fixed order and
never cycle; a seventh series folds into "Other", facets, or small multiples.
And for all-pairs chart forms (scatter, bubble, choropleth, small multiples)
only the **first three slots** validate — past three, fold or facet.

## Token list

All tokens are declared in `00-style.html` with an inline comment. Later figures
consume these and **must not contain a literal colour**; if a figure needs a
colour not named here, add a token rather than hardcoding.

### Ground and ink (themed)

| Token | Purpose | Light | Dark |
|---|---|---|---|
| `--bg` | page plane behind everything | `#f7f8fa` | `#101214` |
| `--surface` | raised plane: figure frames, cards, callouts | `#ffffff` | `#17181a` |
| `--surface-2` | inset plane: code blocks, table zebra, wells | `#eef0f4` | `#1f2124` |
| `--fg` | primary ink: body text, headings | `#101317` | `#f2f3f5` |
| `--muted` | secondary ink: captions, labels, axis text | `#565b63` | `#a3a8b0` |
| `--rule` | hairline: borders, dividers, table rules | `#dcdfe5` | `#2b2e33` |

### Interactive (themed)

| Token | Purpose | Light | Dark |
|---|---|---|---|
| `--accent` | links, active controls, emphasis | `#1c5cab` | `#6da7ec` |
| `--accent-weak` | accent wash: selection, hover fill | `rgba(28,92,171,.10)` | `rgba(109,167,236,.16)` |
| `--focus` | keyboard focus ring **only** | `#2a78d6` | `#6da7ec` |

### Chart chrome (themed, recessive — never used for data)

| Token | Purpose | Light | Dark |
|---|---|---|---|
| `--grid` | chart gridlines | `#e7e9ee` | `#24262a` |
| `--axis` | baselines, axis lines, tick marks | `#b9bec7` | `#3a3d43` |
| `--annotation` | callout leader lines, annotation ink | `#565b63` | `#a3a8b0` |

### Categorical series (themed; fixed order, never cycled)

| Token | Hue | Light | Dark |
|---|---|---|---|
| `--cat-1` | blue | `#2a78d6` | `#3987e5` |
| `--cat-2` | orange | `#eb6834` | `#d95926` |
| `--cat-3` | aqua *(relief required, light)* | `#1baf7a` | `#199e70` |
| `--cat-4` | yellow *(relief required, light)* | `#eda100` | `#c98500` |
| `--cat-5` | magenta *(relief required, light)* | `#e87ba4` | `#d55181` |
| `--cat-6` | green | `#008300` | `#008300` |

### Sequential ramp (themed) — magnitude: densities, heatmaps, tile counts

`--seq-1` … `--seq-6`, single hue (blue), light→dark on the light ground and
dark→light on the dark ground so "near zero" always recedes toward the surface.
Light `#cde2fb #9ec5f4 #6da7ec #3987e5 #256abf #184f95`; dark is that order
reversed.

### Diverging pair (themed) — polarity, e.g. allowed vs denied

`--div-neg` (blue), `--div-mid` (neutral grey, never a hue), `--div-pos` (red).

### Masking — the paper's one subject-specific pair

| Token | Purpose |
|---|---|
| `--masked-in` | a point inside `M_auth`; aliases `--cat-1` |
| `--masked-out` | a point outside it — a low-alpha ink, deliberately *absence* rather than a competing colour, so a masked map never reads as a two-series chart |

### Status (mode-invariant, reserved)

`--status-good` `#0ca30c`, `--status-warning` `#fab219`, `--status-serious`
`#ec835a`, `--status-critical` `#d03b3b`. These mean **state**, never "series 7",
and always ship with a label — `--status-warning` is deliberately sub-3:1 on the
light ground, so colour must never carry the meaning alone.

### Typefaces

| Token | Stack | Used for |
|---|---|---|
| `--font-prose` | Charter / Iowan Old Style / Georgia / serif | running prose, headline-adjacent body |
| `--font-ui` | system-ui / -apple-system / Segoe UI / sans | **all headings, captions, tables, and everything inside figures** |
| `--font-mono` | ui-monospace / SF Mono / Menlo / Consolas | code, identifiers, byte layouts |

No webfonts: the artifact CSP blocks font hosts and a silent fallback is worse
than a chosen stack. The serif/sans split does the register work — a serif for
the argument, a sans for the machinery — and matches the `dataviz` rule that
charts stay in the system sans.

### Type scale (1.22 ratio, anchored at a 17px body)

`--fs-xs` .75rem (legend keys, tick labels) · `--fs-sm` .8125rem (captions,
table cells, control labels) · `--fs-base` 1.0625rem (prose) · `--fs-lg`
1.1875rem (standfirst) · `--fs-xl` 1.375rem (h3) · `--fs-2xl` 1.6875rem (h2) ·
`--fs-3xl` clamp(2rem, 1.4rem + 2.4vw, 2.875rem) (paper title).

### Rhythm and geometry

`--measure` 34rem (prose column, ~65 characters) · `--band` 6rem (breakout
allowance per side) · `--gutter` 1.25rem (minimum page margin) · `--flow`
1.05rem (default sibling gap) · `--radius` 4px · `--hair` 1px · `--wp-cols`
(the shared grid template).

## What a later figure author needs to know

### 1. Where your markup goes

`01-header.html` opens `<article class="wp">`; `90-footer.html` closes it. Every
prose fragment sits between them and should be a top-level `<section>`. The
build globs `src/*.html` and concatenates in **sorted filename order**, so
number your fragments between `01-` and `90-`.

### 2. The layout grid, and how to break out of the measure

`.wp` and every `.wp > section` declare the same five-track grid
(`--wp-cols`): `full | wide | text | wide | full`. Direct children land in
`text` (the 34rem measure). To go wider:

- `<figure>` — **breaks out to the `wide` band automatically**. No class needed.
- `.wide` on any block — same band.
- `.full` on any block — edge to edge.

Sections re-declare the grid, so a `<figure>` nested inside a `<section>` still
breaks out. Every grid child carries `min-width: 0`, so nothing forces the page
sideways.

Measured widths: at 1280px, prose 544px / figure 736px. At 700px, 544 / 608.
At 390px both are 342px. Body horizontal overflow is 0 at all three.

`--band` steps down to 2rem below 48rem and to 0 below 40rem — this is
load-bearing. Without it the band tracks and the measure track compete for the
shortfall and the prose column collapses to about half the screen (this was a
real bug found and fixed during this task; do not "simplify" those media
queries away).

### 3. Figure chrome

```html
<figure id="fig-f7">
  <div class="fig-controls">…buttons, selects, range inputs…</div>
  <div class="fig-frame fig-scroll">…graphic…</div>
  <div class="fig-legend">
    <span><i class="swatch" style="background: var(--cat-1)"></i>Label</span>
  </div>
  <figcaption><span class="fig-num">Figure 7.</span> Caption text.</figcaption>
</figure>
```

- `.fig-frame` — the surface plane with a hairline and radius.
- `.fig-scroll` — `overflow-x: auto` container. **Caveat:** `svg`, `img` and
  `canvas` shrink to fit by default, so a `.fig-scroll` around a bare `<svg>`
  never actually scrolls; the graphic just gets smaller. That is usually right.
  A graphic that must keep its natural size and scroll instead (dense
  timelines, Morton-order strips, wide tables) wraps its content in
  `<div class="fig-fixed">` inside the `.fig-scroll`. Verified: with
  `.fig-fixed` the box scrolls at 1280/700/390 and the page still does not.
- `.fig-controls` — flex row; `button[aria-pressed="true"]` gets the active
  accent treatment for free; `input[type=range]` inherits `accent-color`.
- `.fig-legend` + `.swatch` — a legend is required for two or more series.
- `figure text { fill: var(--muted) }` is already set: SVG text wears text
  tokens, never a series colour.
- The `id="fig-fN"` goes on the `<figure>`. That is what the validator looks
  for, as the literal string `id="fig-fN"` — use double quotes.

### 4. The figure runtime (global `TF`)

Available on every page; no imports, no modules, no network. Verified in a
live browser.

| Call | Behaviour (verified output) |
|---|---|
| `TF.theme()` | `"light"` \| `"dark"` — resolves the data-theme stamp over the OS preference, mirroring the CSS cascade |
| `TF.onThemeChange(fn)` | `fn("dark")` fires on both an OS change and a `data-theme` stamp (`matchMedia` + `MutationObserver`). Returns an unsubscribe function. **Only needed for canvas figures or anything that reads token values in JS — a figure styled purely through `var(--…)` re-themes for free.** |
| `TF.svg(tag, attrs)` | Namespaced element factory. `null`/`undefined` attribute values are skipped, so optional attributes need no branching. `text:` sets `textContent`; `children:` appends an array. |
| `TF.fmt(n, decimals)` | `1048576 → "1,048,576"`, `(2.4157, 2) → "2.42"`, `-9876543 → "-9,876,543"`, `null → "—"` |
| `TF.fmt.dur(us)` | Input is **microseconds** (the unit `probes/results.md` quotes). `430 → "430 µs"`, `21500 → "21.5 ms"`, `4200000 → "4.20 s"`. Non-breaking space before the unit. |
| `TF.fmt.bytes(b)` | `1536000 → "1.46 MiB"` |
| `TF.token(name, el)` | Reads a custom property off `:root` (or `el`). Verified: `--cat-1` returns `#2a78d6` light, `#3987e5` after a dark stamp. **The only correct way for a canvas figure to get a colour.** |
| `TF.reducedMotion()` | Animated figures must check this and present their end state immediately. |
| `TF.NS` | The SVG namespace string. |

`src/lib/figure.js` is the readable reference copy; `00-style.html` holds an
inline copy inside a `<script>` block. **If you change one, change both** — a
check that they are byte-identical is a one-liner:

```bash
python3 -c "
import re,pathlib
js=pathlib.Path('docs/whitepaper/src/lib/figure.js').read_text().rstrip()
inl=re.search(r'<script>\n(.*)\n</script>',pathlib.Path('docs/whitepaper/src/00-style.html').read_text(),re.S).group(1)
print(js==inl)"
```

### 5. Things that will trip the validator

- Any occurrence of the literal `<html`, `<head`, `<body` or `<!doctype` —
  **including inside a comment or a JS string**. This bit during this task: a
  code comment reading "a `data-theme` stamp on `<html>`" failed the build. Say
  "the root element".
- `fetch(`, `XMLHttpRequest`, `WebSocket` anywhere, including in prose about
  networking. Reword.
- `src="//…"`, `url(//…)`, `@import //…`, or any `<link>`. `<a href="https://…">`
  citation links are fine — those are navigation, not resource loads.
- Data must be inlined as a JS literal. There is no way to load an external
  file.

## Verification commands and exact output

```
$ python3 docs/whitepaper/build.py
wrote /home/joe/code/tessera/docs/tessera-white-paper.html (3 fragments, 23883 bytes)

$ python3 docs/whitepaper/validate.py ; echo exit=$?
FAIL: missing figure: fig-f1
FAIL: missing figure: fig-f2
FAIL: missing figure: fig-f2b
FAIL: missing figure: fig-f3
FAIL: missing figure: fig-f4
FAIL: missing figure: fig-f5
FAIL: missing figure: fig-f6
FAIL: missing figure: fig-f7
FAIL: missing figure: fig-f8
FAIL: missing figure: fig-f9
FAIL: missing figure: fig-f10
FAIL: missing figure: fig-f11
FAIL: missing figure: fig-f12
FAIL: missing figure: fig-f13
FAIL: missing figure: fig-f14
FAIL: missing figure: fig-f15
FAIL: missing figure: fig-f16
17 failure(s)
exit=1

$ <venv>/bin/python docs/whitepaper/render_check.py ; echo exit=$?
PASS
exit=0
```

Seventeen failures, all of them "missing figure", nothing else. This is the
intended failing state — each later task clears some of them. No placeholder
elements carrying those IDs were added.

Screenshots written to
`…/scratchpad/shots/{wide,narrow}-{light,dark}.png`. Both wide-light and
narrow-dark were read back and confirmed: measure-limited serif prose, centred
column, sans headings, accent-ruled status note, correct dark ground.

## Environment note

The brief said the browser binaries were already cached and only the driver was
missing. That turned out to be half true: `~/.cache/ms-playwright` held
chromium **1208**, but pip installed playwright **1.61.0**, whose driver
requires chromium **1228**. The harness reported the missing browser, so per the
brief's own escape clause `playwright install chromium` was run; it succeeded
(chromium 1228 + headless shell, ~291 MiB). Nothing else was needed.

Venv: `…/scratchpad/wp-venv` — session-scoped. A future session will need to
recreate it; `python3 -m venv … && pip install playwright` is sufficient now
that the browser is in the shared cache.

## Deviations from the brief

None of substance. The three Python scripts are verbatim. Beyond the required
token set the design system adds `--surface-2`, `--seq-1…6`, `--div-*`,
`--masked-in`/`--masked-out`, `--grid`, `--axis`, `--annotation`, `--focus`,
`--accent-weak`, the four status tokens, three font stacks, a seven-step type
scale, and the geometry tokens — each documented inline, and each added so that
no later figure has a reason to hardcode a value.
