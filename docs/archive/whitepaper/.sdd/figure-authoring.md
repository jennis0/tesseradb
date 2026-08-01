# Figure & prose authoring contract

Read this before writing. It is the same for every part of the paper.

## The paper

A public white paper explaining how Tessera works — a permission-masked point
service: an interactive map over a document corpus where a viewer's permissions
determine not just which items they retrieve but every count, density, cluster
and summary they see. Reader: technically literate, no database-internals
background. Register: a serious systems paper, not a product page. You are
writing one part of six; write as if the whole thing has one author.

## Voice

- British spelling. Authorisation, visualisation, colour, normalise.
- Plain declaratives. Explain the mechanism, then name it — never the reverse.
- No marketing register, no "revolutionary", no rhetorical questions, no
  "Let's dive in". No second person addressing the reader as "you" except where
  describing what a viewer of the system sees.
- Assume the reader is smart and uninformed. Define a term the first time it is
  used, in a clause, not a glossary aside.
- Use the project's established vocabulary, which carries its literature:
  conservative label join, boolean expression indexing, partial evaluation,
  Non-Truman model, compartmented / lattice-based mandatory access control.
  Do not invent synonyms for these.
- Never claim the system is deployed or in production. It is specified and
  measured; Phase 1 is in progress. The header already says this once — do not
  re-hedge in every paragraph.

## Accuracy — this is public

- Every claim about a third-party system and every number comes from your fact
  sheet at `docs/archive/whitepaper/facts/part-N.md`. If it is not in the fact sheet,
  do not write it. Do not supply numbers from your own knowledge.
- Numbers carry their units and, where the source attaches them, their
  measurement conditions. Do not round.
- Preserve demonstrated-vs-marketed distinctions exactly as the sources draw
  them.
- Where you quote an invariant, quote it as the fact sheet has it.

## Output format

Write ONE file: `docs/archive/whitepaper/src/<NN>-part-<N>.html`. It is a fragment
concatenated into a larger page — so:

- NO `<!doctype>`, and no html/head/body elements. Content only.
- The validator greps for those tag names and for `fetch(`, `XMLHttpRequest`,
  `WebSocket` as literal strings **anywhere, including inside comments and
  prose**. Do not write them even in a comment.
- No external resources of any kind. No CDN, no remote font, no remote image,
  no network calls. Citation links in anchor hrefs are fine.
- Wrap your part in `<section class="part" id="part-N">`.

## Structure and CSS

The design system is already built; use it, never override it.

- Prose sits in the default measure. `.wide` on a block widens it to the
  figure band; `.full` goes edge to edge.
- Figure markup:
  ```
  <figure id="fig-fN" class="wide">
    <div class="fig-frame fig-scroll">…graphic…</div>
    <figcaption>…</figcaption>
  </figure>
  ```
- `.fig-scroll` is `overflow-x: auto`, but `svg`, `img` and `canvas` shrink to
  fit inside it. For a graphic with a fixed minimum width, put a
  `<div class="fig-fixed">` inside the `.fig-scroll`.
- `.fig-controls` is the row for interactive controls. Buttons use
  `aria-pressed`; only the pressed state is styled for you.
- **Never hardcode a colour.** Use the tokens: `--bg --fg --muted --rule
  --accent --surface --surface-2`, series `--cat-1 … --cat-6`, sequential
  `--seq-*`, divergent `--div-*`, `--masked-in` / `--masked-out`, chrome
  `--grid --axis --annotation --focus`.
- **Light-mode contrast:** `--cat-3`, `--cat-4` and `--cat-5` fall below 3:1 on
  the light surface. A figure using those slots must carry direct labels or a
  table — never colour alone. Prefer `--cat-1`, `--cat-2`, `--cat-6` for
  anything where colour alone distinguishes series.
- A figure styled purely through `var(--…)` re-themes for free.

## The JS runtime (global `TF`, already loaded)

| Call | Behaviour |
|---|---|
| `TF.theme()` | `"light"` or `"dark"`, resolving the data-theme stamp over OS preference |
| `TF.onThemeChange(fn)` | Fires on OS change and data-theme stamp. Returns an unsubscribe. **Only needed for canvas figures or anything reading token values in JS.** |
| `TF.svg(tag, attrs)` | Namespaced element factory. Null attrs skipped; `text:` sets textContent; `children:` appends an array |
| `TF.fmt(n, decimals)` | `1048576 → "1,048,576"`; `(2.4157, 2) → "2.42"`; `null → "—"` |
| `TF.fmt.dur(us)` | Input in **microseconds**. `430 → "430 µs"`, `21500 → "21.5 ms"` |
| `TF.fmt.bytes(b)` | `1536000 → "1.46 MiB"` |
| `TF.token(name, el)` | Reads a custom property. The only correct way for a canvas figure to get a colour |
| `TF.reducedMotion()` | Animated figures must check this and show their end state immediately |
| `TF.NS` | SVG namespace string |

Write vanilla ES2020. No framework, no build step, no charting library.
Scope every figure's JS inside an IIFE so parts cannot collide.

## Interaction and accessibility

- Every interactive figure must be keyboard operable and show a visible focus
  state. Drag-only is not acceptable — provide buttons, a slider, or arrow-key
  handling alongside.
- Every figure needs a caption that states the figure's point in prose, so the
  paper survives with graphics unavailable.
- Deterministic data only: seed any PRNG inline. No data files, no fetches.
  Figures must render identically on every load.

## Verify before you report

```bash
cd /home/joe/code/tessera
python3 docs/archive/whitepaper/build.py && python3 docs/archive/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/archive/whitepaper/render_check.py
```

The validator will still report missing figures belonging to other parts —
that is expected. What must be true: **your** figure IDs no longer appear in
its output, no non-figure failures appear, and `render_check.py` exits 0 with
no console errors and no body overflow. Then read your screenshots at
`/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/shots/`
and confirm your figures actually look right in both themes and at 390px.

Other authors are writing other parts into the same page at the same time.
Only ever edit your own fragment file. If the build shows someone else's
part mid-write, ignore it.
