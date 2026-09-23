"""The reports that `check()`, `commit()`, `insert()`, `declare_columns()` and the change methods
return.

No call prints. Each report shows as a short summary of what happened, in numbers, so a notebook
cell that ends in one shows it. Every refusal and every finding is in the summary in full. The
detail is on the report's attributes, which each class's docstring lists.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Sequence

from ._columns import DeclaredColumn


class Summarised:
    """A report that shows as the lines `summary()` returns."""

    def summary(self) -> list[str]:
        raise NotImplementedError

    def __str__(self) -> str:
        return "\n".join(self.summary())

    def __repr__(self) -> str:
        return str(self)


def _count(n: int, one: str, many: str | None = None) -> str:
    return f"{n:,} {one if n == 1 else (many or one + 's')}"


@dataclass(repr=False)
class Declared(Summarised):
    """What `declare_columns` declared.

    - `columns`: one row per column declared: its name, data type, what it was declared as, its
      `render` and `index` flags, and why.
    - `vocabularies`: the vocabularies declared for category columns.
    """

    columns: list[DeclaredColumn] = field(default_factory=list)
    vocabularies: list[str] = field(default_factory=list)

    def summary(self) -> list[str]:
        def flags(column: DeclaredColumn) -> str:
            named = [flag for flag in ("render", "index") if getattr(column, flag)]
            return column.declared_as + "".join(f", {flag}" for flag in named)

        out = [
            f"declared {_count(len(self.columns), 'column')}: "
            + (", ".join(f"{c.name} ({flags(c)})" for c in self.columns) or "none")
        ]
        if self.vocabularies:
            out.append(
                f"and {_count(len(self.vocabularies), 'open vocabulary', 'open vocabularies')}: "
                + ", ".join(self.vocabularies)
            )
        return out


@dataclass(repr=False)
class Report(Summarised):
    """What `check()` found before the first commit, or what the first `commit()` did.

    - `what`: `"check"` or `"commit"`.
    - `ok`: `True` if nothing was refused.
    - `rows`: the rows inserted, by view or view group.
    - `frames`: each view's name and the extent it gets.
    - `render_columns`: the columns sent with every point drawn.
    - `notes`: what each insert read and ignored, and each declared column no insert fills.
    - `findings`: problems found before anything was built. Each is in the summary.
    - `log`: the text the declaration check printed, and on a commit the build's log after it.
      A check or build that failed is refused somewhere in it, so the summary of a report that
      is not `ok` carries the whole log.
    """

    what: str
    ok: bool
    rows: dict = field(default_factory=dict)
    frames: list[tuple[str, str]] = field(default_factory=list)
    render_columns: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    findings: list = field(default_factory=list)
    log: str = ""

    def _rows_in_words(self) -> str:
        return "; ".join(f"{target}: {_count(n, 'row')}" for target, n in self.rows.items())

    def summary(self) -> list[str]:
        out = [
            f"check: {'ok' if self.ok else 'FAILED'}, "
            f"{self._rows_in_words() or 'no rows inserted'}"
        ]
        return out + self._problems()

    def _problems(self) -> list[str]:
        out = [str(finding) for finding in self.findings]
        if not self.ok and self.log:
            out.append(self.log.rstrip())
        return out


@dataclass(repr=False)
class CommitReport(Report):
    """What the first `commit()` did: a `Report`, and the database it built and started.

    - `views`: the views the database serves, each group's views counted under the group.
    - `layers`: the annotation layers it holds.
    - `seconds`: how long the check, the build and the server's start took.
    - `viewer`, `session`, `control`: the addresses readers read from, tokens are made at, and
      the operator writes to.
    - `identity`: how the rows are named, by their id column or by `tessera_id`.
    """

    views: dict = field(default_factory=dict)
    layers: list[str] = field(default_factory=list)
    seconds: float | None = None
    viewer: str | None = None
    session: str | None = None
    control: str | None = None
    identity: str = ""

    def summary(self) -> list[str]:
        if not self.ok:
            return ["commit: FAILED"] + self._problems()
        views = [
            name if n == 1 else f"{name} ({_count(n, 'view')})" for name, n in self.views.items()
        ]
        out = [f"built {self._rows_in_words() or 'no rows'}"]
        if views:
            out.append(f"  {_count(len(views), 'view')}: {', '.join(views)}")
        if self.layers:
            out.append(f"  {_count(len(self.layers), 'layer')}: {', '.join(self.layers)}")
        took = "" if self.seconds is None else f"in {self.seconds:.1f} s; "
        out.append(f"  {took}serving at {self.viewer}" if self.viewer else f"  {took}not serving")
        return out + self._problems()


@dataclass(repr=False)
class PagedReport(Summarised):
    """What a `check()` or `commit()` after the first one planned or did.

    `check()` returns it with `sent` false: `plan` lists the requests a commit would send, in
    order, and `findings` any problem found before sending. `commit()` returns the same plan with
    what happened:

    - `rows_accepted`: rows added, by view. `rows` is their total.
    - `artifacts_minted`, `memberships_joined`: annotations added and memberships joined.
    - `values_filled`: attribute values set on items already held.
    - `values_bound`, `titles_set`: vocabulary values added and titles replaced.
    - `already_present`: parts the database already held, which changed nothing.
    - `without_content`: annotations added without the content they declare.
    - `clipped`: rows moved onto the edge of a view's extent by its projection.
    - `refusals`: each refused request, with its status and the server's answer.
    - `tessera_ids`: the id given to each added row.
    - `artifact_ids`: the id given to each added annotation, by layer and then by
      `(level, view, key)`.
    - `replayed`: requests the server had already carried out, which added nothing.
    - `publication`, `flush_wait`, `flush_reached`: the point at which the changes can be read,
      how long the commit waited for it in seconds, and whether it was reached in time.

    `ok` is `True` when nothing was refused and nothing was found before sending. Every finding
    and refusal is in the summary.
    """

    sent: bool = False
    plan: list[str] = field(default_factory=list)
    findings: list = field(default_factory=list)
    rows_accepted: dict = field(default_factory=dict)
    artifacts_minted: int = 0
    #: Memberships the pages added, to artifacts this commit minted and to artifacts already held.
    memberships_joined: int = 0
    values_filled: int = 0
    values_bound: int = 0
    titles_set: int = 0
    already_present: int = 0
    without_content: int = 0
    clipped: int = 0
    refusals: list = field(default_factory=list)
    tessera_ids: list = field(default_factory=list)
    replayed: list = field(default_factory=list)
    publication: int | None = None
    artifact_ids: dict = field(default_factory=dict)
    flush_wait: float | None = None
    flush_reached: bool = True

    @property
    def ok(self) -> bool:
        return not self.refusals and not self.findings

    @property
    def rows(self) -> int:
        return sum(self.rows_accepted.values())

    def summary(self) -> list[str]:
        if not self.sent:
            out = [
                f"check: {'ok' if self.ok else 'FAILED'}, "
                f"{_count(len(self.plan), 'request')} planned, nothing sent"
            ]
            return out + [str(finding) for finding in self.findings]
        added = [f"{_count(n, 'row')} to {view}" for view, n in self.rows_accepted.items()]
        for n, what in (
            (self.artifacts_minted, "annotation"),
            (self.memberships_joined, "membership"),
            (self.values_filled, "value"),
            (self.values_bound, "vocabulary value"),
            (self.titles_set, "vocabulary title"),
        ):
            if n:
                added.append(_count(n, what))
        line = f"commit: {'ok' if self.ok else 'FAILED'}, added " + (", ".join(added) or "nothing")
        if self.flush_wait is not None:
            at = "" if self.publication is None else f" for publication {self.publication}"
            if self.flush_reached:
                line += f"; visible after {self.flush_wait:.2f} s{at}"
            else:
                line += f"; not visible after waiting {self.flush_wait:.2f} s{at}"
        out = [line]
        if self.already_present:
            out.append(f"  already present, changing nothing: {self.already_present:,}")
        if self.without_content:
            out.append(
                f"  annotations published without their declared content: {self.without_content:,}"
            )
        if self.clipped:
            out.append(f"  rows clipped onto the frame's edge by the projection: {self.clipped:,}")
        if self.replayed:
            out.append(f"  requests replayed, adding nothing: {len(self.replayed):,}")
        out += [str(finding) for finding in self.findings]
        out += [
            f"refused {refusal['status']} on {refusal['what']}: {refusal['detail']}"
            for refusal in self.refusals
        ]
        return out


@dataclass(repr=False)
class ChangeReport(Summarised):
    """What `remove`, `suppress`, `unsuppress` or `leave` did.

    - `op`: which of them it was.
    - `requested`: how many ids were given.
    - `refusals`: each refused request, with its status and the server's answer. `ok` is `True`
      when there were none. Each is in the summary.
    """

    op: str
    requested: int = 0
    refusals: list = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.refusals

    def summary(self) -> list[str]:
        out = [f"{self.op}: {_count(self.requested, 'id')}, {'ok' if self.ok else 'FAILED'}"]
        out += [f"refused {refusal['status']}: {refusal['detail']}" for refusal in self.refusals]
        return out


def render_columns_of(attributes: Sequence[dict]) -> list[str]:
    return [block["name"] for block in attributes if block.get("render")]
