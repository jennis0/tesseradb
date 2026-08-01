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
