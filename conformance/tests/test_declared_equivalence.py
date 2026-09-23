"""Anything declared at a running service serves exactly what a build that declared it serves.

A build is an ingest into an empty database, so each case builds two deployments over one corpus:
one whose declaration carries the thing, with all the data, and one whose declaration omits it,
served, with the thing then declared through the control plane, its data written, and a
publication waited for. Every principal's answers to one battery are recorded against both and
compared after the live declaration, after a restart, after a fold, and after a restart following
that fold. `declared_fixture` holds the corpus, the deployments and the one place identifiers are
rekeyed for the comparison.
"""

from __future__ import annotations

from dataclasses import dataclass

import pyarrow as pa
import pytest

import declared_fixture as fx
from declared_fixture import Column, Corpus, Deployment, Plan, Vocabulary

STAGES = ("live", "restart", "fold", "fold-restart")

ALL = list(range(fx.N_ITEMS))
BUILT = list(range(fx.N_BUILT))
INGESTED = list(range(fx.N_BUILT, fx.N_ITEMS))

#: Items opened in every case: some the live build holds, some it takes over ingest, and several
#: in the compartment the `few` principal sees.
ITEMS = (0, 1, 2, 7, 19, 39, 59, 700, fx.N_BUILT - 1, fx.N_BUILT, fx.N_BUILT + 19, fx.N_ITEMS - 1)


def _world(ids, columns=()) -> dict:
    return {fx.WORLD: (ids, [fx.fx_column(), *columns], 0, fx.WORLD_EXTENT)}


# ---------------------------------------------------------------------------------------------
# Entity-scoped columns
# ---------------------------------------------------------------------------------------------

DEPT = Vocabulary(
    "dept_v", "u8", "closed", "public", tuple((f"d{j}", f"Department {j}") for j in range(6))
)
GRADE = Vocabulary("grade_v", "u16", "open", "derived")

WORDS = ("amber", "basalt", "cedar", "delta", "ember", "fjord", "garnet", "harbour")


def _note(i: int) -> str | None:
    if i % 8 == 0:
        return None
    return f"{WORDS[i % 8]} {WORDS[(i // 8) % 8]} item {i}"


ENTITY_COLUMNS = [
    Column("dept", "category", pa.string(), lambda i: None if i % 7 == 0 else f"d{i % 6}",
           index=True, vocabulary="dept_v"),
    Column("grade", "category", pa.string(), lambda i: None if i % 9 == 0 else f"g{i % 4}",
           vocabulary="grade_v"),
    Column("score", "f32", pa.float32(),
           lambda i: None if i % 11 == 0 else ((i * 37) % 1000) / 8.0, index=True),
    Column("weight", "u32", pa.uint32(), lambda i: None if i % 13 == 0 else (i * 13) % 500),
    Column("tag", "keyword", pa.string(),
           lambda i: None if i % 6 == 0 else f"t{i % 17:02d}-{i % 5}", index=True),
    Column("label", "keyword", pa.string(), lambda i: f"L{i}"),
    Column("seen", "timestamp_us", pa.timestamp("us"),
           lambda i: None if i % 10 == 0 else 1_700_000_000_000_000 + i * 3_600_000_000,
           index=True),
    Column("flag", "bool", pa.bool_(), lambda i: None if i % 14 == 0 else i % 3 == 0, index=True),
    Column("alive", "bool", pa.bool_(), lambda i: i % 2 == 0),
]

TEXT_COLUMNS = [
    Column("note", "text", pa.string(), _note, index=True),
    Column("memo", "text", pa.string(), lambda i: None if i % 5 == 0 else f"memo for {i}"),
]

RENDERED_COLUMNS = [
    Column("score_r", "f32", pa.float32(), lambda i: None if i % 11 == 0 else (i % 64) / 4.0,
           render=True),
    Column("dept_r", "category", pa.string(), lambda i: None if i % 7 == 0 else f"d{i % 6}",
           render=True, vocabulary="dept_v"),
    Column("flag_r", "bool", pa.bool_(), lambda i: i % 3 == 0, render=True),
]

ENTITY_FILTERS = (
    ("dept", {"dept": {"in": ["d2", "d4"]}}, None),
    ("score", {"score": {"range": {"gte": 20.0, "lt": 80.0}}}, None),
    ("tag", {"tag": {"prefix": "t03"}}, None),
    ("seen", {"seen": {"range": {"gte": 1_700_000_000_000_000 + 400 * 3_600_000_000}}}, None),
    ("flag", {"flag": {"eq": True}}, None),
)


# ---------------------------------------------------------------------------------------------
# The cases
# ---------------------------------------------------------------------------------------------


class Case:
    """One kind of declaration: the two corpora, what the live side does, and what is asked."""

    name: str
    plan: Plan

    def built(self, work) -> Deployment:
        raise NotImplementedError

    def live(self, work) -> Deployment:
        raise NotImplementedError


class EntityAttributes(Case):
    """Entity-scoped attributes declared live. The items the live build holds get their cells
    through `/control/values` and the rest arrive over `/control/ingest` carrying them; with
    `values_first` the values page goes first."""

    def __init__(self, name: str, columns: list[Column], filters=(), values_first=False):
        self.name = name
        self.columns = columns
        self.values_first = values_first
        self.plan = Plan(
            filters=filters,
            categories=tuple(c.name for c in columns if c.vocabulary),
            # A rendered category that is not indexed takes no counts on the typeahead.
            suggest=tuple(
                (c.name, q, c.index or not c.render)
                for c in columns
                if c.vocabulary
                for q in ("", "g1", "Dep")
            ),
            items=ITEMS,
        )

    def built(self, work) -> Deployment:
        blocks = [fx.world_view_toml(), fx.fx_column().toml(), *(c.toml() for c in self.columns)]
        return Deployment(work, Corpus(blocks, _world(ALL, self.columns), [DEPT, GRADE]))

    def live(self, work) -> Deployment:
        blocks = [fx.world_view_toml(), fx.fx_column().toml()]
        d = Deployment(work, Corpus(blocks, _world(BUILT), [DEPT, GRADE]))
        for column in self.columns:
            d.control("PUT", "/control/attributes", json=column.payload(), expect=(201,))
        writes = [
            lambda: d.rows(
                "/control/ingest",
                fx.point_rows(INGESTED, [fx.fx_column(), *self.columns]),
                f"{self.name}-ingest",
            ),
            lambda: d.rows(
                "/control/values", fx.value_rows(BUILT, self.columns), f"{self.name}-values"
            ),
        ]
        for write in reversed(writes) if self.values_first else writes:
            write()
        d.publish()
        return d


MOOD = Vocabulary(
    "mood_v", "u8", "closed", "public",
    (("calm", "Calm and settled"), ("tense", "Tense"), ("wild", None), ("sunny", "Sunny spells"),
     ("grey", "Grey")),
)
TOPIC = Vocabulary("topic_v", "u16", "open", "derived")

VOCABULARY_COLUMNS = [
    Column("mood", "category", pa.string(),
           lambda i: None if i % 8 == 0 else MOOD.values[i % 5][0], index=True,
           vocabulary="mood_v"),
    Column("topic", "category", pa.string(),
           lambda i: None if i % 6 == 0 else f"topic-{(i * 7) % 9}", vocabulary="topic_v"),
]


class LiveVocabularies(Case):
    """Two vocabularies declared live, a closed public one whose values arrive in the declaration
    and a later page, and an open derived one minted by ingest, each used by a new attribute."""

    name = "live-vocabularies"
    plan = Plan(
        filters=(("mood", {"mood": {"eq": "sunny"}}, None),),
        categories=("mood", "topic"),
        suggest=(
            ("mood", "", True),
            ("mood", "s", True),
            ("mood", "Gr", True),
            ("topic", "", True),
            ("topic", "topic-1", True),
        ),
        items=ITEMS,
    )

    def built(self, work) -> Deployment:
        blocks = [fx.world_view_toml(), fx.fx_column().toml()]
        blocks += [c.toml() for c in VOCABULARY_COLUMNS]
        corpus = Corpus(blocks, _world(ALL, VOCABULARY_COLUMNS), [MOOD, TOPIC])
        return Deployment(work, corpus)

    def live(self, work) -> Deployment:
        blocks = [fx.world_view_toml(), fx.fx_column().toml()]
        d = Deployment(work, Corpus(blocks, _world(BUILT)))
        first = MOOD.payload()
        first["values"] = first["values"][:3]
        d.control("PUT", "/control/vocabularies/mood_v", json=first, expect=(201,))
        d.control(
            "PATCH", "/control/vocabularies/mood_v/values",
            json={"values": MOOD.payload()["values"][3:]}, expect=(200,),
        )
        d.control("PUT", "/control/vocabularies/topic_v", json=TOPIC.payload(), expect=(201,))
        for column in VOCABULARY_COLUMNS:
            d.control("PUT", "/control/attributes", json=column.payload(), expect=(201,))
        d.rows(
            "/control/ingest",
            fx.point_rows(INGESTED, [fx.fx_column(), *VOCABULARY_COLUMNS]),
            "vocab-ingest",
        )
        d.rows("/control/values", fx.value_rows(BUILT, VOCABULARY_COLUMNS), "vocab-values")
        d.publish()
        return d


ATLAS = "atlas"
ATLAS_EXTENT = (-50.0, 50.0, -50.0, 50.0)
ATLAS_MEMBERS = [i for i in ALL if i % 3 != 0]


def _plain_view_toml(name: str, extent) -> str:
    x0, x1, y0, y1 = extent
    return (
        f'[[view]]\nname = "{name}"\nextent = {{ x = [{x0}, {x1}], y = [{y0}, {y1}] }}\n'
        f'source = "{name}"\nvisibility = "public"\n'
        'point_visibility = { field = "access", default = "public" }\n'
    )


def _extent_body(extent) -> dict:
    x0, x1, y0, y1 = extent
    return {"x": [x0, x1], "y": [y0, y1]}


class PlainView(Case):
    """A plain view created live over items the corpus already holds, which join it by their
    external ids."""

    name = "plain-view"
    plan = Plan(
        views=(fx.WORLD, ATLAS),
        filters=(("fx", {"fx": {"range": {"lt": 400}}}, None),),
        items=ITEMS,
        frames=((ATLAS, ATLAS_EXTENT),),
    )

    def built(self, work) -> Deployment:
        blocks = [fx.world_view_toml(), _plain_view_toml(ATLAS, ATLAS_EXTENT),
                  fx.fx_column().toml()]
        points = {**_world(ALL), ATLAS: (ATLAS_MEMBERS, [], 7, ATLAS_EXTENT)}
        return Deployment(work, Corpus(blocks, points))

    def live(self, work) -> Deployment:
        d = Deployment(work, Corpus([fx.world_view_toml(), fx.fx_column().toml()], _world(ALL)))
        body = {
            "projection": "none",
            "extent": _extent_body(ATLAS_EXTENT),
            "visibility": "public",
            "point_visibility": {"default": "public"},
        }
        d.control("PUT", f"/control/views/{ATLAS}", json=body, expect=(201,))
        d.rows(
            "/control/ingest",
            fx.point_rows(ATLAS_MEMBERS, [], seed=7, extent=ATLAS_EXTENT),
            "atlas",
            view=ATLAS,
        )
        d.publish()
        return d


GROUP = "quarter"
GROUP_EXTENT = (-40.0, 40.0, -40.0, 40.0)
KEYS = {"Q1": 11, "Q2": 12}
KEY_MEMBERS = {
    "Q1": [i for i in ALL if i % 3 != 0],
    "Q2": [i for i in ALL if i % 2 == 0],
}
MOOD_Q = Vocabulary(
    "moodq_v", "u8", "closed", "public", (("calm", None), ("tense", None), ("wild", "Wild"))
)


def _scoped(key: str) -> list[Column]:
    """The group's two families, with this view's values."""
    k = KEYS[key]
    return [
        Column("sentiment", "f32", pa.float32(),
               lambda i: None if (i + k) % 5 == 0 else ((i * 7 + k * 13) % 2001 - 1000) / 1024.0,
               index=True, scope=GROUP),
        Column("feel", "category", pa.string(),
               lambda i: None if (i + k) % 6 == 0 else MOOD_Q.values[(i + k) % 3][0],
               index=True, vocabulary="moodq_v", scope=GROUP),
    ]


def _group_toml() -> str:
    x0, x1, y0, y1 = GROUP_EXTENT
    text = (
        f'[[view_group]]\nname = "{GROUP}"\nextent = {{ x = [{x0}, {x1}], y = [{y0}, {y1}] }}\n'
        'visibility = "public"\npoint_visibility = { field = "access", default = "public" }\n'
        'metadata = { label = "text" }\n'
    )
    for key in KEYS:
        text += f'\n[[view_group.view]]\nkey = "{key}"\nsource = "{key.lower()}"\nlabel = "{key}"\n'
    return text


def _group_view(key: str) -> str:
    return f"{GROUP}:{key}"


class GroupScoped(Case):
    """A view group, its views and a group-scoped family of each of two types, all declared
    live, with each view's rows and scoped values ingested into it. With `recreate`, the second
    view is then dropped and created again under its key, and its rows sent again."""

    def __init__(self, name: str, recreate: bool = False):
        self.name = name
        self.recreate = recreate
        views = tuple(_group_view(k) for k in KEYS)
        self.plan = Plan(
            views=(fx.WORLD, *views),
            filters=(
                ("sentiment", {"sentiment": {"range": {"gte": 0.0}}}, views),
                ("feel", {"feel": {"eq": "tense"}}, views),
                ("pinned", {"sentiment@Q2": {"range": {"lt": 0.0}}}, (fx.WORLD,)),
            ),
            categories=("feel@Q1", "feel@Q2"),
            suggest=(("feel@Q1", "", True), ("feel@Q2", "w", True)),
            items=ITEMS,
            frames=tuple((v, GROUP_EXTENT) for v in views),
        )

    def built(self, work) -> Deployment:
        blocks = [fx.world_view_toml(), _group_toml(), fx.fx_column().toml()]
        blocks += [c.toml() for c in _scoped("Q1")]
        points = _world(ALL)
        for key, seed in KEYS.items():
            points[key.lower()] = (KEY_MEMBERS[key], _scoped(key), seed, GROUP_EXTENT)
        return Deployment(work, Corpus(blocks, points, [MOOD_Q]))

    def live(self, work) -> Deployment:
        d = Deployment(
            work, Corpus([fx.world_view_toml(), fx.fx_column().toml()], _world(ALL), [MOOD_Q])
        )
        group = {
            "projection": "none",
            "extent": _extent_body(GROUP_EXTENT),
            "visibility": "public",
            "point_visibility": {"default": "public"},
            "metadata": [{"name": "label", "type": "text"}],
        }
        d.control("PUT", f"/control/view_groups/{GROUP}", json=group, expect=(201,))
        for key in KEYS:
            self._create(d, key)
        for column in _scoped("Q1"):
            d.control("PUT", "/control/attributes", json=column.payload(), expect=(201,))
        for key in KEYS:
            self._ingest(d, key, "first")
        d.publish()
        if self.recreate:
            d.control("DELETE", f"/control/views/{GROUP}/Q2", expect=(200,))
            d.publish()
            self._create(d, "Q2")
            self._ingest(d, "Q2", "again")
            d.publish()
        return d

    @staticmethod
    def _create(d: Deployment, key: str) -> None:
        d.control(
            "PUT", f"/control/views/{GROUP}/{key}", json={"metadata": {"label": key}},
            expect=(201,),
        )

    @staticmethod
    def _ingest(d: Deployment, key: str, batch: str) -> None:
        rows = fx.point_rows(KEY_MEMBERS[key], _scoped(key), seed=KEYS[key], extent=GROUP_EXTENT)
        d.rows("/control/ingest", rows, f"{key}-{batch}", view=_group_view(key))


def _topic_of(i: int) -> str | None:
    return (None, "root", "a", "b", "a1", "a2", "b1", "a1")[fx._hash("topic", i) % 8]


TOPIC_PARENTS = {"root": None, "a": "root", "b": "root", "a1": "a", "a2": "a", "b1": "b"}
TOPIC_MEMBERS = {key: [i for i in ALL if _topic_of(i) == key] for key in TOPIC_PARENTS}

#: The flat layer's artifacts, chosen so its fraction passes for different principals: `s-a`
#: and `s-mixed` only for `everyone`, `s-bc` not for `few`, `s-c` for all three. Disjoint: a
#: point in two artifacts at one depth names the one with the lower `tessera_id` in its
#: membership column, and the two deployments number artifacts differently.
STRICT_MEMBERS = {
    "s-a": [i for i in range(800) if fx.access_of(i) == "pa"],
    "s-mixed": list(range(800, 1200)),
    "s-bc": [i for i in range(800) if fx.access_of(i) in ("pb", "pc")],
    "s-c": [i for i in range(1200, fx.N_ITEMS) if fx.access_of(i) == "pc"],
}

LAYERS_TOML = """
[[layer]]
name = "topics"
title = "Topics"
source = "topics_roster"
views = ["world"]
membership = "enumerated"
hierarchy = { kind = "nested" }
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"

  [layer.members]
  source = "topics_members"

  [layer.content]
  computed = ["centroid", "box", "hull"]

    [[layer.content.supplied]]
    name = "name"
    type = "text"
    require_member_visibility = "inherited"

[[layer]]
name = "strict"
title = "Strict"
source = "strict_roster"
views = ["world"]
membership = "enumerated"
hierarchy = { kind = "flat" }
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { fraction = 0.6 }

  [layer.members]
  source = "strict_members"
"""


def _layer_body(name: str, title: str, hierarchy: str, criterion, content: dict) -> dict:
    return {
        "name": name,
        "title": title,
        "views": [fx.WORLD],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": None,
        "artifact_visibility": {"field": None, "default": "inherited"},
        "require_member_visibility": criterion,
        "hierarchy": {"kind": hierarchy, "prune_children": False},
        "content": content,
        "depends_on": [],
        "levels": [],
        "layout": None,
        "shape": None,
    }


def _members_table(members: dict[str, list[int]]) -> pa.Table:
    keys = [k for k, ids in members.items() for _ in ids]
    entities = [i for ids in members.values() for i in ids]
    return pa.table({"key": pa.array(keys, pa.string()), "entity": pa.array(entities, pa.uint64())})


def _halves(ids: list[int]) -> tuple[list[str], list[str]]:
    addressed = [fx.external_id(i) for i in ids]
    return addressed[: len(addressed) // 2], addressed[len(addressed) // 2 :]


class Layers(Case):
    """Two layers registered live: a nested one with parents, supplied names and derived
    geometry, and a flat one with a fraction for its membership requirement. Each artifact is
    published with half its members and grown by the other half."""

    name = "layers"
    plan = Plan(layers=("topics", "strict"), items=ITEMS[:4])

    def built(self, work) -> Deployment:
        roster = pa.table(
            {
                "key": pa.array(list(TOPIC_PARENTS), pa.string()),
                "parent": pa.array(list(TOPIC_PARENTS.values()), pa.string()),
                "contents": pa.array(
                    [[[f"Topic {k}"]] for k in TOPIC_PARENTS], pa.list_(pa.list_(pa.string()))
                ),
            }
        )
        extra = {
            "topics_roster": roster,
            "topics_members": _members_table(TOPIC_MEMBERS),
            "strict_roster": pa.table({"key": pa.array(list(STRICT_MEMBERS), pa.string())}),
            "strict_members": _members_table(STRICT_MEMBERS),
        }
        blocks = [fx.world_view_toml(), fx.fx_column().toml(), LAYERS_TOML]
        return Deployment(work, Corpus(blocks, _world(ALL), extra=extra))

    def live(self, work) -> Deployment:
        d = Deployment(work, Corpus([fx.world_view_toml(), fx.fx_column().toml()], _world(ALL)))
        topics = _layer_body(
            "topics", "Topics", "nested", None,
            {
                "computed": ["centroid", "box", "hull"],
                "supplied": [
                    {"name": "name", "type": "text", "require_member_visibility": "inherited"}
                ],
            },
        )
        strict = _layer_body(
            "strict", "Strict", "flat", {"fraction": 0.6}, {"computed": [], "supplied": []}
        )
        for body in (topics, strict):
            d.control("PUT", "/control/layers", json=body, expect=(201,))
        artifacts = [
            {
                "key": key,
                "members": _halves(TOPIC_MEMBERS[key])[0],
                "parent": [parent] if parent else [],
                "content": [{"values": [f"Topic {key}"], "generated_from": []}],
            }
            for key, parent in TOPIC_PARENTS.items()
        ]
        d.control(
            "PUT", "/control/layers/topics/artifacts",
            json={"addressing": "external", "artifacts": artifacts}, expect=(201,),
        )
        strict_first = [{"key": k, "members": _halves(ids)[0]} for k, ids in STRICT_MEMBERS.items()]
        d.control(
            "PUT", "/control/layers/strict/artifacts",
            json={"addressing": "external", "artifacts": strict_first}, expect=(201,),
        )
        for layer, members in (("topics", TOPIC_MEMBERS), ("strict", STRICT_MEMBERS)):
            rest = [{"key": k, "members": _halves(ids)[1]} for k, ids in members.items()]
            d.control(
                "PATCH", f"/control/layers/{layer}/artifacts",
                json={"addressing": "external", "artifacts": rest}, expect=(200,),
            )
        d.publish()
        return d


CASES: dict[str, Case] = {
    case.name: case
    for case in (
        EntityAttributes("entity-attributes", ENTITY_COLUMNS, ENTITY_FILTERS),
        EntityAttributes(
            "text-attributes", TEXT_COLUMNS, (("note", {"note": {"match": "cedar"}}, None),)
        ),
        # An open vocabulary's keys reach the values route before any ingest has minted them.
        EntityAttributes(
            "open-category-values",
            [c for c in ENTITY_COLUMNS if c.name == "grade"],
            values_first=True,
        ),
        EntityAttributes(
            "rendered-attributes",
            RENDERED_COLUMNS,
            (
                ("flag_r", {"flag_r": {"eq": True}}, None),
                ("score_r", {"score_r": {"range": {"lt": 4}}}, None),
            ),
        ),
        LiveVocabularies(),
        PlainView(),
        GroupScoped("group-scoped"),
        GroupScoped("group-view-recreated", recreate=True),
        Layers(),
    )
}

RENDER_REFUSED = (
    "the control plane refuses `render = true` on PUT /control/attributes; a rendered column can "
    "only be declared at a build"
)
VALUES_DO_NOT_MINT = (
    "POST /control/values buffers an open vocabulary's new key unminted, and every flush after "
    "it fails on the column's type, so nothing more is published"
)
TEXT_LOST_AT_RESTART = (
    "a text column declared live matches nothing after a restart until a fold rebuilds its index"
)

GROUP_VIEW_RECREATED = (
    "a group view dropped and recreated loses its scoped families after a fold and a restart; "
    "being fixed on branch fix/recreated-view-restart"
)

#: Cases and stages expected to fail as a whole, why, and the exception they fail with. Strict,
#: so each flips when the cause is fixed.
EXPECTED_FAILURES = {
    **{("rendered-attributes", stage): (RENDER_REFUSED, RuntimeError) for stage in STAGES},
    **{("open-category-values", stage): (VALUES_DO_NOT_MINT, RuntimeError) for stage in STAGES},
}


@dataclass(frozen=True)
class Known:
    """A difference narrow enough to leave out of the comparison and pin on its own: the places
    where some `(label, path)` pair of patterns fully matches the answer's label and the path
    inside it."""

    name: str
    case: str
    stages: tuple[str, ...]
    places: tuple[tuple[str, str], ...]
    reason: str


ANY = r".*"

KNOWN = (
    Known(
        "layer-version",
        "layers",
        ("restart", "fold", "fold-restart"),
        ((r"meta", r"\.layers\[\d+\]\.version"),),
        "a restart moves the version of each layer registered live, with no gate edit",
    ),
    Known(
        "text-index",
        "text-attributes",
        ("restart",),
        ((r"viewport world z\d (filter|highlight) note", ANY),),
        TEXT_LOST_AT_RESTART,
    ),
    Known(
        "recreated-view-families",
        "group-view-recreated",
        ("fold-restart",),
        (
            (r"viewport quarter:Q2 z\d (filter|highlight) (feel|sentiment)", ANY),
            (r"viewport world z\d (filter|highlight) pinned", ANY),
            (r"categories feel@Q2", ANY),
            (r"suggest feel@Q2 .*", ANY),
            (r"meta", r"\.scoped_scalars\[\d+\]\.views.*"),
            (r"item \d+", r"\.body\.scoped\.\w+(\.Q2)?"),
        ),
        GROUP_VIEW_RECREATED,
    ),
)


def _params():
    for name in CASES:
        for stage in STAGES:
            failure = EXPECTED_FAILURES.get((name, stage))
            marks = (
                [pytest.mark.xfail(strict=True, reason=failure[0], raises=failure[1])]
                if failure
                else []
            )
            yield pytest.param(name, stage, None, id=f"{name}-{stage}", marks=marks)
            for n, known in enumerate(KNOWN):
                if known.case == name and stage in known.stages:
                    mark = pytest.mark.xfail(strict=True, reason=known.reason, raises=AssertionError)
                    yield pytest.param(
                        name, stage, n, id=f"{name}-{stage}-{known.name}", marks=[mark]
                    )


# ---------------------------------------------------------------------------------------------
# The walk: both deployments, and the live one taken through the stages in order
# ---------------------------------------------------------------------------------------------


class Walk:
    """The built side, recorded once and stopped, and the live side, advanced stage by stage."""

    def __init__(self, case: Case, work):
        self.case = case
        self.work = work
        built = case.built(work / "built")
        try:
            self.expected = fx.observe(built.server, case.plan)
        finally:
            built.stop()
        _assert_not_vacuous(case, self.expected)
        self.live: Deployment | None = None
        self.stage: str | None = None
        self.found: dict[str, list[fx.Difference]] = {}
        self.broken: Exception | None = None

    def differences(self, stage: str) -> list[fx.Difference]:
        """What the live side answers differently from the built side at `stage`."""
        if stage not in self.found:
            live = self.at(stage)
            self.found[stage] = fx.differences(self.expected, fx.observe(live.server, self.case.plan))
        return self.found[stage]

    def at(self, stage: str) -> Deployment:
        if self.broken is not None:
            raise self.broken
        if self.live is None:
            try:
                self.live = self.case.live(self.work / "live")
            except Exception as refused:
                # Later stages fail the same way rather than build over the half-made one.
                self.broken = refused
                Deployment.stop_all()
                raise
            self.stage = "live"
        order = STAGES.index
        if order(stage) < order(self.stage):
            raise RuntimeError(f"stage {stage} asked for after {self.stage}")
        while self.stage != stage:
            nxt = STAGES[order(self.stage) + 1]
            if nxt in ("restart", "fold-restart"):
                self.live.restart()
            else:
                self.live.fold()
            self.stage = nxt
        return self.live


def _assert_not_vacuous(case: Case, observed: dict) -> None:
    """The built side must show the thing under test, or an equal comparison proves nothing."""
    everyone = observed["everyone"]
    for label, _, views in case.plan.filters:
        for view in views or case.plan.views:
            points = everyone[f"viewport {view} z2 filter {label}"]["points"]
            assert points, f"{case.name}: filter {label} matches nothing on {view}"
    for column in case.plan.categories:
        assert any(p["values"] for p in everyone[f"categories {column}"]), column
    for view in case.plan.views:
        assert everyone[f"viewport {view} z0"]["points"], f"{case.name}: {view} is empty"
    for layer in case.plan.layers:
        assert any(a["layer"] == layer for a in everyone[f"viewport {fx.WORLD} z0"]["artifacts"])
    opened = [everyone[f"item {i}"] for i in case.plan.items]
    assert all(o != "not served" for o in opened), f"{case.name}: an item is not served"


@pytest.fixture(scope="module")
def walks(tmp_path_factory):
    held: dict[str, Walk] = {}

    def get(name: str) -> Walk:
        if name not in held:
            Deployment.stop_all()
            held[name] = Walk(CASES[name], tmp_path_factory.mktemp(name))
        return held[name]

    yield get
    Deployment.stop_all()


@pytest.mark.parametrize(("case", "stage", "known"), list(_params()))
def test_a_live_declaration_serves_what_a_build_serves(walks, case, stage, known):
    found = walks(case).differences(stage)
    covering = [k for k in KNOWN if k.case == case and stage in k.stages]
    if known is None:
        for k in covering:
            found, _ = fx.split(found, k.places)
        shown = "\n".join(map(str, found[:12]))
        assert not found, f"{case} at {stage}: {len(found)} answers differ\n{shown}"
    else:
        k = KNOWN[known]
        _, matching = fx.split(found, k.places)
        shown = "\n".join(map(str, matching[:6]))
        assert not matching, f"{k.reason}\n{shown}"
