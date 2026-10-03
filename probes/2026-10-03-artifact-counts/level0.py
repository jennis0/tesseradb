"""The level-0 rows of `measure.py`'s `levels` phase alone, on a fresh capped server.

    python3 level0.py <binary> <label> <scratch>
"""
import json, sys
from pathlib import Path

from measure import artifacts, post, server, token


def main():
    binary, label, scratch = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    out = []
    d = server(binary, scratch)
    try:
        for principal in ("100%", "85%", "7%", "1%"):
            for which in ("new", "second session"):
                r = post(token(principal), artifacts(0))
                r.update(level=0, principal=principal, which=which)
                print(json.dumps(r), flush=True)
                out.append(r)
    finally:
        d.stop()
    (Path(__file__).parent / f"level0-{label}.json").write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
