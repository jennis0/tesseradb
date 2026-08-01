#!/usr/bin/env python3
"""Render the built whitepaper and fail on console errors or body overflow."""
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

OUT = Path(__file__).resolve().parent.parent / "tessera-processes.html"
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
