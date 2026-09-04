"""Builds the twelve artboards and canvas.json for the client-components canvas."""
import json
import os
from gen import *  # noqa

OUT = os.path.dirname(os.path.abspath(__file__))


def write(name, html):
    with open(os.path.join(OUT, name), 'w') as f:
        f.write(html)


HULL_LABELS = {0: 'quantum error correction', 1: 'graph neural networks', 2: 'dark matter haloes',
               3: 'topological insulators', 4: 'sparse attention', 5: 'gravitational lensing'}
HULL_COUNTS = {0: '1,962', 1: '1,410', 2: '903', 3: '811', 4: '640', 5: '514'}

ARXIV_CLUSTERS = [
    (0.30, 0.34, 0.07, 0.06, 1.6, 0), (0.64, 0.30, 0.06, 0.05, 1.3, 1), (0.78, 0.66, 0.05, 0.07, 1.0, 5),
    (0.22, 0.70, 0.06, 0.05, 0.9, 4), (0.50, 0.58, 0.05, 0.05, 0.8, 1), (0.88, 0.22, 0.04, 0.04, 0.6, 5),
    (0.44, 0.82, 0.06, 0.04, 0.7, 3), (0.12, 0.20, 0.04, 0.05, 0.5, 2), (0.60, 0.78, 0.04, 0.04, 0.5, 6),
]


SUB_LABELS = {0: [('surface codes', -52, 24), ('bosonic codes', 62, 24)]}
DESCRIPTIONS = {0: 'decoders, thresholds, fault tolerance', 1: 'message passing, attention on graphs', 2: 'N-body, substructure, lensing',
                3: 'band topology, edge states', 4: 'long-context transformers', 5: 'weak lensing, cluster masses'}


def arxiv_map(w, h, seed=7, n=1700, dark=False, selected=None, label_ids=None, r=1.5, boundaries='all',
              sub=True, counts=True, label_bounds=None, font='IBM Plex Sans', descriptions=False):
    """The arXiv corpus as a data map: points over a density wash, a traced boundary for the selected
    cluster, quiet labels. Returns (points, layers)."""
    pts, members, colours = datamap2(w, h, ARXIV_CLUSTERS, seed=seed, n=n, r=r, dark=dark)
    ids = set(label_ids) if label_ids is not None else set(HULL_LABELS)
    labels = {ci: t for ci, t in HULL_LABELS.items() if ci in ids}
    layers = datamap_layers2(w, h, members, colours, labels, counts=HULL_COUNTS if counts else None,
                             sub_labels=SUB_LABELS if sub else None, dark=dark, selected=selected,
                             boundaries=boundaries, label_bounds=label_bounds, font=font,
                             descriptions=DESCRIPTIONS if descriptions else None)
    return pts, layers


def arxiv(w, h, seed=7, n=1500, hull_ids=(0, 1, 2, 3, 4, 5), r=1.6, muted=False):
    cl = [(cx * w, cy * h, sx * w, sy * h, wt, cls) for cx, cy, sx, sy, wt, cls in ARXIV_CLUSTERS]
    return scatter(w, h, seed=seed, n=n, clusters=cl, hull_ids=hull_ids, r=r, muted=muted)


def accordion(title, summary, body=None, open_=False):
    chev = icon('chev', 14) if open_ else icon('chevr', 14)
    head = (f'<div class="hd" style="margin-bottom:{10 if open_ else 0}px"><span class="row" style="gap:6px">{chev}{title}</span>'
            f'<span style="text-transform:none;letter-spacing:0;font-weight:400;color:var(--tessera-ink-3)">{summary}</span></div>')
    return f'<div class="panel">{head}{body if open_ and body else ""}</div>'


# ============================================================================== Main — docked, light
def board_main():
    W, H, SB = 1440, 900, 336
    mw = W - SB
    pts, hl = arxiv_map(mw, H, selected=0, descriptions=True)
    body = f'''
<div class="{{{{themeClass}}}}" style="width:{W}px; height:{H}px; display: flex; flex-direction: row;">
  <div class="tx-map" style="flex-grow: 1; position: relative;" role="application" aria-label="Map of 181,900 items">
    {pts}{hl}
    <div style="position:absolute; left:{int(mw*0.30)+40}px; top:{int(H*0.34)+40}px; width:12px; height:12px; border-radius:50%; border:2px solid var(--tessera-ink); background:transparent"></div>
    {tooltip(int(mw*0.30)+56, int(H*0.34)+28)}
    <div class="corner tl">{mode_control('pan')}</div>
    <div class="corner bl">{status_strip('shown')}</div>
  </div>
  <aside class="sidebar col" style="width:{SB}px; flex: none; overflow: hidden;" aria-label="Explorer panels">
    {toolbar_panel(colour_by='clusters')}
    {layer_panel()}
    {item_card()}
    {accordion('Filters', '2 applied')}
    {accordion('In view', '9 clusters')}
  </aside>
</div>'''
    write('Main.dc.html', dc_file(FONTS_LINK + css_block(), body, THEME_SCRIPT % ('light', 'light')))


# ============================================================================== Overlay — dark by default
def board_overlay():
    W, H = 1440, 900
    pts, hl = arxiv_map(W, H, seed=11, n=2000, dark=True, selected=0, label_bounds=(345, 1100), descriptions=True)
    body = f'''
<div class="{{{{themeClass}}}}" style="width:{W}px; height:{H}px; position: relative;">
  <div class="tx-map" style="position:absolute; inset:0;" role="application" aria-label="Map">
    {pts}{hl}
    <div style="position:absolute; left:12px; top:12px; width:320px; display:flex; flex-direction:column; gap:10px;">
      <div class="float col">{toolbar_panel(colour_by='clusters')}{layer_panel()}{filters_panel(compact=True, show_abstract=False, stack_seg=True, archives=ARCHIVES[:4])}</div>
    </div>
    <div style="position:absolute; right:12px; top:12px; width:320px; display:flex; flex-direction:column; gap:10px; align-items:flex-end;">
      {mode_control('pan')}
      <div class="float col" style="width:100%">{artifact_list_panel(items=CLUSTERS[:5], tree=False)}{artifact_card()}</div>
    </div>
    <div class="corner bl">{status_strip('shown')}</div>
  </div>
</div>'''
    write('ExplorerOverlay.dc.html', dc_file(FONTS_LINK + css_block(), body, THEME_SCRIPT % ('dark', 'dark')))


# ============================================================================== Narrow — two phones
def board_narrow():
    PW, PH = 390, 844
    pts, hl = arxiv_map(PW, PH - 100, seed=5, n=1000, r=1.5, label_ids=(0, 1, 3), sub=False)
    tab = lambda ic, label, on=False: (f'<button style="flex:1; height:56px; display:flex; flex-direction:column; align-items:center; justify-content:center; gap:3px; '
                                       f'color:{"var(--tessera-accent)" if on else "var(--tessera-ink-2)"}; font-size:11px; font-weight:{600 if on else 500}">{icon(ic, 18)}{label}</button>')
    tabs = ('<div class="row" style="gap:0; border-top:1px solid var(--tessera-line); background:var(--tessera-surface); height:56px;" role="tablist">'
            + tab('filter', 'Filters') + tab('layers', 'Layers') + tab('list', 'In view') + tab('info', 'Item') + '</div>')

    def phone(inner):
        return f'<div class="tx col" style="width:{PW}px; height:{PH}px; border:1px solid var(--tessera-line); border-radius:20px; overflow:hidden; position:relative;">{inner}</div>'

    strip = status_strip('shown', narrow=True).replace('class="tx-status"', 'class="tx-status" style="box-shadow:none; border-radius:0; border-width:1px 0 0; width:100%; height:44px; font-size:12px; overflow-x:auto"')
    closed = phone(f'''
      <div class="tx-map grow" style="position:relative;">{pts}{hl}
        <div class="corner tr">{mode_control('pan')}</div>
      </div>
      {strip}{tabs}''')
    sheet_filters = filters_panel(applied=(('archive', 'quant-ph'),), compact=True, show_abstract=False)
    opened = phone(f'''
      <div class="tx-map" style="position:relative; height:300px;">{pts}{hl}
        <div class="corner tr">{mode_control('pan')}</div>
      </div>
      <div class="col grow" style="background:var(--tessera-surface); border-top:1px solid var(--tessera-line); border-radius:12px 12px 0 0; margin-top:-12px; position:relative; box-shadow:var(--tessera-shadow); overflow:hidden;" role="dialog" aria-label="Filters">
        <div style="width:36px; height:4px; border-radius:2px; background:var(--tessera-line); margin:8px auto 0;"></div>
        <div class="grow" style="overflow:hidden">{sheet_filters}</div>
        <div class="row" style="padding:10px 16px; border-top:1px solid var(--tessera-line-2); gap:10px;">
          <button class="btn" style="height:44px; flex:1; justify-content:center;">Clear</button>
          <button class="btn primary" style="height:44px; flex:2; justify-content:center;">Show 12,465 matched</button>
        </div>
      </div>
      {tabs.replace("Filters</button>", "Filters</button>").replace('color:var(--tessera-ink-2); font-size:11px; font-weight:500">' + icon('filter', 18), 'color:var(--tessera-accent); font-size:11px; font-weight:600">' + icon('filter', 18), 1)}''')
    body = f'''
<div class="tx" style="width:860px; height:880px; padding:18px; display:flex; gap:60px; background:var(--tessera-surface-2);">
  <div class="col" style="gap:10px">{closed}<span class="sm muted">Narrow container: the strip stays in view; panels are sheets behind the bar. Targets 44 px.</span></div>
  <div class="col" style="gap:10px">{opened}<span class="sm muted">Filters sheet open. The primary action names the number it will produce.</span></div>
</div>'''
    write('ExplorerNarrow.dc.html', dc_file(FONTS_LINK + css_block(), body))


# ============================================================================== Status states
def board_status():
    rows = [
        ('detached', 'No store and no data. Not "empty", not "refused".'),
        ('session', 'First request: the visible set is being materialised.'),
        ('loading', 'A request is in flight. No numbers.'),
        ('retrying', 'Server busy; retrying after Retry-After. Still no numbers.'),
        ('shown', 'The only state that carries numbers.'),
        ('empty', 'An answer: nothing in this region.'),
        ('refused', 'The refusal and its code. Never rendered as an empty map.'),
        ('expired', 'The token\'s lifetime ended. A way back in.'),
        ('stale', 'The corpus moved: numbers dim, refresh appears.'),
    ]
    lines = ''.join(
        f'<div style="display:grid; grid-template-columns: 110px 1fr; gap: 16px; align-items:center;">'
        f'<div class="mono sm muted">{s}</div><div class="col" style="gap:5px; align-items:flex-start">{status_strip(s)}<span class="xs faint">{d}</span></div></div>'
        for s, d in rows)
    body = f'''
<div class="tx" style="width:960px; height:1200px; padding:28px 32px; display:flex; flex-direction:column; gap:22px;">
  <div class="col" style="gap:4px"><div class="hd">tessera-status · compact</div>
    <span class="sm muted">Every panel renders the same states through the same part. Strip is an aria-live region.</span></div>
  {lines}
  <div class="col" style="gap:8px; margin-top:8px"><div class="hd">tessera-status · expanded (optional), stale</div>
    <div class="float" style="width:336px">{status_expanded(stale=True)}</div></div>
</div>'''
    write('StatusStates.dc.html', dc_file(FONTS_LINK + css_block(), body))



# ============================================================================== Views — the two pickers
def views_toolbar(layout='Quarterly embedding', key=None, label=None, at_end=False, colour_by='clusters', layer='2 of 3 on',
                  single=False):
    """The explorer's toolbar with `<tessera-view-picker>` and `<tessera-key-picker>` above *Colour by*
    (`view-switching.md` §6.3). `single` is the one-view corpus: both pickers render nothing and the
    toolbar is exactly today's."""
    def step(direction, off=False):
        rot = ' style="transform:rotate(180deg);display:inline-flex"' if direction == 'prev' else ' style="display:inline-flex"'
        cls = 'btn off' if off else 'btn'
        return (f'<button class="{cls}" aria-label="{"Previous" if direction == "prev" else "Next"}" '
                f'style="width:30px;padding:0;display:inline-flex;align-items:center;justify-content:center;flex:none">'
                f'<span{rot}>{icon("chevr", 14)}</span></button>')
    rows = ''
    if not single:
        rows += (f'<div class="col" style="gap:4px;margin-bottom:10px"><span class="xs muted">View</span>'
                 f'<div class="select">{layout}{icon("chev", 14)}</div></div>')
    if key is not None:
        text = f'{label}<span class="mono xs muted" style="margin-left:8px">{key}</span>' if label else key
        rows += (f'<div class="col" style="gap:4px;margin-bottom:10px"><span class="xs muted">quarter</span>'
                 f'<div class="row" style="gap:6px">{step("prev")}'
                 f'<div class="select grow" style="flex-grow:1">{text}{icon("chev", 14)}</div>{step("next", off=at_end)}</div></div>')
    rows += (f'<div class="row" style="gap:10px">'
             f'<div class="col grow" style="gap:4px"><span class="xs muted">Colour by</span>'
             f'<div class="select">{colour_by}{icon("chev", 14)}</div></div>'
             f'<div class="col grow" style="gap:4px"><span class="xs muted">Layers</span>'
             f'<div class="select">{layer}{icon("chev", 14)}</div></div></div>')
    return f'<div class="panel">{rows}</div>'


def view_menu(entries, current):
    """The view picker's open list: plain views, then the groups, in `/v1/meta`'s serving order."""
    items = ''.join(
        f'<div class="row" style="height:28px;padding:0 10px;gap:8px;border-radius:3px;'
        f'{"background:var(--tessera-surface-2);font-weight:500" if e == current else ""}">'
        f'<span style="width:14px;display:inline-flex">{icon("check", 12, sw=2) if e == current else ""}</span>{e}</div>'
        for e in entries)
    return (f'<div class="float" style="width:300px;padding:6px;border:1px solid var(--tessera-line);'
            f'border-radius:var(--tessera-radius);background:var(--tessera-surface)">{items}</div>')


def item_views_chips(views, current):
    chips = ''.join(
        f'<span class="chip" style="{"background:var(--tessera-accent);color:var(--tessera-accent-ink)" if v == current else ""}">{v}</span>'
        for v in views)
    return (f'<div class="panel"><div class="hd">Item</div>'
            f'<div style="font-weight:500;margin-bottom:8px">Decoding surface codes with sparse attention transformers</div>'
            f'<span class="xs muted">In views</span>'
            f'<div class="row" style="flex-wrap:wrap;gap:6px;margin:4px 0 10px">{chips}</div>'
            f'<div class="row" style="gap:8px"><span class="chip">quant-ph</span><span class="chip">2024</span></div></div>')


def board_views():
    col = 'display:flex;flex-direction:column;gap:10px;width:336px'
    case = lambda title, note, body: (f'<div class="col" style="gap:8px;{col}"><div class="hd">{title}</div>'
                                      f'<span class="xs faint" style="min-height:32px">{note}</span>{body}</div>')
    body = f'''
<div class="tx" style="width:1160px; height:660px; padding:28px 32px; display:flex; flex-direction:column; gap:22px;">
  <div class="col" style="gap:4px"><div class="hd">tessera-view-picker · tessera-key-picker</div>
    <span class="sm muted">Two selects at the top of the toolbar slot. The first chooses the layout — a plain view or a group; the second walks the group's roster in creation order, previous and next beside it. Neither draws for a one-view corpus.</span></div>
  <div class="row" style="gap:28px; align-items:flex-start">
    {case('A group, mid-roster', 'Label from roster metadata (a text <span class="mono">label</span>, else <span class="mono">starts</span> as a date), the key muted after it.',
          views_toolbar(layout='Quarterly embedding', key='2026-Q3', label='Jul–Sep 2026'))}
    {case('A group, at the end', 'Next is disabled; the roster never wraps.',
          views_toolbar(layout='Quarterly map', key='2026-Q4', label='Oct–Dec 2026', at_end=True))}
    {case('Two plain views', 'The arXiv rung. No group, so no key picker.',
          views_toolbar(layout='knn'))}
  </div>
  <div class="row" style="gap:28px; align-items:flex-start">
    {case('One view', 'Every demo corpus today. Nothing is drawn; the toolbar is unchanged.',
          views_toolbar(single=True))}
    {case('The view picker, open', 'Plain views first, then the groups — an owner group and the map laid over its keys are two entries. Choosing the other keeps the key.',
          view_menu(['World', 'World (flat)', 'Quarterly embedding', 'Quarterly map'], 'Quarterly embedding'))}
    {case('The item card follows', 'Every view the session may reach holds this item; the current one is marked, another is one click.',
          item_views_chips(['knn', 'pca64'], 'knn'))}
  </div>
</div>'''
    write('Views.dc.html', dc_file(FONTS_LINK + css_block(), body))


# ============================================================================== Selection flow
def board_selection():
    FW, FH = 400, 400
    pts, _unused = arxiv_map(FW, FH, seed=21, n=900, r=1.5, boundaries=False, label_ids=(), sub=False)
    path = [(120, 130), (170, 96), (250, 90), (300, 140), (290, 230), (240, 290), (160, 280), (110, 210)]
    live = lasso_layer(FW, FH, path, live=True)
    snap = lasso_layer(FW, FH, path, live=False, cell=14)

    def frame(overlay, corner_ctl, extra='', strip=None):
        strip_html = ('<div class="corner bl">' + strip + '</div>') if strip else ''
        return (f'<div class="tx-map" style="width:{FW}px; height:{FH}px; position:relative; border:1px solid var(--tessera-line); border-radius:var(--tessera-radius);">'
                f'{pts}{overlay}<div class="corner tl">{corner_ctl}</div>{extra}{strip_html}</div>')

    f1 = frame(live, mode_control('lasso', fit=False),
               f'<div class="tip" style="left:240px; top:250px; padding:4px 8px">release to count</div>')
    f2 = frame(snap, mode_control('lasso', fit=False),
               strip=status_strip('loading').replace('Loading', 'Counting'))
    f3 = (f'<div class="row" style="align-items:stretch; gap:0">{frame(snap, mode_control("pan", fit=False), strip=status_strip("shown"))}'
          f'<div class="float" style="width:300px; margin-left:16px; background:var(--tessera-surface)">{selection_panel()}</div></div>')
    cap = lambda t: f'<div class="sm muted" style="max-width:{FW}px; margin-top:10px; text-wrap:pretty">{t}</div>'
    body = f'''
<div class="tx" style="width:1720px; height:620px; padding:28px 32px; display:flex; flex-direction:column; gap:14px;">
  <div class="hd">Lasso selection</div>
  <div style="display:flex; gap:40px; align-items:flex-start;">
    <div class="col">{f1}{cap('1 · Dragging. The live path is client-side and free.')}</div>
    <div class="col">{f2}{cap('2 · Released. One counting request, at pixel resolution; the shape stays yours.')}</div>
    <div class="col">{f3}{cap('3 · Counted. Both numbers, always. The greyed actions wait on server verbs; hover says which.')}</div>
  </div>
</div>'''
    write('SelectionFlow.dc.html', dc_file(FONTS_LINK + css_block(), body))


# ============================================================================== Host 1 — ops console (geo)
def board_ops():
    W, H = 1440, 900
    TOP, LEFT, RIGHT = 52, 264, 328
    mw, mh = W - LEFT - RIGHT, H - TOP
    sev_palette = ['#f2a93b', '#e0703a', '#c8352f', '#8fb0c8', '#5d7a8e', '#f2a93b', '#e0703a', '#5d7a8e']
    clusters = [(0.30, 0.50, 0.05, 0.04, 1.2, 0), (0.42, 0.62, 0.04, 0.04, 1.0, 1), (0.55, 0.40, 0.05, 0.05, 1.4, 3),
                (0.68, 0.70, 0.04, 0.03, 0.7, 2), (0.20, 0.30, 0.03, 0.03, 0.5, 4), (0.85, 0.22, 0.03, 0.03, 0.6, 1)]
    cl = [(cx * mw, cy * mh, sx * mw, sy * mh, wt, cls) for cx, cy, sx, sy, wt, cls in clusters]
    pts, hulls = scatter(mw, mh, seed=31, n=700, clusters=cl, palette=sev_palette, r=2.2, hull_ids=())
    # district polygons: supplied shapes, drawn as boundaries with the masked count
    districts = (f'<svg viewBox="0 0 {mw} {mh}" width="{mw}" height="{mh}" xmlns="http://www.w3.org/2000/svg" style="position:absolute; inset:0; width:100%; height:100%; pointer-events:none;">'
                 f'<g fill="none" stroke="#f2a93b" stroke-opacity="0.55" stroke-width="1.2" stroke-dasharray="6 4">'
                 f'<path d="M{mw*0.18:.0f} {mh*0.30:.0f} L{mw*0.40:.0f} {mh*0.26:.0f} L{mw*0.48:.0f} {mh*0.52:.0f} L{mw*0.36:.0f} {mh*0.72:.0f} L{mw*0.16:.0f} {mh*0.62:.0f} Z"/>'
                 f'<path d="M{mw*0.48:.0f} {mh*0.52:.0f} L{mw*0.40:.0f} {mh*0.26:.0f} L{mw*0.66:.0f} {mh*0.22:.0f} L{mw*0.76:.0f} {mh*0.48:.0f} L{mw*0.62:.0f} {mh*0.60:.0f} Z"/>'
                 f'<path d="M{mw*0.62:.0f} {mh*0.60:.0f} L{mw*0.76:.0f} {mh*0.48:.0f} L{mw*0.86:.0f} {mh*0.78:.0f} L{mw*0.58:.0f} {mh*0.86:.0f} L{mw*0.36:.0f} {mh*0.72:.0f} L{mw*0.48:.0f} {mh*0.52:.0f} Z"/></g>'
                 f'<g font-family="Barlow, sans-serif" font-size="12" font-weight="600" fill="#f2a93b" letter-spacing="0.06">'
                 f'<text x="{mw*0.30:.0f}" y="{mh*0.44:.0f}">DISTRICT 7 · 318</text><text x="{mw*0.60:.0f}" y="{mh*0.36:.0f}">DISTRICT 4 · 612</text>'
                 f'<text x="{mw*0.66:.0f}" y="{mh*0.74:.0f}">DISTRICT 9 · 274</text></g></svg>')
    tokens = """
      --tessera-surface: #0e1418; --tessera-surface-2: #131a1f; --tessera-surface-3: #1c252c;
      --tessera-ink: #dfe7ec; --tessera-ink-2: #a3b1bb; --tessera-ink-3: #7d8b96;
      --tessera-line: #243039; --tessera-line-2: #1c262d;
      --tessera-accent: #f2a93b; --tessera-accent-ink: #14100a; --tessera-accent-soft: #2b2416;
      --tessera-warn: #f2a93b; --tessera-warn-soft: #2b2416; --tessera-refuse: #ff8a80; --tessera-refuse-soft: #3a1c1a; --tessera-ok: #7ed3a3;
      --tessera-map-bg: #0b1620; --tessera-map-grid: #0b1620;
      --tessera-radius: 2px; --tessera-font: 'Barlow', system-ui, sans-serif; --tessera-font-mono: 'JetBrains Mono', ui-monospace, monospace;
      --tessera-shadow: 0 1px 0 rgba(0,0,0,0.6), 0 8px 24px rgba(0,0,0,0.5); --tessera-map-height: 420px;
    """
    fonts = '<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Barlow:wght@400;500;600;700&amp;family=JetBrains+Mono:wght@400;500&amp;display=swap">'
    extra = """
      .ops .hd { letter-spacing: 0.12em; font-size: 10.5px; color: #7d8b96; }
      .ops .tx-status { border-radius: 0; height: 32px; }
      .ops .check .bx { border-radius: 0; }
      .ops .btn { border-radius: 0; text-transform: uppercase; letter-spacing: 0.06em; font-size: 11.5px; font-weight: 600; }
      .ops .card-title { font-size: 16px; font-weight: 700; letter-spacing: 0.01em; }
      .ops .top { height: 52px; border-bottom: 1px solid var(--tessera-line); display: flex; align-items: center; padding: 0 16px; gap: 20px; background: #0a0f13; }
      .ops .wordmark { font-weight: 700; letter-spacing: 0.22em; font-size: 13px; color: #f2a93b; }
    """
    facet = (f'<div class="panel"><div class="hd">Type</div><div class="col">'
             + ''.join(f'<div class="check"><span class="bx {"on" if on else ""}">{icon("check", 11, sw=2.2) if on else ""}</span>{t}</div>'
                       for t, on in [('Flooding', True), ('Road blocked', True), ('Medical', False), ('Shelter', True), ('Power', False)])
             + '</div></div>'
             f'<div class="panel"><div class="hd">Severity</div><div class="seg" style="height:32px">'
             + ''.join(f'<button class="{"on" if i >= 3 else ""}" style="width:38px">{i}</button>' for i in range(1, 6)) + '</div></div>'
             f'<div class="panel"><div class="hd">Reporting unit</div><div class="select">All units{icon("chev", 14)}</div></div>'
             f'<div class="panel"><div class="hd">Reported</div><div class="row"><div class="input grow mono sm">06-19 00:00</div><span class="faint">→</span><div class="input grow mono sm">now</div></div></div>')
    inc_card = (f'<div class="panel"><div class="hd">Incident <span class="mono" style="text-transform:none;letter-spacing:0;color:var(--tessera-ink-3)">INC-20481</span></div>'
                f'<div class="card-title" style="margin-bottom:10px">Road blocked — Ridge Rd at km 14</div>'
                f'<div class="field"><div class="k">severity</div><div class="v"><span class="sw" style="background:#e0703a;display:inline-block;margin-right:6px"></span>4 · high</div>'
                f'<div class="k">reported</div><div class="v mono sm">06-21 14:32</div><div class="k">unit</div><div class="v">Coastal team B</div>'
                f'<div class="k">district</div><div class="v">District 7</div><div class="k">status</div><div class="v">Unassigned</div></div>'
                f'<div class="row" style="margin-top:14px"><button class="btn primary">Assign</button><button class="btn">Open log</button></div></div>')
    districts_list = artifact_list_panel(items=[('District 4', 612), ('District 7', 318), ('District 9', 274)], selected=1, tree=False, title='Districts in view').replace('clusters', 'districts')
    body = f'''
<div class="tx ops col" style="width:{W}px; height:{H}px;">
  <div class="top"><span class="wordmark">FIELDLINE</span>
    <div class="select" style="height:30px; background:transparent">Sector 4 — Coastal{icon('chev', 14)}</div>
    <span class="sm muted">Incidents · live</span>
    <span class="grow"></span>
    <span class="sm muted row" style="gap:6px">{icon('lock', 14)}Viewing as J. Okafor · Sector 4 clearance</span>
    <span style="width:28px;height:28px;border-radius:50%;background:var(--tessera-surface-3);display:grid;place-items:center">{icon('user', 14)}</span>
  </div>
  <div class="row grow" style="gap:0; align-items:stretch; min-height:0;">
    <aside class="col" style="width:{LEFT}px; flex:none; border-right:1px solid var(--tessera-line); overflow:hidden;">{facet}</aside>
    <div class="tx-map grow" style="position:relative;">
      {basemap(mw, mh)}{pts}{districts}
      <div class="corner tr">{mode_control('pan')}</div>
      <div class="corner bl">{status_strip('shown', shown=318, matched=318, visible=1204)}</div>
      <div class="corner br">{legend_float('severity', [('1–2', '#8fb0c8'), ('3', '#f2a93b'), ('4', '#e0703a'), ('5', '#c8352f')])}</div>
    </div>
    <aside class="col sidebar" style="width:{RIGHT}px; flex:none;">{inc_card}{districts_list}
      <div class="panel"><div class="hd">Access</div><p class="sm muted" style="margin:0">Everything here is computed from the 1,204 reports you are cleared for.</p></div>
    </aside>
  </div>
</div>'''
    write('OpsConsole.dc.html', dc_file(fonts + css_block(theme_vars=tokens, extra=extra), body))


# ============================================================================== Host 2 — image dataset curator
def board_images():
    W, H = 1440, 900
    NAV, TOP = 224, 60
    mw, mh = 760, H - TOP - 56
    palette = ['#0f766e', '#c2410c', '#6d28d9', '#b45309', '#0369a1', '#be185d', '#4d7c0f', '#9ca3af']
    classes = ['cyclist', 'pedestrian', 'car', 'bus', 'traffic light', 'sign', 'tram', 'other']
    clusters = [(0.28, 0.36, 0.07, 0.07, 1.5, 0), (0.62, 0.30, 0.07, 0.06, 1.3, 1), (0.74, 0.68, 0.06, 0.06, 1.2, 2),
                (0.34, 0.74, 0.06, 0.05, 0.9, 3), (0.52, 0.56, 0.04, 0.04, 0.6, 4), (0.86, 0.26, 0.03, 0.05, 0.5, 5), (0.14, 0.62, 0.04, 0.04, 0.5, 6)]
    cl = [(cx * mw, cy * mh, sx * mw, sy * mh, wt, cls) for cx, cy, sx, sy, wt, cls in clusters]
    pts, _ = scatter(mw, mh, seed=41, n=1600, clusters=cl, palette=palette, r=1.7)
    path = [(150, 190), (230, 150), (330, 170), (360, 260), (320, 350), (210, 370), (140, 300)]
    snap = lasso_layer(mw, mh, path, live=False, cell=14, color='#0f766e')
    thumb = lambda w, h, hue, label='': (f'<div style="width:{w}px;height:{h}px;border-radius:6px;background:repeating-linear-gradient(135deg,{hue} 0 6px,transparent 6px 12px),#e5e7eb;'
                                        f'display:grid;place-items:center;color:#6b7280;flex:none;position:relative" aria-label="image placeholder">{icon("image", 18, stroke="#6b7280")}'
                                        + (f'<span style="position:absolute;left:6px;bottom:4px;font-size:10px;color:#374151;font-family:DM Sans">{label}</span>' if label else '') + '</div>')
    tokens = """
      --tessera-surface: #ffffff; --tessera-surface-2: #f8fafc; --tessera-surface-3: #eef2f6;
      --tessera-ink: #111827; --tessera-ink-2: #4b5563; --tessera-ink-3: #6b7280;
      --tessera-line: #e2e8f0; --tessera-line-2: #eef2f6;
      --tessera-accent: #0f766e; --tessera-accent-ink: #ffffff; --tessera-accent-soft: #e6f4f1;
      --tessera-warn: #92400e; --tessera-warn-soft: #fef3c7; --tessera-refuse: #b91c1c; --tessera-refuse-soft: #fee2e2; --tessera-ok: #15803d;
      --tessera-map-bg: #f8fafc; --tessera-map-grid: #eef2f6;
      --tessera-radius: 8px; --tessera-font: 'DM Sans', system-ui, sans-serif; --tessera-font-mono: 'DM Mono', ui-monospace, monospace;
      --tessera-shadow: 0 1px 2px rgba(15,23,42,0.06), 0 8px 24px rgba(15,23,42,0.08); --tessera-map-height: 420px;
    """
    fonts = '<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=DM+Sans:wght@400;500;600;700&amp;family=DM+Mono:wght@400;500&amp;display=swap">'
    extra = """
      .lf .hd { text-transform: none; letter-spacing: 0; font-size: 13px; font-weight: 600; color: #111827; }
      .lf .btn { border-radius: 8px; height: 34px; }
      .lf .btn.primary { background: #0f766e; }
      .lf .tx-status { border-radius: 999px; padding: 0 4px; }
      .lf .tx-status > * + * { border-left: 0; }
      .lf .ctl { border-radius: 10px; }
      .lf .nav a { display:flex; align-items:center; gap:10px; height:36px; padding:0 12px; border-radius:8px; color:#374151; text-decoration:none; font-weight:500; }
      .lf .nav a.on { background:#e6f4f1; color:#0f766e; }
      .lf .tab { height: 60px; display:flex; align-items:center; padding:0 4px; margin-right:20px; border-bottom:2px solid transparent; color:#6b7280; font-weight:500; }
      .lf .tab.on { color:#111827; border-color:#0f766e; }
    """
    nav = (f'<nav class="nav col" style="width:{NAV}px; flex:none; border-right:1px solid var(--tessera-line); padding:16px 12px; gap:4px; background:#fbfcfd">'
           f'<div style="font-weight:700; font-size:15px; padding:4px 12px 16px; letter-spacing:-0.01em">Lensfold</div>'
           f'<a href="#">{icon("grid", 16)}Datasets</a><a href="#" class="on">{icon("image", 16)}street-scenes-v3</a><a href="#">{icon("play", 16)}Runs</a><a href="#">{icon("tag", 16)}Labels</a><a href="#">{icon("list", 16)}Label queues</a>'
           f'</nav>')
    header = (f'<div class="row" style="height:{TOP}px; border-bottom:1px solid var(--tessera-line); padding:0 24px; gap:24px; flex:none">'
              f'<div class="col" style="gap:0"><span style="font-weight:600; font-size:15px">street-scenes-v3</span><span class="xs muted">1,204,311 images · embeddings v9</span></div>'
              f'<div class="row grow" style="gap:0; margin-left:12px"><span class="tab">Overview</span><span class="tab on">Embedding map</span><span class="tab">Labels</span><span class="tab">Quality</span></div>'
              f'<button class="btn" aria-label="More">{icon("dots", 14)}</button></div>')
    legend_rows = ''.join(f'<div class="row" style="height:26px"><span class="sw" style="background:{c};border-radius:50%"></span><span class="grow">{v}</span></div>'
                          for v, c in zip(classes, palette))
    legend = (f'<div class="panel"><div class="hd">Predicted class <div class="select" style="height:28px;border-radius:8px">predicted_class{icon("chev", 14)}</div></div>'
              f'<div class="col">{legend_rows}</div></div>')
    strip_thumbs = ''.join(thumb(64, 48, c) for c in ['#0f766e55', '#c2410c55', '#0f766e55', '#6d28d955', '#0f766e55', '#b4530955'])
    selection = (f'<div class="panel"><div class="hd">Selection <span class="faint" style="font-weight:400">lasso</span></div>'
                 f'<div class="row" style="gap:18px;margin-bottom:10px"><div class="col"><span class="num" style="font-size:22px;font-weight:600">312</span><span class="xs muted">shown inside</span></div>'
                 f'<div class="col"><span class="num" style="font-size:22px;font-weight:600">2,114</span><span class="xs muted">matched inside</span></div>'
                 f'<div class="col"><span class="num" style="font-size:22px;font-weight:600">2,114</span><span class="xs muted">visible inside</span></div></div>'
                 f'<div class="row" style="gap:6px;overflow:hidden;margin-bottom:12px">{strip_thumbs}</div>'
                 f'<button class="btn primary" style="width:100%;justify-content:center">Queue 312 shown for labelling</button>'
                 f'<p class="xs faint" style="margin:8px 0 0" title="312 is the sample on screen; queueing all 2,114 needs the export verb, not yet served.">312 of 2,114 — the sample on screen</p></div>')
    item = (f'<div class="panel"><div class="hd">frame_0812.jpg <button class="faint" aria-label="Close">{icon("close", 14)}</button></div>'
            f'<div class="row" style="align-items:flex-start;gap:12px">{thumb(120, 90, "#0f766e55", "")}'
            f'<div class="field" style="grid-template-columns:82px 1fr"><div class="k">predicted</div><div class="v">cyclist · 0.81</div><div class="k">label</div><div class="v muted">none yet</div>'
            f'<div class="k">captured</div><div class="v mono sm">2025-11-03</div><div class="k">camera</div><div class="v">front-left</div></div></div>'
            f'<div class="row" style="margin-top:12px;gap:6px;flex-wrap:wrap"><button class="btn">cyclist</button><button class="btn">pedestrian</button><button class="btn">other</button><button class="btn" style="border:0;color:var(--tessera-ink-2)">Skip</button></div></div>')
    tip = (f'<div class="tip" style="left:{int(mw*0.28)+18}px; top:{int(mh*0.36)-30}px; padding:6px; display:flex; gap:8px; align-items:center; border-radius:10px">'
           f'{thumb(88, 66, "#0f766e55")}<div class="col" style="gap:2px"><span class="mono xs">frame_0812.jpg</span><span class="xs muted">cyclist · 0.81</span></div></div>')
    body = f'''
<div class="tx lf row" style="width:{W}px; height:{H}px; gap:0; align-items:stretch;">
  {nav}
  <div class="col grow" style="min-width:0">
    {header}
    <div class="row grow" style="gap:20px; padding:20px 24px; align-items:stretch; min-height:0">
      <div class="col" style="gap:10px; width:{mw}px; flex:none">
        <div class="tx-map" style="position:relative; height:{mh}px; border:1px solid var(--tessera-line); border-radius:12px;">{pts}{snap}{tip}
          <div class="corner tl">{mode_control('lasso')}</div>
          <div class="corner bl">{status_strip('shown', shown=4020, matched=61233, visible=1204311)}</div>
        </div>
      </div>
      <div class="col grow float" style="min-width:0; overflow:hidden">{legend}{selection}{item}</div>
    </div>
  </div>
</div>'''
    write('ImageCurator.dc.html', dc_file(fonts + css_block(theme_vars=tokens, extra=extra), body))


# ============================================================================== Host 3 — document research desk
def board_research():
    W, H = 1440, 900
    TOP, LEFT, RIGHT = 64 + 44, 352, 380
    mw, mh = W - LEFT - RIGHT, H - TOP - 44
    pts, members, colours = datamap2(mw, mh, ARXIV_CLUSTERS, seed=55, n=1500, r=1.5, wash=False)
    dept = ['#7a1f2b', '#3b6d8c']
    for i, c in enumerate(colours):
        pts = pts.replace(f'fill="{c}"', f'fill="{dept[i % 2]}"')
    hl = datamap_layers2(mw, mh, members, ['#6b5e55'] * len(colours), {k: v for k, v in HULL_LABELS.items() if k in (0, 1, 2)}, counts=HULL_COUNTS, font='Source Sans 3', boundaries='none', ink='#3a322c', halo='rgba(246,242,234,0.8)')
    tokens = """
      --tessera-surface: #fbf8f2; --tessera-surface-2: #f4efe5; --tessera-surface-3: #e9e2d4;
      --tessera-ink: #2a2622; --tessera-ink-2: #625a50; --tessera-ink-3: #7d7468;
      --tessera-line: #d8cfbf; --tessera-line-2: #e8e0d1;
      --tessera-accent: #7a1f2b; --tessera-accent-ink: #ffffff; --tessera-accent-soft: #f3e3e3;
      --tessera-warn: #7a5600; --tessera-warn-soft: #f9edc9; --tessera-refuse: #8f2222; --tessera-refuse-soft: #f7dede; --tessera-ok: #2f6b3f;
      --tessera-map-bg: #f6f2ea; --tessera-map-grid: #ebe4d6;
      --tessera-radius: 3px; --tessera-font: 'Source Sans 3', system-ui, sans-serif; --tessera-font-mono: 'Source Code Pro', ui-monospace, monospace;
      --tessera-shadow: 0 1px 2px rgba(42,38,34,0.08), 0 6px 18px rgba(42,38,34,0.08); --tessera-map-height: 420px;
    """
    fonts = ('<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Source+Serif+4:wght@500;600&amp;family=Source+Sans+3:wght@400;500;600'
             '&amp;family=Source+Code+Pro:wght@400;500&amp;display=swap">')
    extra = """
      .md .serif { font-family: 'Source Serif 4', Georgia, serif; }
      .md .hd { letter-spacing: 0.04em; }
      .md .card-title { font-family: 'Source Serif 4', Georgia, serif; font-weight: 600; font-size: 17px; }
      .md .result { padding: 10px 16px; border-bottom: 1px solid var(--tessera-line-2); display:flex; flex-direction:column; gap:3px; }
      .md .result.on { background: var(--tessera-accent-soft); }
      .md .result .t { font-family: 'Source Serif 4', Georgia, serif; font-weight: 500; font-size: 14px; line-height: 1.35; }
      .md .tab { padding: 0 2px; height: 44px; display:flex; align-items:center; margin-right: 18px; border-bottom: 2px solid transparent; color: var(--tessera-ink-2); }
      .md .tab.on { color: var(--tessera-ink); border-color: var(--tessera-accent); font-weight: 600; }
      .md .facet { height: 30px; padding: 0 10px; border: 1px solid var(--tessera-line); border-radius: 999px; display:inline-flex; align-items:center; gap:6px; background: var(--tessera-surface); }
      .md .facet.on { background: var(--tessera-accent-soft); border-color: transparent; color: var(--tessera-accent); font-weight: 500; }
    """
    results = [('Decoding surface codes with sparse attention transformers', 'Policy · 2024'),
               ('Entanglement distribution across metropolitan fibre: a regulatory view', 'Legal · 2023'),
               ('Threshold estimates for bosonic cat codes under biased noise', 'Policy · 2024'),
               ('Standards for quantum key distribution procurement', 'Legal · 2022'),
               ('A graph-state compiler for photonic cluster states', 'Policy · 2023'),
               ('Export controls and entanglement sources: a survey', 'Legal · 2021'),
               ('Leakage-aware syndrome extraction on heavy-hex lattices', 'Policy · 2024'),
               ('Liability in distributed quantum sensing networks', 'Legal · 2023')]
    res_rows = ''.join(f'<div class="result {"on" if i == 0 else ""}"><span class="t">{t}</span><span class="xs muted">{m}</span></div>' for i, (t, m) in enumerate(results))
    left = (f'<aside class="col" style="width:{LEFT}px; flex:none; border-right:1px solid var(--tessera-line); background:var(--tessera-surface); overflow:hidden">'
            f'<div class="row" style="padding:12px 16px; border-bottom:1px solid var(--tessera-line-2)"><span class="num" style="font-weight:600">12,465</span><span class="sm muted">matched</span>'
            f'<span class="grow"></span><span class="xs faint">showing 200 nearest the view</span></div>{res_rows}</aside>')
    related = artifact_list_panel(items=CLUSTERS[:4], selected=0, tree=False, title='')[len('<div class="panel">'):]
    preview = (f'<aside class="col sidebar" style="width:{RIGHT}px; flex:none; background:var(--tessera-surface)">'
               f'<div class="panel"><div class="hd">Document <button class="faint" aria-label="Close">{icon("close", 14)}</button></div>'
               f'<div class="card-title" style="margin-bottom:8px">Decoding surface codes with sparse attention transformers</div>'
               f'<div class="xs muted" style="margin-bottom:12px">Policy · 2024 · 4 authors · <span class="mono">0x3f9a…c21e</span></div>'
               f'<p class="sm" style="margin:0 0 12px; line-height:1.55; text-wrap:pretty">We study whether sparse attention decoders match minimum-weight matching on rotated surface codes at distances up to 21, and report thresholds under circuit-level noise. A regulatory appendix summarises procurement implications for fault-tolerant hardware.</p>'
               f'<div class="row"><button class="btn primary">{icon("book", 14)}Open in reader</button><button class="btn">Cite</button></div></div>'
               f'<div class="panel"><div class="hd">Related clusters</div>{related}'
               f'</aside>')
    preview = preview.replace('<div class="hd"> <span class="faint" style="text-transform:none;letter-spacing:0;font-weight:400">4 clusters</span></div>', '')
    header = (f'<div class="row" style="height:64px; padding:0 24px; gap:24px; border-bottom:1px solid var(--tessera-line); background:var(--tessera-surface)">'
              f'<span class="serif" style="font-size:19px; font-weight:600; letter-spacing:-0.01em">Meridian <span style="font-weight:500;color:var(--tessera-ink-2)">Research Desk</span></span>'
              f'<div class="input grow" style="height:38px; max-width:560px; font-size:14px; border-radius:3px">{icon("search", 16)}<span class="grow">quantum entanglement</span>'
              f'<div class="seg" style="height:26px"><button class="on">all words</button><button>phrase</button></div></div>'
              f'<span class="grow"></span><span class="sm muted row" style="gap:6px">{icon("lock", 14)}Your access: Policy, Legal</span></div>')
    facets = (f'<div class="row" style="height:44px; padding:0 24px; gap:8px; border-bottom:1px solid var(--tessera-line-2); background:var(--tessera-surface-2)">'
              f'<span class="facet on">Department: Policy, Legal{icon("close", 12)}</span><span class="facet on">2019 – 2024{icon("close", 12)}</span>'
              f'<span class="facet">Type{icon("chev", 12)}</span><span class="facet">Author{icon("chev", 12)}</span>'
              f'<span class="grow"></span><span class="xs faint">visible holds at 181,900 while filters move matched</span></div>')
    tabs = (f'<div class="row" style="height:44px; padding:0 20px; gap:0; border-bottom:1px solid var(--tessera-line-2); background:var(--tessera-surface)">'
            f'<span class="tab on">Map</span><span class="tab">List</span><span class="tab">Timeline</span></div>')
    body = f'''
<div class="tx md col" style="width:{W}px; height:{H}px;">
  {header}{facets}
  <div class="row grow" style="gap:0; align-items:stretch; min-height:0">
    {left}
    <div class="col grow" style="min-width:0">{tabs}
      <div class="tx-map grow" style="position:relative;">{pts}{hl}
        <div class="corner tr">{mode_control('pan')}</div>
        <div class="corner bl">{status_strip('shown', shown=200, matched=12465, visible=181900)}</div>
        <div class="corner br">{legend_float('department', [('Policy', '#7a1f2b'), ('Legal', '#3b6d8c')])}</div>
      </div>
    </div>
    {preview}
  </div>
</div>'''
    write('ResearchDesk.dc.html', dc_file(fonts + css_block(theme_vars=tokens, extra=extra), body))


# ============================================================================== Host 4 — editorial drop-in
def board_article():
    W, H = 900, 1500
    EW, EH = 680, 520
    pts, hl = arxiv_map(EW, EH, seed=77, n=1300, label_ids=(0, 1, 2, 3), sub=False, counts=False, boundaries='none', font='Newsreader')
    tokens = """
      --tessera-surface: #fffdf8; --tessera-surface-2: #f7f3ea; --tessera-surface-3: #ece6d8;
      --tessera-ink: #1f1d1a; --tessera-ink-2: #5a554d; --tessera-ink-3: #75706a;
      --tessera-line: #1f1d1a; --tessera-line-2: #e3ddd0;
      --tessera-accent: #2f5d3a; --tessera-accent-ink: #ffffff; --tessera-accent-soft: #e5eee7;
      --tessera-warn: #7a5600; --tessera-warn-soft: #f9edc9; --tessera-refuse: #8f2222; --tessera-refuse-soft: #f7dede; --tessera-ok: #2f5d3a;
      --tessera-map-bg: #fbf9f3; --tessera-map-grid: #efeadf;
      --tessera-radius: 0px; --tessera-font: 'Newsreader', Georgia, serif; --tessera-font-mono: 'Newsreader', Georgia, serif;
      --tessera-shadow: none; --tessera-map-height: 520px;
    """
    fonts = '<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Newsreader:ital,opsz,wght@0,6..72,400;0,6..72,500;0,6..72,600;1,6..72,400&amp;display=swap">'
    extra = """
      .ed { font-family: 'Newsreader', Georgia, serif; }
      .ed p { font-size: 18px; line-height: 1.6; margin: 0 0 22px; text-wrap: pretty; }
      .ed .tx-status { font-size: 13px; }
      .ed .tx-status .count b { font-family: 'Newsreader', Georgia, serif; font-size: 14px; }
      .ed .ctl { border-color: #1f1d1a; }
    """
    body = f'''
<div class="tx ed" style="width:{W}px; height:{H}px; padding:56px 0 0; display:flex; flex-direction:column; align-items:center; background:#fffdf8">
  <div style="width:{EW}px; display:flex; flex-direction:column;">
    <div class="row" style="justify-content:space-between; border-bottom:1px solid #1f1d1a; padding-bottom:10px; margin-bottom:36px">
      <span style="font-size:13px; letter-spacing:0.14em; text-transform:uppercase; font-weight:600">Field Notes</span>
      <span style="font-size:13px; color:var(--tessera-ink-2)">Data appendix · June 2026</span></div>
    <h1 style="font-size:44px; line-height:1.08; font-weight:500; margin:0 0 14px; letter-spacing:-0.015em; text-wrap:balance">The shape of a field</h1>
    <div style="font-size:15px; color:var(--tessera-ink-2); margin-bottom:34px; font-style:italic">What 2.4 million preprints look like from above, and why two readers never see the same map</div>
    <p>Every archive below is drawn from the same corpus, but the map you see depends on what you may read. The numbers in the corner are computed from that, and only that: they will not agree with a colleague\'s, and that disagreement is the point.</p>
    <p>Drag to move, scroll to zoom. The labels are clusters a topic model found; click one for the count of papers <em>you</em> can see in it.</p>
    <div style="border:1px solid #1f1d1a; margin:8px 0 12px;">
      <div class="tx-map" style="position:relative; height:{EH}px; background-image:none;">{pts}{hl}
        <div class="corner tr">{mode_control('pan')}</div>
        <div class="corner bl">{status_strip('shown', shown=2412, matched=181900, visible=181900)}</div>
      </div>
    </div>
    <div style="font-size:14px; color:var(--tessera-ink-2); line-height:1.5; margin-bottom:34px; text-wrap:pretty">Figure 1. arXiv, 2.4 million items, coloured by cluster. Embedded with one tag and a dozen style tokens; the panel fonts are the page\'s own.</div>
    <p>The map answers <em>how many</em>, <em>where</em> and <em>which examples</em> over exactly what a given reader may see. It is a counting engine before it is a scatterplot, which is why the count in the corner is worth reading before the picture.</p>
  </div>
</div>'''
    write('Article.dc.html', dc_file(fonts + css_block(theme_vars=tokens, extra=extra), body))


# ============================================================================== Host 5 — notebook
def board_notebook():
    W, H = 1200, 1000
    EW, EH, SB = 1040, 440, 280
    mw = EW - SB
    pts, hl = arxiv_map(mw, EH, seed=91, n=1100, label_ids=(0, 1, 2), sub=False)
    fonts = FONTS_LINK.replace('&amp;display=swap', '&amp;family=Source+Code+Pro:wght@400;500&amp;display=swap')
    extra = """
      .nb { font-family: system-ui, -apple-system, 'Segoe UI', sans-serif; background: #ffffff; color: #212121; }
      .nb .cell { display: grid; grid-template-columns: 72px 1fr; gap: 0; padding: 6px 24px 6px 0; }
      .nb .cell.active { border-left: 4px solid #2196f3; }
      .nb .prompt { font-family: 'Source Code Pro', ui-monospace, monospace; font-size: 13px; color: #303f9f; text-align: right; padding: 8px 12px 0 0; }
      .nb .prompt.out { color: #d84315; }
      .nb .code { font-family: 'Source Code Pro', ui-monospace, monospace; font-size: 13px; line-height: 1.55; background: #f5f5f5; border: 1px solid #e0e0e0; padding: 8px 12px; white-space: pre; border-radius: 2px; }
      .nb .code .kw { color: #008000; font-weight: 600; } .nb .code .st { color: #ba2121; } .nb .code .nm { color: #666; }
      .nb .out { font-family: 'Source Code Pro', ui-monospace, monospace; font-size: 13px; line-height: 1.55; padding: 8px 0; white-space: pre; }
      .nb .menubar { height: 30px; border-bottom: 1px solid #e0e0e0; display:flex; align-items:center; gap:18px; padding: 0 12px; font-size: 13px; color:#424242; }
      .nb .toolbar { height: 34px; border-bottom: 1px solid #e0e0e0; display:flex; align-items:center; gap:10px; padding: 0 12px; }
      .nb .toolbar span { width: 24px; height: 22px; border-radius: 2px; background:#f0f0f0; display:inline-block; }
    """
    body = f'''
<div class="nb col" style="width:{W}px; height:{H}px;">
  <div class="menubar"><b>Lab</b><span>File</span><span>Edit</span><span>View</span><span>Run</span><span>Kernel</span><span style="margin-left:auto;font-size:12px;color:#757575">Python 3 · idle</span></div>
  <div class="toolbar"><span></span><span></span><span></span><span></span><span style="width:60px"></span><span style="margin-left:auto;width:90px"></span></div>
  <div class="col" style="padding:14px 0; gap:6px; overflow:hidden">
    <div class="cell"><div class="prompt">[1]:</div><div class="code"><span class="kw">import</span> tesseradb
m = tesseradb.Map(<span class="st">"https://tessera.lab.internal"</span>, credential=os.environ[<span class="st">"TESSERA_SESSION_CRED"</span>],
                  view=<span class="st">"arxiv"</span>, layer=<span class="st">"clusters/kmeans-v1"</span>)
m</div></div>
    <div class="cell active"><div class="prompt"></div>
      <div class="tx" style="width:{EW}px; height:{EH}px; display:flex; border:1px solid var(--tessera-line); border-radius:var(--tessera-radius); overflow:hidden">
        <div class="tx-map" style="position:relative; width:{mw}px; flex:none;">{pts}{hl}
          <div class="corner tl">{mode_control('box')}</div>
          <div class="corner bl">{status_strip('shown', shown=3106, matched=181900, visible=181900)}</div>
          {lasso_layer(mw, EH, [(300, 120), (520, 120), (520, 300), (300, 300)], live=False)}
        </div>
        <aside class="sidebar col" style="width:{SB}px; flex:none;">
          {toolbar_panel(colour_by='clusters', layer='1 of 2 on')}
          {selection_panel(shown=214, matched=4102, visible=4102, shape='box', items=['Decoding surface codes with sparse attention transformers', 'Threshold estimates for bosonic cat codes under biased noise']).replace('and 212 more…', 'and 212 more…')}
        </aside>
      </div></div>
    <div class="cell"><div class="prompt">[2]:</div><div class="code">m.region</div></div>
    <div class="cell"><div class="prompt out">[2]:</div><div class="out">Region(shape=<span class="st">'box'</span>, visible=<span class="nm">4102</span>, matched=<span class="nm">4102</span>, shown=<span class="nm">214</span>, exact=<span class="kw">True</span>)</div></div>
    <div class="cell"><div class="prompt">[3]:</div><div class="code">m.selected</div></div>
    <div class="cell"><div class="prompt out">[3]:</div><div class="out">Item(tessera_id=<span class="nm">0x3f9a…c21e</span>, archive=<span class="st">'quant-ph'</span>, submitted_at=<span class="st">'2024-03-12'</span>, author_count=<span class="nm">4</span>)</div></div>
  </div>
</div>'''
    write('Notebook.dc.html', dc_file(fonts + css_block(extra=extra), body))


# ============================================================================== canvas.json
# ============================================================================== Highlight and hierarchy
MESH_ROWS = [
    (0, 'Neoplasms', 4_812_004, 21_309, '', True),
    (1, 'Neoplasms by Site', 1_204_881, 8_142, '', True),
    (2, 'Breast Neoplasms', 288_412, 3_004, 'Skin and Connective Tissue Diseases', False),
    (2, 'Lung Neoplasms', 251_770, 2_118, '', False),
    (2, 'Digestive System Neoplasms', 214_005, 1_442, '', False),
    (1, 'Neoplasms by Histologic Type', 962_110, 4_508, '', False),
    (0, 'Anatomy', 9_115_442, 44_002, '', False),
    (0, 'Chemicals and Drugs', 8_240_119, 39_551, '', False),
]


def board_highlight():
    W, H = 600, 400
    lit = {0, 4}
    map_svg, members, colours = datamap_highlight(W, H, ARXIV_CLUSTERS[:7], lit, seed=7, n=1500, r=1.5)
    plain, _m, _c = datamap(W, H, ARXIV_CLUSTERS[:7], seed=7, n=1500, r=1.5)
    frame = lambda inner, strip: (
        f'<div class="tx-map" style="width:{W}px; height:{H}px; position:relative; overflow:hidden;'
        f' border:1px solid var(--tessera-line); border-radius:var(--tessera-radius);">{inner}'
        f'<div class="corner bl">{strip}</div></div>')
    cap = lambda t: f'<div class="sm muted" style="max-width:{W}px; margin-top:10px; text-wrap:pretty">{t}</div>'
    filters = filters_panel(applied=(('title', 'quantum entanglement'), ('archive', 'quant-ph')),
                            verbs={'archive': 'highlight'}, compact=True, show_abstract=False)
    tree = hierarchy_panel(MESH_ROWS, filtered=True, clause=('Breast Neoplasms', 'highlight'))
    card = (artifact_card(label='Breast Neoplasms', layer='mesh/descriptors', key='D001943', count=288_412,
                          children=(('Breast Carcinoma in Situ', 12_004), ('Inflammatory Breast Neoplasms', 3_118)),
                          content='A MeSH descriptor. Its members are spread over the whole layout, so the layer draws nothing.')
            .replace('<div class="row" style="margin-top:12px"><button class="btn">' + icon('fit', 14) + 'Fit to cluster</button></div>',
                     artifact_card_verbs(pressed='highlight', fit=False)))
    body = f'''
<div class="tx" style="width:1760px; height:1080px; padding:28px 32px; display:flex; flex-direction:column; gap:20px;">
  <div class="col" style="gap:4px"><div class="hd">Highlight · member-of · the hierarchy panel</div>
    <span class="sm muted">A filter narrows the map; a highlight keeps every point and lights the matched ones. They are two fields of one request, and every clause carries both verbs.</span></div>
  <div class="row" style="gap:28px; align-items:flex-start">
    <div class="col">{frame(plain, status_strip(shown=4812, matched=12465, visible=181900))}
      {cap('1 · A filter. The map narrows to the matches and the strip says <b>matched</b>.')}</div>
    <div class="col">{frame(map_svg, status_strip(shown=4812, matched=12465, visible=181900, highlighted=3204))}
      {cap('2 · A highlight. The map does not move: matched points are lit, the rest dulled to 0.22 alpha, and the wash under them is the per-tile <b>highlighted</b> count — which is what shows the members the cap clause did not draw. The strip gains <b>the highlight matched N</b>.')}</div>
    <div class="col" style="width:336px">{filters}
      {cap('3 · The chip carries the verb. <b>archive</b> is highlighting and <b>title</b> is filtering; clicking the word moves the clause, and the predicate is never re-entered.')}</div>
  </div>
  <div class="row" style="gap:28px; align-items:flex-start">
    <div class="col" style="width:336px">{tree}
      {cap('4 · <b>tessera-hierarchy</b>, over the browse verb. The roots whatever the zoom, children on expansion, <i>also under</i> for a node served beneath several parents, and — under a filter — the matched count beside the masked one. A click is <b>highlight</b>.')}</div>
    <div class="col" style="width:336px">{card}
      {cap('5 · The card. <i>Filter to this</i>, <i>Highlight this</i> and <i>Outside this</i> are <b>member_of</b> clauses; the pressed one says what is on. No <i>fit</i>: this layer draws nothing.')}</div>
    <div class="col" style="width:336px">{layer_panel(layers=(('clusters/kmeans', True), ('topics/ctfidf', True)))}
      <div class="panel" style="margin-top:-10px"><div class="xs muted" style="margin:0 0 4px">Filter layers</div>
        <div class="col"><div class="row" style="height:26px;padding-left:22px"><span class="mono sm faint">mesh/descriptors</span></div></div></div>
      {cap('6 · A filter layer is still a layer. <b>mesh/descriptors</b> declares no computed content, so it is listed and never presented for viewing — no draw toggle, no place in <i>In view</i>, no label and no shape — and it is never named in a viewport request.')}</div>
  </div>
</div>'''
    write('Highlight.dc.html', dc_file(FONTS_LINK + css_block(), body))


def canvas():
    boards = [
        # page 1 — the default explorer
        dict(file='Main.dc.html', title='Explorer — docked', x=0, y=0, w=1440, h=900, page='page-1'),
        dict(file='ExplorerOverlay.dc.html', title='Explorer — overlay', x=1540, y=0, w=1440, h=900, page='page-1'),
        dict(file='ExplorerNarrow.dc.html', title='Explorer — narrow', x=0, y=1040, w=860, h=880, page='page-1'),
        dict(file='StatusStates.dc.html', title='Status — every state', x=960, y=1040, w=960, h=1200, page='page-1'),
        dict(file='SelectionFlow.dc.html', title='Lasso — the flow', x=2020, y=1040, w=1720, h=620, page='page-1'),
        # page 2 — host apps
        dict(file='OpsConsole.dc.html', title='Host 1 — ops console (geo)', x=0, y=0, w=1440, h=900, page='page-2'),
        dict(file='ImageCurator.dc.html', title='Host 2 — image dataset curator', x=1540, y=0, w=1440, h=900, page='page-2'),
        dict(file='ResearchDesk.dc.html', title='Host 3 — document research desk', x=0, y=1040, w=1440, h=900, page='page-2'),
        dict(file='Article.dc.html', title='Host 4 — editorial drop-in', x=1540, y=1040, w=900, h=1500, page='page-2'),
        dict(file='Notebook.dc.html', title='Host 5 — notebook', x=2540, y=1040, w=1200, h=1000, page='page-2'),
        dict(file='Views.dc.html', title='View switching — the two pickers', x=0, y=2400, w=1160, h=660, page='page-1'),
        dict(file='Highlight.dc.html', title='Highlight, member-of and the hierarchy panel', x=1260, y=2400, w=1760, h=1080, page='page-1'),
    ]
    notes = [
        dict(id='n-default', x=0, y=-150, w=560, page='page-1',
             text='The default explorer. One tag, its own store. Docked and overlay are the same components in two layouts; '
                  'the theme chip above each board switches the same tokens between light and dark.\nStates and the lasso flow below.'),
        dict(id='n-ops', x=0, y=-190, w=440, page='page-2',
             text='Host 1 · Ops console (geo)\nCustomisation: mid. Docked explorer; basemap slotted under the map; their facet rail is our filter elements restyled; their incident card replaces the item card. Tokens: dark, amber, Barlow + JetBrains Mono, 2px radii.'),
        dict(id='n-img', x=1540, y=-190, w=440, page='page-2',
             text='Host 2 · Image dataset curator (ML)\nCustomisation: heavy. No explorer — pieces in their own grid. Tooltip slot shows a thumbnail; item card and selection panel are theirs, fed from the store. Lasso → queue for labelling, with the sample and the set both named.'),
        dict(id='n-res', x=0, y=890, w=440, page='page-2',
             text='Host 3 · Document research desk\nCustomisation: mid. Search-first app; the header search box is a tessera-filter on title. Results list and document preview are theirs; the map is one view among tabs. Warm serif tokens.'),
        dict(id='n-art', x=1540, y=890, w=440, page='page-2',
             text='Host 4 · Editorial drop-in\nCustomisation: minimal. One tag in an article column, panels="legend", a dozen tokens: the page\'s serif, one green, no radii, no shadow. Tests the narrow container.'),
        dict(id='n-nb', x=2540, y=890, w=440, page='page-2',
             text='Host 5 · Notebook\nCustomisation: none. tesseradb.Map in a cell at cell width; the same docked explorer with a narrower sidebar. The next cells read the selection back into Python. The token never enters the notebook.'),
    ]
    doc = dict(pages=[dict(id='page-1', name='Default explorer'), dict(id='page-2', name='Host apps')],
               artboards=boards, annotations=notes, launch=dict(view='canvas', page='page-1'))
    with open(os.path.join(OUT, 'canvas.json'), 'w') as f:
        json.dump(doc, f, indent=2)


if __name__ == '__main__':
    board_main(); board_overlay(); board_narrow(); board_status(); board_selection()
    board_ops(); board_images(); board_research(); board_article(); board_notebook(); board_views()
    board_highlight()
    canvas()
    for n in sorted(os.listdir(OUT)):
        if n.endswith('.dc.html') or n == 'canvas.json':
            print(f'{os.path.getsize(os.path.join(OUT, n)) // 1024:5d} KB  {n}')
