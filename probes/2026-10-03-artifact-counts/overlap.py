"""A points request beside a cold level build, on sessions whose own state is already warm.

    python3 overlap.py <binary> <label> <scratch>

Separates waiting on the build's threads from a new session's own cold work: each points request
here is on a session that has already been served the same points request once.
"""
import json, sys, time
from pathlib import Path

from measure import artifacts, counts, points, post, server, together, token


def main():
    binary, label, scratch = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    out = []
    d = server(binary, scratch)
    try:
        for builder, level in (("85%", 1), ("100%", 2)):
            warm = token("100%" if builder == "85%" else "85%")
            own = token(builder)
            for tok in (warm, own):
                post(tok, counts())
                post(tok, points(7))
            alone = post(warm, points(7))
            built, beside, own_points = together((artifacts(level), own), (points(7), warm), (points(7), own))
            case = {"build": f"{builder} level {level}", "points alone, warm session": alone,
                    "artifacts": built, "points beside, another warm session": beside,
                    "points beside, the building session": own_points}
            print(json.dumps(case), flush=True)
            out.append(case)
        # A new session's first points request, beside a cold build by another session: the new
        # session walks its own mask for the first time while the build walks another.
        for builder, level, newcomer in (("7%", 1, "1%"), ("7%", 2, "85%")):
            own = token(builder)
            post(own, counts())
            fresh = token(newcomer)
            built, first = together((artifacts(level), own), (points(7), fresh))
            alone = post(token(newcomer), points(7))
            case = {"build": f"{builder} level {level}", "artifacts": built,
                    f"first points of a new {newcomer} session beside it": first,
                    f"first points of another new {newcomer} session after it": alone}
            print(json.dumps(case), flush=True)
            out.append(case)
    finally:
        d.stop()
    (Path(__file__).parent / f"overlap-{label}.json").write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
