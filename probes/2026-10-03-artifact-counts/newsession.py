"""A new session's first points request beside a cold level build: on the building session's own
new token, and on a second new token of the same principal.

    python3 newsession.py <binary> <label> <scratch>
"""
import json, sys
from pathlib import Path

from measure import artifacts, points, post, server, together, token


def main():
    binary, label, scratch = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    out = []
    d = server(binary, scratch)
    try:
        for principal, shared in (("85%", True), ("100%", False), ("85%", False), ("100%", True)):
            level = 1 if (principal, shared) in (("85%", True), ("100%", False)) else 2
            builder = token(principal)
            other = builder if shared else token(principal)
            built, first = together((artifacts(level), builder), (points(7), other))
            case = {"build": f"{principal} level {level}", "points token": "the building one" if shared else "another new one",
                    "artifacts": built, "points": first}
            print(json.dumps(case), flush=True)
            out.append(case)
    finally:
        d.stop()
    (Path(__file__).parent / f"newsession-{label}.json").write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
