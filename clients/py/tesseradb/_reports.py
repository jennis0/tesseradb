"""The reports that `check()`, `commit()`, `insert()`, `declare_columns()` and the change methods
return.

No call prints. Each report shows as a short summary of what happened, in numbers, so a notebook
cell that ends in one shows it. Every refusal and every finding is in the summary in full. The
detail is on the report's attributes, which each class's docstring lists.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import re
from typing import Sequence

from ._columns import DeclaredColumn


class Summarised:
    """A report that shows as the lines `summary()` returns."""

    def summary(self) -> list[str]:
        """The lines `print()` shows."""
        raise NotImplementedError

    def __str__(self) -> str:
        return "\n".join(self.summary())

    def __repr__(self) -> str:
        return str(self)


#: The check page's closing line, which counts the warnings above it.
_WARNINGS = re.compile(r"^check OK:.* (\d+) warning\(s\)$")


def _count(n: int, one: str, many: str | None = None) -> str:
    return f"{n:,} {one if n == 1 else (many or one + 's')}"


def _by_reason(refused: Sequence[dict]) -> dict:
    """How many refused rows or members give each reason."""
    counts: dict = {}
    for one in refused:
        counts[one["reason"]] = counts.get(one["reason"], 0) + 1
    return counts


def _reasons_in_words(counts: dict) -> str:
    return ", ".join(f"{reason.replace('_', ' ')} {n:,}" for reason, n in counts.items())


@dataclass(repr=False)
class Declared(Summarised):
    """What `declare_columns` declared.

    - `columns`: a `DeclaredColumn` for each column of the frame that was neither skipped nor
      already declared, including those no type fits, which were not declared.
    - `vocabularies`: the vocabularies declared for category columns.
    """

    columns: list[DeclaredColumn] = field(default_factory=list)
    vocabularies: list[str] = field(default_factory=list)

    def summary(self) -> list[str]:
        """The lines `print()` shows: each column and what it was declared as, and the
        vocabularies declared."""

        def flags(column: DeclaredColumn) -> str:
            named = [flag for flag in ("render", "index") if getattr(column, flag)]
            return column.declared_as + "".join(f", {flag}" for flag in named)

        out = [
            f"declared {_count(len(self.columns), 'column')}: "
            + (", ".join(f"{c.name} ({flags(c)})" for c in self.columns) or "none")
        ]
        if self.vocabularies:
            out.append(
                f"and {_count(len(self.vocabularies), 'open vocabulary', 'open vocabularies')}, "
                "public, so every reader may list their values: " + ", ".join(self.vocabularies)
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
      Each is in the summary, since a declared column with nothing to fill it fails the build.
    - `findings`: problems found before anything was built. Each is in the summary.
    - `log`: the text the declaration check printed, and on a commit the build's log after it.
      Its warnings are in the summary. A check or build that failed is refused somewhere in it,
      so the summary of a report that is not `ok` carries the whole log.
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
        """The lines `print()` shows: the rows inserted, then every note, finding and warning,
        or the whole log where the check failed."""
        out = [
            f"check: {'ok' if self.ok else 'FAILED'}, inserted "
            f"{self._rows_in_words() or 'no rows'}"
        ]
        return out + self._problems()

    def _problems(self) -> list[str]:
        """The notes, the findings, and the log's warnings, or the whole log where not `ok`."""
        out = [f"  {note}" for note in self.notes]
        out += [str(finding) for finding in self.findings]
        if not self.ok:
            if self.log:
                out.append(self.log.rstrip())
            return out
        for line in self.log.splitlines():
            stripped = line.strip()
            counted = _WARNINGS.search(stripped)
            if stripped.startswith("WARNING") or (counted and int(counted.group(1)) > 0):
                out.append(f"  {stripped}")
        return out


@dataclass(repr=False)
class CommitReport(Report):
    """What the first `commit()` did: a `Report`, and the database it built and started.

    - `views`: the views the database serves, each group's views counted under the group.
    - `layers`: the annotation layers and label sets it declares.
    - `items`, `minted`, `unclustered`: the build's own figures, read from its log: the items
      it built, the annotations it made from key columns, and the member rows in no annotation
      (a null key). Each is `None` where the log does not give it.
    - `seconds`: how long the check, the build and the server's start took.
    - `viewer`, `session`, `control`: the addresses of the viewer plane, where readers read, the
      session plane, where tokens are made, and the control plane, where the operator writes.
    - `identity`: how the rows name their items: by the unique attributes' columns, or each row
      of the points an item of its own.
    - `refused`: the rows the build left out, one entry per file and reason: `source`, the file;
      `object`, the block that read it; `reason`, one of `names_no_item`, `names_two_items`,
      `one_item_twice`, `one_value_twice` and `unknown_tessera_id`; `rows`, how many; and
      `values`, up to ten of them as the file wrote them. Each is in the summary.
    """

    views: dict = field(default_factory=dict)
    layers: list[str] = field(default_factory=list)
    items: int | None = None
    minted: int | None = None
    unclustered: int | None = None
    seconds: float | None = None
    viewer: str | None = None
    session: str | None = None
    control: str | None = None
    identity: str = ""
    refused: list = field(default_factory=list)

    def summary(self) -> list[str]:
        """The lines `print()` shows: the items built, the views and layers, the address served
        at, then every note, finding and warning."""
        if not self.ok:
            return ["commit: FAILED"] + self._problems()
        views = [
            name if n == 1 else f"{name} ({_count(n, 'view')})" for name, n in self.views.items()
        ]
        if self.items is None:
            out = [f"inserted {self._rows_in_words() or 'no rows'}"]
        else:
            out = [f"built {_count(self.items, 'item')} from {self._rows_in_words() or 'no rows'}"]
        if self.minted:
            out.append(f"  {_count(self.minted, 'annotation')} made from key columns")
        if self.unclustered:
            out.append(f"  {_count(self.unclustered, 'member row')} in no annotation")
        for one in self.refused:
            sample = f": {', '.join(one['values'])}" if one.get("values") else ""
            out.append(
                f"  {_count(int(one['rows']), 'row')} of {one['source']} left out "
                f"({one['object']}, {one['reason'].replace('_', ' ')}){sample}"
            )
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

    - `rows_accepted`: rows added, by view: each created an item or added one to the view. `rows`
      is their total.
    - `artifacts_minted`, `memberships_joined`: annotations added and memberships joined.
    - `items_edited`: items already held that a row changed: a value, the label or a position.
      An edited item keeps its `tessera_id`. Placing an item in an annotation changes the
      annotation, not the item, and is counted in `memberships_joined`.
    - `values_bound`, `titles_set`: vocabulary values added and titles replaced.
    - `already_present`: parts the database already held, which changed nothing, rows naming an
      item they matched among them.
    - `without_content`: annotations added without the content they declare.
    - `clipped`: rows outside the range the view's projection can place, stored on the view's
      edge.
    - `clamped`: rows outside a view's extent, moved onto its edge.
    - `refusals`: each refused request, with its status and the server's answer.
    - `refused`: each row or member the identity rule refused and the server left out while
      applying the rest of its request. A row is `{"target", "view", "row", "reason"}`, `row`
      being its position in the table inserted into `target`; a member is `{"layer", "level",
      "view", "key", "list", "member", "reason"}`, `member` being the row naming it as sent.
      `reason` is one of `names_no_item`, `names_two_items`, `one_item_twice`, `one_value_twice`
      and `unknown_tessera_id`. `refused_by_reason` counts them.
    - `ignored_columns`: by layer, the fields of its member structs, and the columns of the
      memberships its declaration writes, that name no item, being neither `tessera_id` nor a
      unique attribute's column: those the plan did not send, and any the server says it
      ignored. `check()` names those the plan leaves out.
    - `tessera_ids`: the id given to each row sent, `None` for a refused row.
    - `artifact_ids`: the id given to each added annotation, by layer and then by
      `(level, view, key)`.
    - `replayed`: requests the server had already carried out, which added nothing.
    - `publication`, `flush_wait`, `flush_reached`: the point at which the changes can be read,
      how long the commit waited for it in seconds, and whether it was reached in time.

    `ok` is `True` when no request was refused and nothing was found before sending: rows the
    identity rule refused are counted in the summary by reason and leave `ok` as it is. Every
    finding and refused request is in the summary.
    """

    sent: bool = False
    plan: list[str] = field(default_factory=list)
    findings: list = field(default_factory=list)
    rows_accepted: dict = field(default_factory=dict)
    artifacts_minted: int = 0
    #: Memberships the pages added, to artifacts this commit minted and to artifacts already held.
    memberships_joined: int = 0
    items_edited: int = 0
    values_bound: int = 0
    titles_set: int = 0
    already_present: int = 0
    without_content: int = 0
    clipped: int = 0
    clamped: int = 0
    refusals: list = field(default_factory=list)
    refused: list = field(default_factory=list)
    ignored_columns: dict = field(default_factory=dict)
    tessera_ids: list = field(default_factory=list)
    replayed: list = field(default_factory=list)
    publication: int | None = None
    artifact_ids: dict = field(default_factory=dict)
    flush_wait: float | None = None
    flush_reached: bool = True

    @property
    def ok(self) -> bool:
        """`True` when nothing was refused and nothing was found before sending."""
        return not self.refusals and not self.findings

    @property
    def rows(self) -> int:
        """The rows added, over every view."""
        return sum(self.rows_accepted.values())

    @property
    def refused_by_reason(self) -> dict:
        """How many rows and members `refused` holds for each reason."""
        return _by_reason(self.refused)

    def summary(self) -> list[str]:
        """The lines `print()` shows: the requests planned, or what was added and when it became
        visible, then every finding and refusal."""
        if not self.sent:
            out = [
                f"check: {'ok' if self.ok else 'FAILED'}, "
                f"{_count(len(self.plan), 'request')} planned, nothing sent"
            ]
            return out + self._ignored() + [str(finding) for finding in self.findings]
        added = [f"{_count(n, 'row')} to {view}" for view, n in self.rows_accepted.items()]
        for n, what in (
            (self.artifacts_minted, "annotation"),
            (self.memberships_joined, "membership"),
            (self.values_bound, "vocabulary value"),
            (self.titles_set, "vocabulary title"),
        ):
            if n:
                added.append(_count(n, what))
        line = f"commit: {'ok' if self.ok else 'FAILED'}, added " + (", ".join(added) or "nothing")
        if self.items_edited:
            line += f", edited {_count(self.items_edited, 'item')}"
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
        if self.clamped:
            out.append(f"  rows outside the extent, clamped onto its edge: {self.clamped:,}")
        if self.replayed:
            out.append(f"  requests replayed, adding nothing: {len(self.replayed):,}")
        rows = [one for one in self.refused if "row" in one]
        members = [one for one in self.refused if "member" in one]
        if rows:
            out.append(
                f"  {_count(len(rows), 'row')} refused and left out: "
                f"{_reasons_in_words(_by_reason(rows))}"
            )
        if members:
            out.append(
                f"  {_count(len(members), 'member')} refused and left out: "
                f"{_reasons_in_words(_by_reason(members))}"
            )
        out += self._ignored()
        out += [str(finding) for finding in self.findings]
        out += [
            f"refused {refusal['status']} on {refusal['what']}: {refusal['detail']}"
            for refusal in self.refusals
        ]
        return out

    def _ignored(self) -> list[str]:
        return [
            f"  layer '{layer}': {_count(len(names), 'member column')} naming no item, ignored: "
            f"{', '.join(names)}"
            for layer, names in self.ignored_columns.items()
        ]


@dataclass(repr=False)
class ChangeReport(Summarised):
    """What `remove`, `suppress`, `unsuppress` or `leave` did.

    - `op`: which of them it was.
    - `requested`: how many rows were given.
    - `accepted`: how many were applied.
    - `refused`: each row the identity rule refused, `{"row", "reason"}`, `row` being its
      position in what was given, and `reason` one of `names_no_item`, `names_two_items` and
      `unknown_tessera_id`. The other rows were applied. `refused_by_reason` counts them.
    - `ignored_columns`: the columns given that name no item, being neither `tessera_id` nor a
      unique attribute's column: those not sent, and any the server says it ignored.
    - `refusals`: each refused request, with its status and the server's answer. `ok` is `True`
      when there were none. Each is in the summary.
    """

    op: str
    requested: int = 0
    accepted: int = 0
    refused: list = field(default_factory=list)
    ignored_columns: list = field(default_factory=list)
    refusals: list = field(default_factory=list)

    @property
    def ok(self) -> bool:
        """`True` when no request was refused."""
        return not self.refusals

    @property
    def refused_by_reason(self) -> dict:
        """How many rows `refused` holds for each reason."""
        return _by_reason(self.refused)

    def summary(self) -> list[str]:
        """The lines `print()` shows: the rows given and applied, the rows refused by reason, the
        columns ignored, then every refused request."""
        out = [
            f"{self.op}: {_count(self.requested, 'row')}, {self.accepted:,} applied, "
            f"{'ok' if self.ok else 'FAILED'}"
        ]
        if self.refused:
            out.append(
                f"  {_count(len(self.refused), 'row')} refused: "
                f"{_reasons_in_words(self.refused_by_reason)}"
            )
        if self.ignored_columns:
            out.append(
                f"  {_count(len(self.ignored_columns), 'column')} naming no item, ignored: "
                f"{', '.join(self.ignored_columns)}"
            )
        out += [f"refused {refusal['status']}: {refusal['detail']}" for refusal in self.refusals]
        return out


def render_columns_of(attributes: Sequence[dict]) -> list[str]:
    return [block["name"] for block in attributes if block.get("render")]
