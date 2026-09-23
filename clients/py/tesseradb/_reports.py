"""The reports that `check()`, `commit()`, `declare_columns()` and the change methods return.

Each prints as plain text, so a notebook cell that returns one shows it.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Sequence

from ._columns import DeclaredColumn


class Printed:
    """A report that prints as the lines `lines()` returns."""

    def lines(self) -> list[str]:
        raise NotImplementedError

    def __str__(self) -> str:
        return "\n".join(self.lines())

    __repr__ = __str__


@dataclass
class Declared(Printed):
    """What `declare_columns` declared: one row per column, and the vocabularies it added.

    - `columns`: each column's name, data type, what it was declared as, its `render` and
      `index` flags, and why.
    - `vocabularies`: the vocabularies declared for category columns.
    """

    columns: list[DeclaredColumn] = field(default_factory=list)
    vocabularies: list[str] = field(default_factory=list)

    def lines(self) -> list[str]:
        out = [
            "declare_columns: every column below is declared as details, stored in the record "
            "blob and shown at drill-down, with the flags render= and index= named",
            f"  {'column':<26} {'dtype':<16} {'declared as':<14} {'render':<7} {'index':<6} why",
        ]
        for column in self.columns:
            out.append(
                f"  {column.name:<26} {column.dtype:<16} {column.declared_as:<14} "
                f"{str(column.render).lower():<7} {str(column.index).lower():<6} {column.why}"
            )
        if self.vocabularies:
            out.append(
                "  vocabularies declared open and public: "
                + ", ".join(self.vocabularies)
                + ". Every reader may list their values"
            )
        return out


@dataclass
class Report(Printed):
    """What `check()` or a first `commit()` found, and what the package decided on the way.

    - `what`: `"check"` or `"commit"`.
    - `ok`: `True` if nothing was refused.
    - `frames`: each view's name and the extent it gets.
    - `render_columns`: the columns sent with every point drawn.
    - `notes`: what each insert read and ignored, and each declared column no insert fills.
    - `findings`: problems found before anything was built.
    - `output`: the page the declaration check and the build printed.
    """

    what: str
    ok: bool
    frames: list[tuple[str, str]] = field(default_factory=list)
    render_columns: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    findings: list = field(default_factory=list)
    output: str = ""

    def lines(self) -> list[str]:
        out = [f"{self.what}: {'ok' if self.ok else 'FAILED'}"]
        out += [f"  {note}" for note in self.notes]
        if self.frames:
            out.append("frames (a frame is fixed at the first commit and never changes)")
            out += [f"  {view:<26} {extent}" for view, extent in self.frames]
        if self.render_columns:
            out.append(
                "render columns, fixed at the first commit (a later declare_attribute(render=True) "
                "is refused; an indexed column can be added at any time)"
            )
            out.append("  " + ", ".join(self.render_columns))
        if self.findings:
            out.append("pre-flight")
            out += [f"  {finding}" for finding in self.findings]
        if self.output:
            out.append("")
            out.append(self.output.rstrip())
        return out


@dataclass
class CommitReport(Report):
    """What the first `commit()` did: a `Report`, and where the new server listens.

    - `viewer`, `session`, `control`: the addresses readers read from, tokens are made at, and
      the operator writes to.
    - `identity`: how the rows are named, by their id column or by `tessera_id`.
    """

    viewer: str | None = None
    session: str | None = None
    control: str | None = None
    identity: str = ""

    def lines(self) -> list[str]:
        out = super().lines()
        if self.identity:
            out.insert(1, f"  {self.identity}")
        out.insert(
            1,
            "  the items' order on disk was chosen from the whole inserted corpus, which affects "
            "speed and never answers",
        )
        if self.viewer:
            out.append("")
            out.append(
                f"serving  viewer {self.viewer}  session {self.session}  control {self.control}"
            )
        return out


@dataclass
class PagedReport(Printed):
    """What a `check()` or `commit()` after the first one planned or did.

    `check()` returns it with `sent` false: the requests a commit would send, in order, and any
    problem found before sending. `commit()` returns the same plan with what happened:

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

    `ok` is `True` when nothing was refused and nothing was found before sending.
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

    def lines(self) -> list[str]:
        what = "commit" if self.sent else "check"
        out = [f"{what}: {'ok' if self.ok else 'FAILED'}"]
        if self.plan:
            out.append(f"plan ({len(self.plan)} request(s), in the order they are sent)")
            out += [f"  {line}" for line in self.plan]
        else:
            out.append("plan: nothing to send")
        if self.findings:
            out.append("pre-flight")
            out += [f"  {finding}" for finding in self.findings]
        if not self.sent:
            return out
        out.append("sent")
        for view, rows in self.rows_accepted.items():
            out.append(f"  rows accepted into view '{view}': {rows}")
        out.append(f"  artifacts minted: {self.artifacts_minted}")
        out.append(f"  memberships joined: {self.memberships_joined}")
        if self.values_filled:
            out.append(f"  values filled: {self.values_filled}")
        if self.values_bound:
            out.append(f"  vocabulary values bound to a code: {self.values_bound}")
        if self.titles_set:
            out.append(f"  vocabulary titles replaced: {self.titles_set}")
        out.append(f"  parts already present: {self.already_present}")
        if self.without_content:
            out.append(f"  artifacts published without their declared content: {self.without_content}")
        if self.clipped:
            out.append(f"  rows clipped onto the frame's edge by the projection: {self.clipped}")
        for line in self.replayed:
            out.append(f"  replayed, nothing landed: {line}")
        if self.flush_wait is not None:
            reached = "" if self.flush_reached else ", not reached within the wait"
            at = "" if self.publication is None else f" {self.publication}"
            out.append(f"  waited {self.flush_wait:.2f} s for publication{at}{reached}")
        for refusal in self.refusals:
            out.append(
                f"  refused {refusal['status']} on {refusal['what']}: {refusal['detail']}"
            )
        return out


@dataclass
class ChangeReport(Printed):
    """What `remove`, `suppress`, `unsuppress` or `leave` did.

    - `op`: which of them it was.
    - `requested`: how many ids were given.
    - `refusals`: each refused request, with its status and the server's answer. `ok` is `True`
      when there were none.
    """

    op: str
    requested: int = 0
    refusals: list = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.refusals

    def lines(self) -> list[str]:
        out = [f"{self.op}: {self.requested} id(s), {'ok' if self.ok else 'FAILED'}"]
        for refusal in self.refusals:
            out.append(f"  refused {refusal['status']}: {refusal['detail']}")
        return out


def render_columns_of(attributes: Sequence[dict]) -> list[str]:
    return [block["name"] for block in attributes if block.get("render")]
