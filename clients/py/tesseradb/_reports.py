"""What the SDK's verbs hand back.

`check()` and `commit()` return an object that prints as a table: what the declaration check read
from each file and what it refuses. Beside it the SDK states what it decided for the user: what
each insert read and ignored, the frame each view got, and the render columns the first commit
froze. `declare_columns` prints the table it declared.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Sequence

from ._columns import DeclaredColumn


class Printed:
    """What a verb hands back and a cell prints: one `lines()`, and the text is those lines."""

    def lines(self) -> list[str]:
        raise NotImplementedError

    def __str__(self) -> str:
        return "\n".join(self.lines())

    __repr__ = __str__


@dataclass
class Declared(Printed):
    """The table `declare_columns` prints."""

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
                + ". Every principal is told their value names, and on a local database the user "
                "is the authority that choice asks for"
            )
        return out


@dataclass
class Report(Printed):
    """The common shape: the SDK's own decisions, then the check's page."""

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
    """The first commit's report: the build's own output, and where the instance is listening."""

    viewer: str | None = None
    session: str | None = None
    control: str | None = None
    #: How this database names a row: its id column, or the tessera_id.
    identity: str = ""

    def lines(self) -> list[str]:
        out = super().lines()
        if self.identity:
            out.insert(1, f"  {self.identity}")
        out.insert(
            1,
            "  the allocation is signature-sorted over the whole inserted corpus, which affects "
            "posting compression and latency and never what is served",
        )
        if self.viewer:
            out.append("")
            out.append(
                f"serving  viewer {self.viewer}  session {self.session}  control {self.control}"
            )
        return out


@dataclass
class PagedReport(Printed):
    """What a later `check()` and `commit()` hand back.

    `check()` returns it with `sent` false: the plan and the pre-flight, with nothing sent. The two
    are one object because they come from one planner, so what a check prints is what a commit
    does.
    """

    sent: bool = False
    plan: list[str] = field(default_factory=list)
    findings: list = field(default_factory=list)
    rows_accepted: dict = field(default_factory=dict)
    artifacts_minted: int = 0
    memberships_joined: int = 0
    values_filled: int = 0
    #: Vocabulary values this commit's declarations and value pages drew a code for.
    values_bound: int = 0
    #: Held values whose title one of those pages replaced.
    titles_set: int = 0
    #: Parts a page supplied that the database already held: the fill rule's no-effect arm.
    already_present: int = 0
    without_content: int = 0
    clipped: int = 0
    refusals: list = field(default_factory=list)
    #: The identities the ingest route answered with, one per accepted row.
    tessera_ids: list = field(default_factory=list)
    #: The pages the server answered as a replay of one it had already applied.
    replayed: list = field(default_factory=list)
    #: The publication this commit's work is visible at, from the closing flush's answer.
    publication: int | None = None
    #: The identity each published artifact was given, by layer and key. A layer's own
    #: `tessera_id` is the only address by which it can later be addressed.
    artifact_ids: dict = field(default_factory=dict)
    flush_wait: float | None = None
    flush_reached: bool = True

    @property
    def ok(self) -> bool:
        # A finding refuses the commit: the pre-flight sends nothing while one stands, and
        # a commit that sent pages is ok only where every one of them was accepted.
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
    """What `remove`, `suppress` and `unsuppress` hand back."""

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
