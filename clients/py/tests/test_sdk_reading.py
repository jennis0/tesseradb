"""Reading a database: the widget, the principal's terms and the query verbs (python-sdk.md §8).

Against a real served database, because what is under test is that the SDK reads through the
viewer plane as a principal: a fake plane would check the SDK against the SDK's own reading of
the contract, and the one thing these tests are for — that a viewer sees what its terms allow and
no more — is the plane's answer, not Python's.

No browser: the widget's page half is exercised in `clients/ts`, and what is checked here is the
kernel half — where the widget points, what its token source mints, and that neither the token
nor the credential is ever widget state.
"""

from __future__ import annotations

import json
import time

import pytest

from conftest import notebook_corpus  # noqa: F401  (the fixtures below use `corpus`)
from tesseradb import Token, authorise, connect
from tesseradb._refusal import Refusal

from test_sdk_corpus import declare_notebook

pytest.importorskip("pyarrow")
pytest.importorskip("anywidget")

#: A term the notebook corpus's access column holds, so a principal can be minted for it alone.
ONE_TERM = "cs.LG"


@pytest.fixture
def db(served, corpus):
    """The notebook corpus, committed and served, as its own all-terms principal."""
    return served(lambda one: declare_notebook(one, corpus))


def served_count(table) -> int:
    return json.loads(table.schema.metadata[b"tessera.counts"])["served"]


def visible_count(table) -> int:
    return json.loads(table.schema.metadata[b"tessera.counts"])["visible"]


# ---------------------------------------------------------------------------- the widget


def test_map_points_at_the_announced_viewer_with_a_token_that_plane_accepts(db, stub_bundle):
    """`db.map()` is the widget at this database's own address, minting from its credential."""
    m = db.map()
    assert m.url == db.viewer_url
    # The source mints on the page's `ready`, not before, and what it mints the plane accepts.
    minted = m._current_token(renew=False)
    assert isinstance(minted, Token) and minted.token
    assert connect(db.viewer_url, minted.token).meta()["views"][0]["id"] == "s0"


def test_neither_the_token_nor_the_credential_is_widget_state(db, stub_bundle):
    """Client-components §7: the token crosses as a custom message and no traitlet carries it."""
    m = db.map()
    minted = m._current_token(renew=False)
    state = {k: v for k, v in m.get_state().items() if k != "_esm"}
    assert minted.token not in json.dumps(state)
    assert db.session_credential not in json.dumps(state)
    assert not any("token" in name or "credential" in name for name in m.trait_names())


def test_the_map_of_a_named_principal_is_one_call(db, stub_bundle):
    """§8: `viewer(terms).map()` is the same widget as another principal."""
    m = db.viewer([ONE_TERM]).map(view="s0", height=320)
    assert m.url == db.viewer_url and m.view == "s0" and m.height == 320


# ---------------------------------------------------------------------------- terms


def test_a_viewer_sees_what_its_terms_allow_and_not_what_the_operator_sees(db):
    """The union against a subset, through the plane: the mask is the server's, not a filter.

    **I2**: the count a principal is served is computed inside their own mask. So the subset
    principal's `visible` is smaller than the union's and larger than nothing — a filter applied
    to the operator's answer would give the same number by a route that discloses.
    """
    whole = db.view("s0").count()
    subset = db.viewer([ONE_TERM]).view("s0").count()
    assert 0 < subset < whole


def test_a_term_the_database_has_not_inserted_is_minted_and_sees_nothing_of_it(db):
    """Which terms a session may hold is the session plane's, so the SDK mints what it is asked
    for: a term no row carries reaches no row."""
    typo = db.viewer(["cs.LGG"])
    assert typo.view("s0").count() == 0


def test_viewer_refuses_an_empty_term_set(db):
    """A principal holding no term sees nothing, which is the blank map the verb prevents."""
    with pytest.raises(Refusal):
        db.viewer([])


def test_the_union_is_every_access_label_the_sdk_inserted(db):
    """`viewer()` with no terms mints for the distinct labels of every access column inserted."""
    assert ONE_TERM in db.terms
    assert len(db.terms) > 1


# ---------------------------------------------------------------------------- the query verbs


def test_a_sample_carries_the_rendered_columns_and_the_served_count(db):
    """The points surface: `tessera_id`, `code` and what `/v1/meta` declares as rendered."""
    meta = db.meta()
    rendered = {s["name"] for s in meta["declared_scalars"] if s["render"]}
    table = db.view("s0").sample(k=64)
    assert set(table.column_names) == {"tessera_id", "code"} | rendered
    # A served set is not the whole set: `k` bounds it, and the counts are what say by how much.
    assert table.num_rows == served_count(table)
    assert served_count(table) < visible_count(table)
    assert json.loads(table.schema.metadata[b"tessera.trailer"])["points"] == table.num_rows


def test_item_is_the_record_for_a_point_a_sample_served(db):
    """The drill-down: the record by declared column name, and the satisfied terms only."""
    table = db.view("s0").sample(k=8)
    one = table.column("tessera_id")[0].as_py()
    record = db.item(one)
    assert record["fields"]["arxiv_id"]
    # Decision 0114: the item's own labels intersected with this session's satisfied set, never
    # the full set, so a viewer learns no compartment they do not hold.
    assert set(record["labels"]) <= set(db.terms)
    assert [view["id"] for view in record["views"]] == ["s0"]


def test_a_sample_that_serves_no_point_still_has_the_two_fixed_columns(db):
    """A response with no points frame: the table is empty, and its columns are not invented."""
    empty = db.view("s0").filter({"arxiv_id": {"eq": "no-such-paper"}}).sample()
    assert empty.num_rows == 0
    assert empty.column_names == ["tessera_id", "code"]
    assert json.loads(empty.schema.metadata[b"tessera.counts"])["matched"] == 0


def test_the_external_id_comes_back_as_the_column_that_carried_it(db):
    """The wire says base64 bytes; the SDK knows which column those bytes came from.

    The notebook corpus names its rows by an integer column, so the database reads the eight
    little-endian bytes back as that integer. A `connect()` viewer has no declaration to read and
    answers with the bytes.
    """
    table = db.view("s0").sample(k=8)
    one = table.column("tessera_id")[0].as_py()
    carried = db.item(one)["external_id"]
    assert isinstance(carried, int)

    token = authorise(db.session_url, db.session_credential, db.terms)
    raw = connect(db.viewer_url, token).item(one)["external_id"]
    assert isinstance(raw, bytes)
    assert int.from_bytes(raw, "little") == carried


def test_a_string_id_column_comes_back_as_the_string_it_carried(served):
    """The other arm: a database whose rows are named by a string column."""
    from test_sdk_pages import small

    one = served(small)
    table = one.view("map").sample(k=8)
    picked = one.item(table.column("tessera_id")[0].as_py())["external_id"]
    assert isinstance(picked, str) and picked.startswith("p")


# ---------------------------------------------------------------------------- connect()


def test_connect_reads_a_hosted_deployment_with_the_token_it_was_given(db):
    """§10.6: the same three verbs against a deployment somebody else runs."""
    token = authorise(db.session_url, db.session_credential, [ONE_TERM])
    v = connect(db.viewer_url, token)
    assert v.meta()["views"][0]["id"] == "s0"
    table = v.view("s0").sample(k=8)
    assert table.num_rows > 0
    assert v.item(table.column("tessera_id")[0].as_py())["fields"]


def test_connect_has_no_way_to_mint_another_principal_and_no_way_to_write(db):
    """§1, §8: minting needs the session credential and writing needs the operator's."""
    v = connect(db.viewer_url, "a-token-this-test-never-uses")
    assert not hasattr(v, "viewer")
    for verb in ("insert", "declare", "commit", "check", "remove", "suppress", "leave"):
        assert not hasattr(v, verb)


def test_connect_takes_a_string_a_token_or_a_callable(db):
    """As `Map` takes one today (client-components §7)."""
    minted = authorise(db.session_url, db.session_credential, [ONE_TERM])
    for source in (minted.token, minted, lambda: minted):
        assert connect(db.viewer_url, source).meta()["views"][0]["id"] == "s0"


# ---------------------------------------------------------------------------- before a commit


def test_reading_an_uncommitted_database_names_the_commit_that_would_build_it(tmp_path, corpus):
    """There is no server yet, so every read says so rather than failing on a missing key file."""
    from tesseradb._database import create

    db = create(tmp_path / "unbuilt")
    try:
        for read in (
            db.map,
            db.meta,
            lambda: db.view("s0"),
            lambda: db.categories("archive"),
            lambda: db.item(1),
            lambda: db.viewer(["a"]),
        ):
            with pytest.raises(Refusal):
                read()
    finally:
        db.close()


# ---------------------------------------------------------------------------- renewal


def test_a_token_never_prints_itself(db):
    """A repr reaches a saved notebook, a traceback and a log; the token must not be in it."""
    minted = db.token()
    assert minted.token not in repr(minted)
    assert f"{len(db.terms)} term(s)" in repr(minted)


def test_a_token_past_its_expiry_is_renewed_and_the_next_read_answers(db):
    """The session plane's lifetime is an hour, so the clock is faked rather than waited out."""
    v = db.viewer()
    first = v.token()
    v._token = Token(first.token, expires_at=time.time() - 1.0, renew=first.renew)
    assert v.meta()["views"][0]["id"] == "s0"
    assert v.token().expires_at > time.time()


def test_the_widget_answers_reauthorise_with_a_token_the_plane_accepts(db, stub_bundle):
    """The page asks again before expiry; the kernel mints again and sends, never as state."""
    m = db.map()
    sent = []
    m.send = lambda content, buffers=None: sent.append(content)
    m._on_page_message(m, {"type": "ready"}, [])
    m._on_page_message(m, {"type": "reauthorise"}, [])
    assert [message["type"] for message in sent] == ["token", "token"]
    assert m.tokens_sent == 2
    assert connect(db.viewer_url, sent[-1]["token"]).meta()["views"][0]["id"] == "s0"
