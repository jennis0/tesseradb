#!/usr/bin/env python3
"""Static checks on the built whitepaper: CSP, structure, theming."""
import re
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "tessera-processes.html"
FIGURE_IDS = [
    "fig-ingest", "fig-auth", "fig-query", "fig-update",
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
