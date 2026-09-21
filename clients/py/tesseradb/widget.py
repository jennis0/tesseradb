"""The notebook widget: ``Map``, the kernel half of the protocol in ``components/src/widget.ts``.

The page half is the components' single-file bundle, which
anywidget evaluates as ``_esm``; this half holds the token, answers the page's ``ready`` and
``reauthorise`` with it as a **custom message**, and mirrors control and selection as traitlets.

**The token is never model state.** No traitlet carries it, so nothing that serialises widget
state can save it — not the frontend's "save widget state", not ``nbconvert --execute``, not
papermill, not a headless run in which no frontend ever mounts. What remains is a token in the
browser's memory for its lifetime, which is browser-direct's exposure everywhere.

**Where the widget's requests go.** The page calls ``url`` — the viewer plane — from the
notebook page's origin, so that origin must be in the viewer plane's CORS list: today the
development-only ``serve.dev_cors_origins``; the production list is design D10, not yet ruled.
⊘ The proxy arm (the widget's base URL a path on the notebook server, the proxy holding the
credential and authorising per principal) is documented in ``clients/py/README.md`` and not built;
D4 chooses which arm ships first. A VS Code notebook and Colab render in origins no list can
name, so they wait on it.

**What crosses the kernel boundary is control and selection, never data.** ``url`` and ``view``
down; ``bbox``, ``layers``, ``colour_by`` and ``filters`` both ways, synced up **at the settle** —
when the map has finished fetching for a view — and never per frame, so the kernel is never on
the pan path; ``selected``, ``selected_artifact`` and ``region`` up. Ids are **decimal strings**:
a ``tessera_id`` is a ``u64``, which is not a JavaScript number, and a ``BigInt`` does not
serialise.
"""

from __future__ import annotations

import pathlib
import warnings
from typing import Any, Optional, Sequence

import anywidget
import traitlets

from ._auth import Token, TokenSource, minted

_HERE = pathlib.Path(__file__).parent
_BUNDLE = "tessera-components.js"


def bundle_path() -> Optional[pathlib.Path]:
    """Where the bundle is: packaged by the wheel's build hook, or the checkout's own build."""
    packaged = _HERE / "static" / _BUNDLE
    if packaged.exists():
        return packaged
    checkout = _HERE.parents[2] / "ts" / "components" / "dist" / _BUNDLE
    if checkout.exists():
        return checkout
    return None


U64_MAX = 2**64 - 1


def _decimal_id(value: Any, name: str) -> Optional[str]:
    """An id as the decimal string of a ``u64``; an ``int`` is accepted and stringified."""
    if value is None:
        return None
    if isinstance(value, bool):
        raise traitlets.TraitError(f"{name} is a tessera_id as a decimal string, not {value!r}")
    if isinstance(value, int):
        if not 0 <= value <= U64_MAX:
            raise traitlets.TraitError(f"{name} {value} is outside u64")
        return str(value)
    if isinstance(value, str) and value.isdecimal() and 0 <= int(value) <= U64_MAX:
        return value
    raise traitlets.TraitError(f"{name} is a tessera_id as a decimal string, not {value!r}")


class Map(anywidget.AnyWidget):
    """The explorer in a notebook cell, against a Tessera viewer plane.

    ``Map(url, token=..., view=None)``: ``url`` is the viewer plane; ``token`` is the viewer token
    your deployment issued you — a string, a ``Token`` from :func:`tesseradb.authorise` (renewed
    on expiry), or a callable returning either (called on expiry). ``view`` names one of the
    served views; ``None`` is the first.

    ``layers`` names the annotation layers to draw (their dependents come with them); ``None``
    leaves the explorer's default and ``[]`` draws none. ``colour_by`` is a column, or
    ``"cluster:<layer>"`` for the served clusters' exact membership.

    Reading the widget in the next cell is the point: ``m.selected`` is the picked item's id,
    ``m.selected_artifact`` the opened artifact's, ``m.region`` the drawn box or lasso with its
    counts, ``m.bbox`` where the camera settled. Setting ``m.filters`` (the wire's expression, e.g.
    ``{"year": {"range": {"gte": 2000}}}`` or ``{"all_of": [...]}``), ``m.layers``, ``m.colour_by``
    or ``m.bbox`` redraws. Nothing is data: the marks stay in the browser.

    Marimo: ``mo.ui.anywidget(m)`` folds every synced trait into one ``.value``, so a cell that
    reads it re-runs at every settle (each pan that finishes fetching). To react to a pick alone,
    ``m.observe(fn, names="selected")`` on this object.

    Reading ``.filters`` after the panel changes it gives the composed expression the store
    applied; setting it applies without the panel's typing debounce. An expression the panel's
    controls cannot hold (``any_of``, ``none_of``, two leaves on a column) is refused by the page
    and reported through ``last_error`` and a warning, and applies nothing.
    """

    # The bundle's text, set per instance from `bundle_path()`; anywidget's frontend reads both.
    _esm = traitlets.Unicode("").tag(sync=True)
    _css = traitlets.Unicode("").tag(sync=True)

    # Down.
    url = traitlets.Unicode().tag(sync=True)
    view = traitlets.Unicode(None, allow_none=True).tag(sync=True)
    # Not `layout`: ipywidgets' DOMWidget already has one, the CSS Layout model the frontend reads.
    explorer_layout = traitlets.Enum(["docked", "overlay"], default_value="docked").tag(sync=True)
    height = traitlets.Int(480).tag(sync=True)
    # Both ways, synced up at the settle.
    bbox = traitlets.List(traitlets.Float(), minlen=4, maxlen=4, allow_none=True, default_value=None).tag(sync=True)
    # `None` leaves the explorer's own default; `[]` is none; a list is exactly those (with their
    # dependency closure, which the store adds).
    layers = traitlets.List(traitlets.Unicode(), allow_none=True, default_value=None).tag(sync=True)
    colour_by = traitlets.Unicode(None, allow_none=True).tag(sync=True)
    filters = traitlets.Dict(default_value=None, allow_none=True).tag(sync=True)
    # Up.
    # `Any` rather than `Unicode` so the validator below runs on an int and stringifies it.
    selected = traitlets.Any(None, allow_none=True).tag(sync=True)
    selected_artifact = traitlets.Any(None, allow_none=True).tag(sync=True)
    region = traitlets.Dict(default_value=None, allow_none=True).tag(sync=True)
    # Kernel-side only: the page's last refusal of something set here (never synced).
    last_error = traitlets.Unicode(None, allow_none=True)

    def __init__(
        self,
        url: str,
        token: Optional[TokenSource] = None,
        *,
        view: Optional[str] = None,
        layers: Optional[Sequence[str]] = None,
        colour_by: Optional[str] = None,
        filters: Optional[dict] = None,
        bbox: Optional[Sequence[float]] = None,
        height: int = 480,
        explorer_layout: str = "docked",
        **kwargs: Any,
    ) -> None:
        if token is None:
            raise TypeError(
                "Map needs a token: the viewer token your deployment issued you. An operator "
                "running locally mints one with tesseradb.authorise(session_url, credential, terms)."
            )
        bundle = bundle_path()
        if bundle is None:
            raise RuntimeError(
                "tesseradb's bundle is missing. From PyPI: pip install 'tesseradb[widget]' installs "
                "it. From the checkout: pip install -e 'clients/py[widget]' builds it with Node, or "
                "run `npm run build -w @tesseradb/components` in clients/ts."
            )
        self._token_source: TokenSource = token
        self._token: Optional[Token] = None
        self.tokens_sent = 0
        super().__init__(
            url=url,
            view=view,
            layers=None if layers is None else list(layers),
            colour_by=colour_by,
            filters=filters,
            bbox=None if bbox is None else [float(v) for v in bbox],
            height=height,
            explorer_layout=explorer_layout,
            _esm=bundle.read_text(encoding="utf-8"),
            **kwargs,
        )
        self.on_msg(self._on_page_message)

    # ---- the token ---------------------------------------------------------------------------

    def _current_token(self, *, renew: bool) -> Token:
        if renew and self._token is not None and self._token.renew is not None:
            self._token = self._token.renew()
            return self._token
        self._token = minted(self._token_source)
        return self._token

    def _token_message(self, *, renew: bool) -> dict:
        token = self._current_token(renew=renew)
        return {"type": "token", "token": token.token, "expires_at": token.expires_at}

    def _on_page_message(self, widget: Any, content: Any, buffers: Any) -> None:
        kind = content.get("type") if isinstance(content, dict) else None
        if kind in ("ready", "reauthorise"):
            try:
                message = self._token_message(renew=kind == "reauthorise")
            except Exception as e:  # a supplier that failed: the page shows `refused`
                self.send({"type": "refused", "detail": str(e)})
                return
            self.tokens_sent += 1
            self.send(message)
        elif kind == "error":
            detail = f"{content.get('what')}: {content.get('detail')}"
            self.last_error = detail
            warnings.warn(f"tesseradb widget refused {detail}", stacklevel=2)

    # ---- validation --------------------------------------------------------------------------

    @traitlets.validate("selected", "selected_artifact")
    def _validate_id(self, proposal: dict) -> Optional[str]:
        return _decimal_id(proposal["value"], proposal["trait"].name)

    @traitlets.validate("layers")
    def _validate_layers(self, proposal: dict) -> Optional[list]:
        if proposal["value"] is None:
            return None
        layers = list(proposal["value"])
        if "all" in layers:
            raise traitlets.TraitError("`all` is not a layer name; name the layers, or set none")
        return layers

    def __repr__(self) -> str:
        return f"Map(url={self.url!r}, view={self.view!r}, layers={self.layers!r})"
