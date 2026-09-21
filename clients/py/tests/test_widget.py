"""The kernel half of the widget protocol, against a fake comm.

ipywidgets opens a dummy comm when no kernel is running, so a widget can be built here; what is
faked is the send side (captured) and the page's messages (delivered through the same dispatch
the comm would use), which is the whole surface the protocol has.
"""

import json
import warnings

import pytest
import traitlets

anywidget = pytest.importorskip("anywidget")

from tesseradb import Map, Token  # noqa: E402


@pytest.fixture
def make(monkeypatch, tmp_path):
    """A Map whose bundle is a stub and whose `send` is captured."""
    import tesseradb.widget as widget

    stub = tmp_path / "tessera-components.js"
    stub.write_text("export function render() {}")
    monkeypatch.setattr(widget, "bundle_path", lambda: stub)

    def build(**kw):
        m = Map(kw.pop("url", "http://viewer.test"), **kw)
        sent = []
        monkeypatch.setattr(m, "send", lambda content, buffers=None: sent.append(content))
        m.sent = sent
        return m

    return build


def page_says(m, content):
    m._handle_custom_msg(content, [])


def test_the_token_is_never_model_state(make):
    m = make(token="tok-secret")
    state = m.get_state()
    assert "tok-secret" not in json.dumps({k: v for k, v in state.items() if k != "_esm"})
    assert not any(name.startswith("token") for name in m.trait_names())


def test_the_page_gets_the_bundle_as_synced_esm_and_empty_css(make):
    m = make(token="t")
    state = m.get_state()
    assert state["_esm"].startswith("export function render")
    assert state["_css"] == ""


def test_the_up_traits_start_as_none_not_empty(make):
    m = make(token="t")
    assert m.region is None and m.filters is None and m.selected is None and m.bbox is None


def test_ready_is_answered_with_the_token_as_a_custom_message(make):
    m = make(token="tok-1")
    assert m.sent == []
    page_says(m, {"type": "ready"})
    assert m.sent == [{"type": "token", "token": "tok-1", "expires_at": None}]
    assert m.tokens_sent == 1


def test_reauthorise_renews_a_token_that_can_renew(make):
    calls = []

    def mint(n=[0]):
        n[0] += 1
        calls.append(n[0])
        return Token(f"tok-{n[0]}", 1800000000.0 + n[0], renew=mint)

    m = make(token=mint())
    page_says(m, {"type": "ready"})
    page_says(m, {"type": "reauthorise"})
    assert [s["token"] for s in m.sent] == ["tok-1", "tok-2"]
    assert m.sent[1]["expires_at"] == 1800000002.0


def test_reauthorise_calls_a_callable_source_and_resends_a_fixed_string(make):
    tokens = iter(["a", "b"])
    m = make(token=lambda: next(tokens))
    page_says(m, {"type": "ready"})
    page_says(m, {"type": "reauthorise"})
    assert [s["token"] for s in m.sent] == ["a", "b"]
    fixed = make(token="fixed")
    page_says(fixed, {"type": "ready"})
    page_says(fixed, {"type": "reauthorise"})
    assert [s["token"] for s in fixed.sent] == ["fixed", "fixed"]


def test_a_failing_source_refuses_rather_than_raising_into_the_comm(make):
    def broken():
        raise RuntimeError("idp down")

    m = make(token=broken)
    page_says(m, {"type": "ready"})
    assert m.sent == [{"type": "refused", "detail": "idp down"}]


def test_map_needs_a_token(make):
    with pytest.raises(TypeError):
        make()


def test_ids_are_decimal_strings(make):
    m = make(token="t")
    m.selected = 2**63 + 5
    assert m.selected == "9223372036854775813"
    m.selected_artifact = "7"
    assert m.selected_artifact == "7"
    for bad in ("0x10", -1, 2**64, 1.5, "seven", True):
        with pytest.raises(traitlets.TraitError):
            m.selected = bad
    m.selected = None
    assert m.selected is None


def test_layers_refuse_the_all_keyword_and_tell_none_from_default(make):
    m = make(token="t")
    assert m.layers is None
    with pytest.raises(traitlets.TraitError):
        m.layers = ["all"]
    m.layers = ["clusters/a"]
    assert m.layers == ["clusters/a"]
    m.layers = []
    assert m.layers == []


def test_bbox_is_four_floats_or_none(make):
    m = make(token="t", bbox=(0, 1, 2, 3))
    assert m.bbox == [0.0, 1.0, 2.0, 3.0]
    with pytest.raises(traitlets.TraitError):
        m.bbox = [1, 2]


def test_a_page_error_lands_in_last_error_and_a_warning(make):
    m = make(token="t")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        page_says(m, {"type": "error", "what": "filters", "detail": "none_of"})
    assert m.last_error == "filters: none_of"
    assert any("filters: none_of" in str(w.message) for w in caught)
    assert "last_error" not in m.get_state()


def test_the_synced_surface_is_exactly_the_design_s(make):
    synced = set(Map.class_traits(sync=True)) - set(anywidget.AnyWidget.class_traits(sync=True))
    synced = {k for k in synced if not k.startswith("_")}  # `_esm` and `_css` are anywidget's
    assert synced == {
        "url", "view", "explorer_layout", "height",
        "bbox", "layers", "colour_by", "filters",
        "selected", "selected_artifact", "region",
    }


def test_map_without_a_bundle_says_how_to_get_one(monkeypatch):
    import tesseradb.widget as widget

    monkeypatch.setattr(widget, "bundle_path", lambda: None)
    with pytest.raises(RuntimeError):
        Map("http://viewer.test", token="t")
