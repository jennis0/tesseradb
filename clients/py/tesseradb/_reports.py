"""What `check()` and `commit()` hand back.

Both return an object that prints as a table. The binary's own output is carried through rather
than re-formatted, `tessera check`'s disclosure table being what a reader of the declaration is
meant to read. Beside it the SDK states what it decided for the user: what it inferred and
under which thresholds, the frame each view got, the render columns the first commit froze, and
the vocabularies it declared open.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Sequence

from ._infer import FEW_DISTINCT, SHORT_MEDIAN, InferredColumn


@dataclass
class Inference:
    """The table §4.5 asks the SDK to print once."""

    source: str | None = None
    columns: list[InferredColumn] = field(default_factory=list)
    vocabularies: list[str] = field(default_factory=list)

    def lines(self) -> list[str]:
        if not self.columns:
            return []
        out = [
            f"inferred from '{self.source}', the default source "
            f"(at most {FEW_DISTINCT} distinct values is a category; a median length under "
            f"{SHORT_MEDIAN} characters is a keyword. Both are assumed, and one "
            f"declare_attribute call overrides either for one column)",
            f"  {'column':<26} {'dtype':<16} {'declared as':<14} {'render':<7} {'index':<6} why",
        ]
        for column in self.columns:
            out.append(
                f"  {column.name:<26} {column.dtype:<16} {column.declared_as:<14} "
                f"{str(column.render).lower():<7} {str(column.index).lower():<6} {column.why}"
            )
        if self.vocabularies:
            out.append(
                "  vocabularies declared open and public, minted from the data: "
                + ", ".join(self.vocabularies)
                + ". Every principal is told their value names, and on a local database the user is "
                "the authority that choice asks for"
            )
        return out


@dataclass
class Report:
    """The common shape: the SDK's own decisions, then the binary's output."""

    what: str
    ok: bool
    inference: Inference
    frames: list[tuple[str, str]] = field(default_factory=list)
    render_columns: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
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
        out += self.inference.lines()
        if self.output:
            out.append("")
            out.append(self.output.rstrip())
        return out

    def __str__(self) -> str:
        return "\n".join(self.lines())

    __repr__ = __str__


@dataclass
class CommitReport(Report):
    """The first commit's report: the build's own output, and where the instance is listening."""

    viewer: str | None = None
    session: str | None = None
    control: str | None = None
    #: How many ids the map assigned. Zero where every source was read in place, the map being the
    #: identity over their own ids.
    entities: int = 0

    def lines(self) -> list[str]:
        out = super().lines()
        if self.entities:
            out.insert(1, f"  {self.entities} entity id(s) assigned from the user's own ids")
        out.insert(
            1,
            "  the allocation is signature-sorted over the whole staged corpus, which affects "
            "posting compression and latency and never what is served",
        )
        if self.viewer:
            out.append("")
            out.append(
                f"serving  viewer {self.viewer}  session {self.session}  control {self.control}"
            )
        return out


def render_columns_of(attributes: Sequence[dict]) -> list[str]:
    return [block["name"] for block in attributes if block.get("render")]
