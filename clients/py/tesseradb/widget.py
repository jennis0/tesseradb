"""The notebook widget, `Map`: the kernel half of the protocol in `components/src/widget.ts`.

The page half is the components' single-file bundle, which anywidget runs as `_esm`. This half
holds the token, answers the page's `ready` and `reauthorise` messages with it as a custom
message, and mirrors the map's controls and selection as traitlets.

No traitlet carries the token, so nothing that saves widget state saves it: not the front end's
"save widget state", `nbconvert --execute`, papermill, or a run in which no front end mounts. The
token stays in the browser's memory while the page is open.

The page calls `url`, the viewer plane, from the notebook page's origin, so the viewer plane must
allow that origin. A database made with `create()` allows pages served from a loopback address,
through `serve.cors_loopback`; another deployment names the notebook's origin in
`serve.cors_origins`. A VS Code notebook and Colab render in origins no list can name, so the
widget cannot reach a database from them. Not built yet: a Jupyter server extension that would
forward the widget's requests from the notebook's own origin.

Only controls and selection cross the kernel boundary, never data. `url` goes down. `view`,
`bbox`, `layers`, `colour_by`, the four size settings and `filters` go both ways, and up only
when the map settles, once it has finished fetching for a view, so the kernel is never asked on
every frame. `selected` and `selected_artifact` go up on a pick, and `region` when a region's
counts arrive. Ids are decimal strings, because a `tessera_id` is a `u64`, which is not a
JavaScript number, and a `BigInt` does not serialise.
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
    """The interactive map in a notebook cell, reading a Tessera database with a token.

    The page in the browser fetches its data from the database itself, as the token allows. The
    token reaches the page as a message and is never widget state, so saving the notebook does
    not save it.

    - `url`: the address of the database's viewer plane.
    - `token`: required, though the signature gives it a default of `None`. The token your
      deployment issued you, as a string, a `Token` from `login` or `authorise`, which is renewed
      before it expires, or a function returning either, which is called again then. A `Map`
      without one raises `TypeError`.
    - `view`: the view to open on. `None`, the default, opens the first one.
    - `layers`: the annotation layers to draw, each with the layers it depends on. `None`, the
      default, lets the map choose, and `[]` draws none. `"all"` is not a layer name and is
      refused.
    - `colour_by`: the column to colour points by, or `"cluster:<layer>"` to colour them by the
      annotations of that layer.
    - `size_by`: a number column to size points by. `None`, the default, draws every point at
      one size.
    - `size_min`, `size_max`: the radius in pixels of the smallest and the largest value under
      `size_by`. A point with no value draws as a ring. The page ignores a radius that is not a
      number above zero. `None` leaves the map's choice, 2 and 9 until one is made.
    - `size_scale`: how values are placed between the two radii: `"linear"`, `"log"`, or
      `"rank"` among a sample of the values the map has drawn. `None` leaves the map's choice,
      linear until one is made.
    - `filters`: a filter expression, as `Selection.filter` takes one.
    - `bbox`: the box to frame the camera on, as `(min_x, min_y, max_x, max_y)`.
    - `height`: the widget's height in pixels. The default is 480.
    - `explorer_layout`: `"docked"`, the default, puts the controls in a sidebar beside the map;
      `"overlay"` draws the map across the whole widget with the controls floating over it.
    - `title_field`: the field whose value titles a point in the hover and the item card.
      `None`, the default, titles a point by its id.
    - `artifacts_per_tile`: the most annotations each level of a layer shows in one tile of the
      map, largest first, at most the server's `max_artifacts_per_tile`. `None`, the default,
      draws no annotation and leaves colouring by a layer without colours, and the map says what
      to set. The map asks for annotations by tile at map zoom + 2, the zoom rounded down, a
      tile of 128 to 256 pixels, and at most 558 tiles in one request, the most a 3840 by 2160
      screen touches, or the server's `max_tiles_per_request` where that is fewer; a larger
      screen is asked for at zoom + 1, then coarser, until its tiles fit. So the tiles in view
      times `artifacts_per_tile` bounds the annotations one level draws.

    Read the widget's attributes in a later cell:

    - `selected`: the `tessera_id` of the picked item, as a decimal string, or `None`. It
      changes on a pick.
    - `selected_artifact`: the `tessera_id` of the opened annotation, likewise.
    - `region`: the drawn box or lasso, as a dictionary: its `shape` and `status`, the counts
      `visible`, `matched` and `served` inside it, `verdict`, which says whether the counts are
      exact for the shape or over the grid cells covering it, and `refusal` where the server
      refused it. It changes when the region's counts arrive, and is `None` when it is cleared.
    - `view`, `bbox`: the view shown and where the camera settled.
    - `filters`, `layers`, `colour_by`, `size_by`, `size_min`, `size_max`, `size_scale`: as the
      map shows them. Setting one redraws the map.
    - `last_error`: why the page last refused something set here, or `None`.
    - `url`, `height`, `explorer_layout`, `title_field`, `artifacts_per_tile`: as given.
    - `tokens_sent`: how many tokens the kernel has sent the page.

    `view`, `bbox`, `filters`, `layers`, `colour_by` and the size settings change when the map
    settles, once it has finished fetching after a pan or zoom, and not during one. In marimo,
    `mo.ui.anywidget(m)` puts every attribute in one `.value`, so a cell that reads it runs again
    at every settle. To react to a pick alone, call `m.observe(fn, names="selected")` on the `Map`.

    Setting `filters` applies the expression at once. Reading it after the filter panel changes
    gives the expression the panel built. An expression the panel cannot show, such as
    `any_of`, `none_of` or two tests on one column, is refused by the page: nothing is applied,
    and `last_error` and a warning say why.

    The map needs the components' bundle, which `pip install 'tesseradb[widget]'` installs; a
    `Map` made without it raises `RuntimeError` naming what to install.

        m = tesseradb.Map(viewer_url, token=token, colour_by="venue")
        m
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
    # The record field that titles a point in the hover and the item card; `None` titles it by id.
    title_field = traitlets.Unicode(None, allow_none=True).tag(sync=True)
    # The store's quota of annotations per level per tile; `None` draws none.
    artifacts_per_tile = traitlets.Int(None, allow_none=True).tag(sync=True)
    # Both ways, synced up at the settle.
    bbox = traitlets.List(traitlets.Float(), minlen=4, maxlen=4, allow_none=True, default_value=None).tag(sync=True)
    # `None` leaves the explorer's own default; `[]` is none; a list is exactly those (with their
    # dependency closure, which the store adds).
    layers = traitlets.List(traitlets.Unicode(), allow_none=True, default_value=None).tag(sync=True)
    colour_by = traitlets.Unicode(None, allow_none=True).tag(sync=True)
    size_by = traitlets.Unicode(None, allow_none=True).tag(sync=True)
    size_min = traitlets.Float(None, allow_none=True).tag(sync=True)
    size_max = traitlets.Float(None, allow_none=True).tag(sync=True)
    size_scale = traitlets.Enum(["linear", "log", "rank"], default_value=None, allow_none=True).tag(sync=True)
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
        size_by: Optional[str] = None,
        size_min: Optional[float] = None,
        size_max: Optional[float] = None,
        size_scale: Optional[str] = None,
        filters: Optional[dict] = None,
        bbox: Optional[Sequence[float]] = None,
        height: int = 480,
        explorer_layout: str = "docked",
        title_field: Optional[str] = None,
        artifacts_per_tile: Optional[int] = None,
        **kwargs: Any,
    ) -> None:
        if token is None:
            raise TypeError(
                "Map needs a token: the viewer token your deployment issued you, one from "
                "tesseradb.login(url, ...), or one an operator running locally mints with "
                "db.token(terms)."
            )
        bundle = bundle_path()
        if bundle is None:
            raise RuntimeError(
                "tesseradb's bundle is missing. From PyPI: pip install 'tesseradb[widget]' installs "
                "it. From the checkout: pip install -e 'clients/py[widget]' builds it with Node, or "
                "run `npm run bundle -w @tesseradb/components` in clients/ts."
            )
        self._token_source: TokenSource = token
        self._token: Optional[Token] = None
        self.tokens_sent = 0
        super().__init__(
            url=url,
            view=view,
            layers=None if layers is None else list(layers),
            colour_by=colour_by,
            size_by=size_by,
            size_min=size_min,
            size_max=size_max,
            size_scale=size_scale,
            filters=filters,
            bbox=None if bbox is None else [float(v) for v in bbox],
            height=height,
            explorer_layout=explorer_layout,
            title_field=title_field,
            artifacts_per_tile=artifacts_per_tile,
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
