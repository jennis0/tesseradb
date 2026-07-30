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
