#!/usr/bin/env python3
"""Build the process-reference page. Reuses the white paper's design system."""
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC = ROOT / "src"
STYLE = ROOT.parent / "whitepaper" / "src" / "00-style.html"
OUT = ROOT.parent / "tessera-processes.html"


def main() -> None:
    frags = sorted(SRC.glob("*.html"))
    if not frags:
        raise SystemExit("no fragments found in %s" % SRC)
    parts = ["<!-- shared design system: whitepaper/src/00-style.html -->",
             STYLE.read_text(encoding="utf-8").rstrip()]
    for f in frags:
        parts.append("<!-- %s -->" % f.name)
        parts.append(f.read_text(encoding="utf-8").rstrip())
    OUT.write_text("\n".join(parts) + "\n", encoding="utf-8")
    print("wrote %s (%d fragments + shared style, %d bytes)"
          % (OUT, len(frags), OUT.stat().st_size))


if __name__ == "__main__":
    main()
