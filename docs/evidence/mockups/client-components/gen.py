"""Shared generators for the client-component mock-ups: scatter maps, hulls, icons, and the
default explorer's component markup. Every board imports from here so the same pieces appear,
restyled, in every host app."""
import math
import random

# ----------------------------------------------------------------------------- data palette
# Categorical, distinguishable under the common colour-vision deficiencies (Okabe–Ito, adjusted).
PALETTE = ['#0072B2', '#E69F00', '#009E73', '#CC79A7', '#56B4E9', '#D55E00', '#B8A100', '#8C8C8C']
ARCHIVES = ['quant-ph', 'cs', 'math', 'physics', 'cond-mat', 'astro-ph', 'hep-th', 'other']


def hull(points):
    pts = sorted(set(points))
    if len(pts) < 3:
        return pts
    def cross(o, a, b):
        return (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    lower = []
    for p in pts:
        while len(lower) >= 2 and cross(lower[-2], lower[-1], p) <= 0:
            lower.pop()
        lower.append(p)
    upper = []
    for p in reversed(pts):
        while len(upper) >= 2 and cross(upper[-2], upper[-1], p) <= 0:
            upper.pop()
        upper.append(p)
    return lower[:-1] + upper[:-1]


def expand(poly, cx, cy, by=8):
    out = []
    for x, y in poly:
        dx, dy = x - cx, y - cy
        d = math.hypot(dx, dy) or 1
        out.append((x + dx / d * by, y + dy / d * by))
    return out


def scatter(w, h, seed=1, n=1400, clusters=None, palette=PALETTE, r=1.6, hull_ids=(), labels=None,
            dim_outside=None, muted=False, extra_class=''):
    """Clustered 2-D points as SVG. `clusters` is a list of (cx, cy, sx, sy, weight, class).
    Returns (svg_markup, hulls) where hulls maps cluster index -> (polygon, centroid)."""
    rnd = random.Random(seed)
    if clusters is None:
        clusters = []
        for i in range(9):
            clusters.append((rnd.uniform(0.12, 0.88) * w, rnd.uniform(0.12, 0.88) * h,
                             rnd.uniform(0.03, 0.09) * w, rnd.uniform(0.03, 0.09) * h,
                             rnd.uniform(0.5, 1.6), i % len(palette)))
    total_w = sum(c[4] for c in clusters)
    groups = {}
    members = {}
    for ci, (cx, cy, sx, sy, wt, cls) in enumerate(clusters):
        k = int(n * wt / total_w)
        pts = []
        for _ in range(k):
            x = rnd.gauss(cx, sx)
            y = rnd.gauss(cy, sy)
            if 0 < x < w and 0 < y < h:
                pts.append((round(x, 1), round(y, 1)))
        members[ci] = pts
        groups.setdefault(cls, []).extend(pts)
    # a thin background scatter so the map does not read as nine islands
    bg = [(round(rnd.uniform(0, w), 1), round(rnd.uniform(0, h), 1)) for _ in range(n // 6)]
    groups.setdefault(len(palette) - 1, []).extend(bg)
    parts = []
    for cls, pts in groups.items():
        fill = palette[cls % len(palette)]
        op = ' opacity="0.35"' if muted else ' opacity="0.85"'
        circles = ''.join(f'<circle cx="{x}" cy="{y}"/>' for x, y in pts)
        parts.append(f'<g fill="{fill}"{op} class="pts {extra_class}">{circles}</g>')
    hulls = {}
    for ci in hull_ids:
        pts = members.get(ci, [])
        if len(pts) < 5:
            continue
        cx = sum(p[0] for p in pts) / len(pts)
        cy = sum(p[1] for p in pts) / len(pts)
        poly = expand(hull(pts), cx, cy)
        hulls[ci] = (poly, (cx, cy))
    svg = (f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%;">'
           f'<style>circle{{r:{r}px}}</style>' + ''.join(parts) + '</svg>')
    return svg, hulls


def hull_layer(w, h, hulls, labels, stroke='#1c1f23', fill='rgba(28,31,35,0.06)', text='#1c1f23',
               font='IBM Plex Sans', selected=None, counts=None, halo='rgba(255,255,255,0.75)'):
    out = [f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%; pointer-events:none;">']
    for ci, (poly, (cx, cy)) in hulls.items():
        d = 'M' + ' L'.join(f'{x:.1f} {y:.1f}' for x, y in poly) + ' Z'
        sel = ci == selected
        out.append(f'<path d="{d}" fill="{fill}" stroke="{stroke}" stroke-width="{2 if sel else 1}" '
                   f'stroke-dasharray="{"" if sel else "4 3"}" stroke-linejoin="round"/>')
        label = labels.get(ci) if labels else None
        if label:
            cnt = f' · {counts[ci]}' if counts and ci in counts else ''
            out.append(f'<text x="{cx:.1f}" y="{cy:.1f}" text-anchor="middle" font-family="{font}, sans-serif" '
                       f'font-size="12" font-weight="600" fill="{text}" paint-order="stroke" stroke="{halo}" '
                       f'stroke-width="3">{label}{cnt}</text>')
    out.append('</svg>')
    return ''.join(out)


def lasso_layer(w, h, path_pts, snapped_cells=None, cell=12, live=True, color='#2457a3'):
    """A lasso: the live path while dragging, or the highlight snapped to counted tiles."""
    out = [f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%; pointer-events:none;">']
    d = 'M' + ' L'.join(f'{x} {y}' for x, y in path_pts)
    if live:
        out.append(f'<path d="{d}" fill="{color}" fill-opacity="0.08" stroke="{color}" stroke-width="1.5" '
                   f'stroke-dasharray="5 4" stroke-linejoin="round"/>')
    else:
        out.append(f'<path d="{d} Z" fill="{color}" fill-opacity="0.12" stroke="{color}" stroke-opacity="0.9" stroke-width="1.5" stroke-linejoin="round"/>')
    out.append('</svg>')
    return ''.join(out)


def inside(pt, poly):
    x, y = pt
    n = len(poly)
    ins = False
    j = n - 1
    for i in range(n):
        xi, yi = poly[i]
        xj, yj = poly[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / ((yj - yi) or 1e-9) + xi:
            ins = not ins
        j = i
    return ins


def basemap(w, h, seed=3, water='#0b1620', land='#151d24', road='#26323b', coast='#1f2e3a'):
    """A stylised basemap placeholder: water, two land masses, roads. Deliberately abstract."""
    rnd = random.Random(seed)
    def blob(cx, cy, rx, ry, k=14):
        pts = []
        for i in range(k):
            a = 2 * math.pi * i / k
            rr = rnd.uniform(0.8, 1.2)
            pts.append((cx + math.cos(a) * rx * rr, cy + math.sin(a) * ry * rr))
        return 'M' + ' L'.join(f'{x:.0f} {y:.0f}' for x, y in pts) + ' Z'
    out = [f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%;">'
           f'<rect width="{w}" height="{h}" fill="{water}"/>']
    for (cx, cy, rx, ry) in [(w * 0.42, h * 0.55, w * 0.40, h * 0.42), (w * 0.86, h * 0.22, w * 0.18, h * 0.2)]:
        out.append(f'<path d="{blob(cx, cy, rx, ry)}" fill="{land}" stroke="{coast}" stroke-width="2" stroke-linejoin="round"/>')
    for _ in range(9):
        x0, y0 = rnd.uniform(0.1, 0.7) * w, rnd.uniform(0.2, 0.9) * h
        pts = [(x0, y0)]
        for _ in range(5):
            x0 += rnd.uniform(-0.12, 0.16) * w
            y0 += rnd.uniform(-0.12, 0.12) * h
            pts.append((x0, y0))
        d = 'M' + ' L'.join(f'{x:.0f} {y:.0f}' for x, y in pts)
        out.append(f'<path d="{d}" fill="none" stroke="{road}" stroke-width="{rnd.choice([1, 1, 2])}" stroke-linecap="round"/>')
    out.append('</svg>')
    return ''.join(out)


# ----------------------------------------------------------------------------- icons (stroke, 16px grid)
_ICONS = {
    'pan': '<path d="M9 3v7M5 6v4M13 6v4M5 10c0 3 2 5 4 5s4-2 4-5"/>',
    'box': '<rect x="3" y="3" width="10" height="10" stroke-dasharray="2.5 2"/>',
    'lasso': '<path d="M8 2.5c3.3 0 5.5 1.6 5.5 3.7S11.3 10 8 10 2.5 8.4 2.5 6.2 4.7 2.5 8 2.5z"/><path d="M6 9.5c-.5 1.5-.5 3 .8 4"/>',
    'fit': '<path d="M2 6V2h4M10 2h4v4M14 10v4h-4M6 14H2v-4"/>',
    'refresh': '<path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9"/><path d="M13.5 2.5v3h-3"/>',
    'close': '<path d="M4 4l8 8M12 4l-8 8"/>',
    'search': '<circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5L14 14"/>',
    'chev': '<path d="M4 6l4 4 4-4"/>',
    'chevr': '<path d="M6 4l4 4-4 4"/>',
    'layers': '<path d="M8 2l6 3-6 3-6-3 6-3z"/><path d="M2 8l6 3 6-3M2 11l6 3 6-3"/>',
    'filter': '<path d="M2 3h12l-4.5 5.5V13l-3 1.5V8.5L2 3z"/>',
    'info': '<circle cx="8" cy="8" r="6"/><path d="M8 7v4M8 5v.5"/>',
    'warn': '<path d="M8 2l6.5 11.5h-13L8 2z"/><path d="M8 6.5v3M8 11.5v.5"/>',
    'check': '<path d="M3 8.5l3 3 7-7"/>',
    'open': '<path d="M9 3h4v4M13 3l-6 6M7 3H3v10h10V9"/>',
    'dots': '<circle cx="3" cy="8" r="1"/><circle cx="8" cy="8" r="1"/><circle cx="13" cy="8" r="1"/>',
    'plus': '<path d="M8 3v10M3 8h10"/>',
    'minus': '<path d="M3 8h10"/>',
    'menu': '<path d="M2 4h12M2 8h12M2 12h12"/>',
    'user': '<circle cx="8" cy="5.5" r="3"/><path d="M2.5 14c.7-3 2.8-4.5 5.5-4.5s4.8 1.5 5.5 4.5"/>',
    'lock': '<rect x="3" y="7" width="10" height="7" rx="1"/><path d="M5 7V5a3 3 0 0 1 6 0v2"/>',
    'grid': '<rect x="2" y="2" width="5" height="5"/><rect x="9" y="2" width="5" height="5"/><rect x="2" y="9" width="5" height="5"/><rect x="9" y="9" width="5" height="5"/>',
    'list': '<path d="M5 4h9M5 8h9M5 12h9M2 4h.5M2 8h.5M2 12h.5"/>',
    'clock': '<circle cx="8" cy="8" r="6"/><path d="M8 4.5V8l2.5 1.5"/>',
    'image': '<rect x="2" y="3" width="12" height="10" rx="1"/><path d="M2 11l3.5-3.5 3 3 2-2L14 12"/><circle cx="10.5" cy="6" r="1"/>',
    'tag': '<path d="M2 2h6l6 6-6 6-6-6V2z"/><circle cx="5.5" cy="5.5" r="1"/>',
    'play': '<path d="M4 2.5l9 5.5-9 5.5v-11z"/>',
    'book': '<path d="M2 3.5C4 2.5 6 2.5 8 3.5c2-1 4-1 6 0v10c-2-1-4-1-6 0-2-1-4-1-6 0v-10z"/><path d="M8 3.5v10"/>',
}


def icon(name, size=16, stroke='currentColor', sw=1.5, cls=''):
    return (f'<svg width="{size}" height="{size}" viewBox="0 0 16 16" fill="none" stroke="{stroke}" '
            f'stroke-width="{sw}" stroke-linecap="round" stroke-linejoin="round" class="{cls}" aria-hidden="true">'
            f'{_ICONS[name]}</svg>')


# ----------------------------------------------------------------------------- the explorer's tokens + CSS
FONTS_LINK = ('<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Sans:wght@400;500;600'
              '&amp;family=IBM+Plex+Sans+Condensed:wght@500;600&amp;family=IBM+Plex+Mono:wght@400;500&amp;display=swap">')

TOKENS_LIGHT = """
  --tessera-surface: #fbfbfa; --tessera-surface-2: #f2f2ef; --tessera-surface-3: #e7e7e2;
  --tessera-ink: #1c1f23; --tessera-ink-2: #555b63; --tessera-ink-3: #6f757d;
  --tessera-line: #d6d6d0; --tessera-line-2: #e6e6e1;
  --tessera-accent: #2457a3; --tessera-accent-ink: #ffffff; --tessera-accent-soft: #e4ecf8;
  --tessera-warn: #7a5600; --tessera-warn-soft: #fff1cf;
  --tessera-refuse: #a12b2b; --tessera-refuse-soft: #fbe5e5;
  --tessera-ok: #226b44;
  --tessera-map-bg: #f7f7f4; --tessera-map-grid: #e9e9e4;
  --tessera-radius: 4px; --tessera-font: 'IBM Plex Sans', system-ui, sans-serif;
  --tessera-font-mono: 'IBM Plex Mono', ui-monospace, monospace;
  --tessera-shadow: 0 1px 2px rgba(20,22,25,0.08), 0 4px 16px rgba(20,22,25,0.08);
  --tessera-map-height: 420px;
"""
TOKENS_DARK = """
  --tessera-surface: #151719; --tessera-surface-2: #1d2024; --tessera-surface-3: #272b30;
  --tessera-ink: #e8eaec; --tessera-ink-2: #aab0b7; --tessera-ink-3: #868d95;
  --tessera-line: #30353b; --tessera-line-2: #262a2f;
  --tessera-accent: #86b0f0; --tessera-accent-ink: #0d1a2e; --tessera-accent-soft: #1f2d42;
  --tessera-warn: #e6b84a; --tessera-warn-soft: #3a2e0e;
  --tessera-refuse: #f29a9a; --tessera-refuse-soft: #3e1c1c;
  --tessera-ok: #6cc38e;
  --tessera-map-bg: #0c0e11; --tessera-map-grid: #191c20;
  --tessera-shadow: 0 1px 2px rgba(0,0,0,0.4), 0 6px 20px rgba(0,0,0,0.45);
"""

# Component CSS. Selectors are the *parts* of the design (`part="…"` names), written as classes so
# the mock-up reads like the contract: .tx-status is <tessera-status>, [part=count] is its count.
COMPONENT_CSS = """
  .tx { font-family: var(--tessera-font); color: var(--tessera-ink); background: var(--tessera-surface);
        font-size: 13px; line-height: 1.45; -webkit-font-smoothing: antialiased; }
  .tx * { box-sizing: border-box; }
  .tx button { font: inherit; color: inherit; background: none; border: 0; cursor: pointer; }
  .tx :focus-visible { outline: 2px solid var(--tessera-accent); outline-offset: 2px; }
  .mono { font-family: var(--tessera-font-mono); font-variant-numeric: tabular-nums; }
  .num { font-family: var(--tessera-font-mono); font-variant-numeric: tabular-nums; letter-spacing: -0.01em; }
  .muted { color: var(--tessera-ink-2); }
  .faint { color: var(--tessera-ink-3); }
  .sm { font-size: 12px; }
  .xs { font-size: 11px; }
  .row { display: flex; align-items: center; gap: 8px; }
  .col { display: flex; flex-direction: column; }
  .grow { flex-grow: 1; }
  .hd { font-size: 11px; font-weight: 600; letter-spacing: 0.06em; text-transform: uppercase; color: var(--tessera-ink-2); }

  /* map */
  .tx-map { position: relative; background: var(--tessera-map-bg); overflow: hidden; }
  .tx-map .corner { position: absolute; display: flex; flex-direction: column; gap: 8px; }
  .tx-map .tl { top: 12px; left: 12px; } .tx-map .tr { top: 12px; right: 12px; }
  .tx-map .bl { bottom: 12px; left: 12px; } .tx-map .br { bottom: 12px; right: 12px; }
  .ctl { display: flex; flex-direction: column; background: var(--tessera-surface); border: 1px solid var(--tessera-line);
         border-radius: var(--tessera-radius); box-shadow: var(--tessera-shadow); overflow: hidden; }
  .ctl button { width: 36px; height: 36px; display: grid; place-items: center; color: var(--tessera-ink-2);
                border-bottom: 1px solid var(--tessera-line-2); }
  .ctl button:last-child { border-bottom: 0; }
  .ctl button.on { background: var(--tessera-accent-soft); color: var(--tessera-accent); }
  .ctl .sep { height: 6px; background: var(--tessera-surface-2); border-bottom: 1px solid var(--tessera-line-2); }

  /* status strip */
  .tx-status { display: inline-flex; align-items: center; gap: 0; background: var(--tessera-surface);
               border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius);
               box-shadow: var(--tessera-shadow); height: 36px; font-size: 12px; white-space: nowrap; }
  .tx-status > * { display: flex; align-items: center; gap: 6px; padding: 0 12px; height: 100%; }
  .tx-status > * + * { border-left: 1px solid var(--tessera-line-2); }
  .tx-status .badge { font-weight: 600; }
  .tx-status .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--tessera-ok); }
  .tx-status .count { gap: 5px; }
  .tx-status .count b { font-weight: 500; font-family: var(--tessera-font-mono); font-variant-numeric: tabular-nums; font-size: 12.5px; }
  .tx-status .count span { color: var(--tessera-ink-2); }
  .tx-status .warn { background: var(--tessera-warn-soft); color: var(--tessera-warn); font-weight: 600; }
  .tx-status .refuse { background: var(--tessera-refuse-soft); color: var(--tessera-refuse); font-weight: 600; }
  .tx-status .quiet { color: var(--tessera-ink-2); }
  .tx-status button.act { padding: 0 10px; height: 26px; margin: 0 6px 0 0; border-radius: 3px; font-weight: 600; font-size: 12px;
                          background: var(--tessera-accent); color: var(--tessera-accent-ink); display: inline-flex; align-items: center; gap: 6px; }
  .tx-status.narrow > * { padding: 0 8px; }
  .tx-status.narrow .count b { font-size: 12px; }
  .tx-status .skel { display: inline-block; height: 10px; width: 44px; border-radius: 2px; background: var(--tessera-surface-3); }

  /* panels */
  .panel { border-bottom: 1px solid var(--tessera-line-2); padding: 14px 16px; }
  .panel:last-child { border-bottom: 0; }
  .panel .hd { margin-bottom: 10px; display: flex; align-items: center; justify-content: space-between; }
  .kv { display: grid; grid-template-columns: 1fr auto; gap: 4px 16px; align-items: baseline; }
  .kv .k { color: var(--tessera-ink-2); }
  .kv .v { font-family: var(--tessera-font-mono); font-variant-numeric: tabular-nums; text-align: right; }
  .chip { display: inline-flex; align-items: center; gap: 6px; height: 24px; padding: 0 6px 0 8px; border-radius: 3px;
          background: var(--tessera-accent-soft); color: var(--tessera-accent); font-size: 12px; font-weight: 500; }
  .chip svg { opacity: 0.8; }
  .input { white-space: nowrap; overflow: hidden; height: 30px; border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius); background: var(--tessera-surface);
           padding: 0 10px; display: flex; align-items: center; gap: 8px; color: var(--tessera-ink); font-size: 13px; }
  .input.ph { color: var(--tessera-ink-3); }
  .select { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; height: 30px; border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius); background: var(--tessera-surface);
            padding: 0 8px 0 10px; display: inline-flex; align-items: center; justify-content: space-between; gap: 8px; font-size: 13px; }
  .seg { display: inline-flex; border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius); overflow: hidden; height: 28px; }
  .seg button { padding: 0 10px; font-size: 12px; color: var(--tessera-ink-2); }
  .seg button.on { background: var(--tessera-surface-3); color: var(--tessera-ink); font-weight: 600; }
  .check { display: flex; align-items: center; gap: 8px; height: 26px; }
  .check .bx { width: 15px; height: 15px; border: 1.5px solid var(--tessera-ink-3); border-radius: 3px; display: grid; place-items: center; }
  .check .bx.on { background: var(--tessera-accent); border-color: var(--tessera-accent); color: var(--tessera-accent-ink); }
  .radio { display: flex; align-items: center; gap: 8px; height: 26px; }
  .radio .bx { width: 15px; height: 15px; border: 1.5px solid var(--tessera-ink-3); border-radius: 50%; display: grid; place-items: center; }
  .radio .bx.on { border-color: var(--tessera-accent); }
  .radio .bx.on::after { content: ''; width: 7px; height: 7px; border-radius: 50%; background: var(--tessera-accent); }
  .sw { width: 10px; height: 10px; border-radius: 2px; flex: none; }
  .btn { height: 30px; padding: 0 12px; border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius);
         display: inline-flex; align-items: center; gap: 6px; font-weight: 500; font-size: 12.5px; background: var(--tessera-surface); }
  .btn.primary { background: var(--tessera-accent); color: var(--tessera-accent-ink); border-color: var(--tessera-accent); }
  .btn.off { color: var(--tessera-ink-3); border-style: dashed; cursor: not-allowed; }
  .list > .item { display: flex; align-items: center; gap: 8px; height: 30px; padding: 0 6px; border-radius: 3px; }
  .list > .item.on { background: var(--tessera-accent-soft); }
  .list > .item .n { margin-left: auto; font-family: var(--tessera-font-mono); font-variant-numeric: tabular-nums; color: var(--tessera-ink-2); font-size: 12px; }
  .list > .item.child { padding-left: 24px; }
  .card-title { font-size: 14.5px; font-weight: 600; line-height: 1.35; text-wrap: pretty; }
  .field { display: grid; grid-template-columns: 112px 1fr; gap: 6px 12px; align-items: baseline; }
  .field .k { color: var(--tessera-ink-2); font-size: 12px; }
  .field .v { font-size: 13px; }
  .tip { position: absolute; background: var(--tessera-surface); border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius);
         box-shadow: var(--tessera-shadow); padding: 8px 10px; max-width: 260px; font-size: 12px; }
  .state { display: flex; align-items: center; gap: 8px; padding: 10px 12px; border-radius: var(--tessera-radius);
           background: var(--tessera-surface-2); color: var(--tessera-ink-2); font-size: 12px; }
  .state.refuse { background: var(--tessera-refuse-soft); color: var(--tessera-refuse); }
  .state.warn { background: var(--tessera-warn-soft); color: var(--tessera-warn); }
  .skel { display: inline-block; height: 10px; border-radius: 2px; background: var(--tessera-surface-3); }
  .drawer { border-top: 1px solid var(--tessera-line-2); margin-top: 10px; padding-top: 10px; }
  .sidebar { background: var(--tessera-surface); border-left: 1px solid var(--tessera-line); overflow: hidden; }
  .float { background: var(--tessera-surface); border: 1px solid var(--tessera-line); border-radius: var(--tessera-radius); box-shadow: var(--tessera-shadow); }
"""


def css_block(theme_vars=TOKENS_LIGHT, dark_vars=TOKENS_DARK, extra=''):
    return (f'<style>\n  body {{ margin: 0; }} a {{ color: #2457a3; }} a:hover {{ color: #1a3f78; }}\n'
            f'  .tx {{ {theme_vars} }}\n  .tx.dark {{ {dark_vars} }}\n{COMPONENT_CSS}\n{extra}\n</style>')


# ----------------------------------------------------------------------------- component markup
def fmt(n):
    return f'{n:,}'


def status_strip(state='shown', shown=4812, matched=12465, visible=181900, retry=None, code=None, narrow=False):
    """<tessera-status> compact. Only `shown` carries numbers."""
    def counts(dim=False):
        st = ' style="opacity:0.45"' if dim else ''
        return (f'<div class="count" part="count"{st}><b>{fmt(shown)}</b><span>shown</span></div>'
                f'<div class="count" part="count"{st}><b>{fmt(matched)}</b><span>matched</span></div>'
                f'<div class="count" part="count"{st}><b>{fmt(visible)}</b><span>visible</span></div>')
    skel = ('<div class="count"><span class="skel"></span><span>shown</span></div>'
            '<div class="count"><span class="skel"></span><span>matched</span></div>'
            '<div class="count"><span class="skel"></span><span>visible</span></div>')
    if state == 'shown' and narrow:
        body = f'<div class="badge" part="state" style="padding:0 8px"><span class="dot" role="img" aria-label="Current"></span></div>{counts()}'
    elif state == 'shown':
        body = f'<div class="badge" part="state"><span class="dot"></span>Current</div>{counts()}'
    elif state == 'loading':
        body = f'<div class="badge quiet" part="state"><span class="dot" style="background:var(--tessera-ink-3)"></span>Loading</div>{skel}'
    elif state == 'session':
        body = (f'<div class="badge quiet" part="state" title="The first request materialises what you may see; it can take a few seconds."><span class="dot" style="background:var(--tessera-ink-3)"></span>Starting session…</div>')
    elif state == 'retrying':
        body = (f'<div class="badge quiet" part="state"><span class="dot" style="background:var(--tessera-warn)"></span>Retrying</div>'
                f'<div class="quiet" title="The server asked for a pause; retrying after Retry-After.">{retry or 2} of 5</div>{skel}')
    elif state == 'empty':
        body = (f'<div class="badge" part="state"><span class="dot"></span>Current</div>'
                f'{status_counts_zero()}<div class="quiet">nothing in this region</div>')
    elif state == 'refused':
        body = (f'<div class="refuse" part="state">{icon("warn", 14)}Refused</div>'
                f'<div class="quiet mono">{code or "422 contract"}</div>')
    elif state == 'expired':
        body = (f'<div class="refuse" part="state">{icon("lock", 14)}Session expired</div>'
                f'<div><button class="act" part="refresh">Sign in again</button></div>')
    elif state == 'stale':
        body = (f'<div class="warn" part="state">{icon("clock", 14)}Corpus updated</div>{counts(dim=True)}'
                f'<div><button class="act" part="refresh">{icon("refresh", 13)}Refresh</button></div>')
    elif state == 'detached':
        body = f'<div class="quiet" part="state">No data</div>'
    else:
        body = counts()
    cls = 'tx-status narrow' if narrow else 'tx-status'
    return (f'<div class="{cls}" role="status" aria-live="polite" title="Shown is a sample of matched; filters narrow matched and never move visible. '
            f'Depth 9 · 3,312 tiles · 0 provisional · replica 41 MB, 93% from cache.">{body}</div>')


def status_counts_zero():
    return ('<div class="count"><b>0</b><span>shown</span></div>'
            '<div class="count"><b>0</b><span>matched</span></div>'
            '<div class="count"><b>0</b><span>visible</span></div>')


def status_expanded(shown=4812, matched=12465, visible=181900, provisional=0, depth=9, tiles=3312,
                    replica=True, stale=False, brief=False):
    stale_row = ''
    if stale:
        stale_row = (f'<div class="state warn" style="margin-bottom:10px">{icon("clock", 14)}'
                     f'<span class="grow">The corpus changed since this view was drawn</span>'
                     f'<button class="btn primary" style="height:26px">{icon("refresh", 13)}Refresh</button></div>')
    dim = ' style="opacity:0.45"' if stale else ''
    drawer = ''
    if replica:
        drawer = ('<details class="drawer"><summary class="sm muted" style="cursor:pointer">Replica</summary>'
                  '<div class="kv sm" style="margin-top:8px">'
                  '<div class="k">Held</div><div class="v">41.2 MB · 1.18M points · 118 bands</div>'
                  '<div class="k">Last view</div><div class="v">93% from cache · 2 tiles fetched</div>'
                  '<div class="k">Look-ahead</div><div class="v">3 rings · 1.1 MB</div>'
                  '</div></details>')
    return (f'<div class="panel"><div class="hd">View <span class="row sm" style="text-transform:none;letter-spacing:0;font-weight:500;color:var(--tessera-ok)">'
            f'<span class="dot" style="width:8px;height:8px;border-radius:50%;background:var(--tessera-ok);display:inline-block"></span>Current</span></div>'
            f'{stale_row}'
            f'<div class="kv"{dim}>'
            f'<div class="k">Shown</div><div class="v">{fmt(shown)}</div>'
            f'<div class="k">Matched by filters</div><div class="v">{fmt(matched)}</div>'
            f'<div class="k">Visible to you here</div><div class="v">{fmt(visible)}</div>'
            f'</div>'
            f'<p class="xs faint" style="margin:8px 0 0">Shown is a sample of matched. Filters narrow matched and never move visible.</p>'
            + ('' if brief else f'<div class="kv sm" style="margin-top:10px"><div class="k">Region</div><div class="v">depth {depth} · {fmt(tiles)} tiles</div>'
            f'<div class="k">Provisional marks</div><div class="v">{provisional}</div></div>')
            + f'{drawer}</div>')


def toolbar_panel(colour_by='archive', layer='2 of 3 on'):
    return (f'<div class="panel"><div class="row" style="gap:10px">'
            f'<div class="col grow" style="gap:4px"><span class="xs muted">Colour by</span>'
            f'<div class="select">{colour_by}{icon("chev", 14)}</div></div>'
            f'<div class="col grow" style="gap:4px"><span class="xs muted">Layers</span>'
            f'<div class="select">{layer}{icon("chev", 14)}</div></div>'
            f'</div></div>')


def filters_panel(applied=(('title', 'quantum entanglement'), ('archive', 'quant-ph')), compact=False,
                  archives=ARCHIVES, checked=('quant-ph',), show_abstract=True, stack_seg=False):
    chips = ''.join(f'<span class="chip">{k}: {v}{icon("close", 12)}</span>' for k, v in applied)
    clear = '<button class="sm" style="color:var(--tessera-accent);font-weight:500">Clear all</button>' if applied else ''
    checks = ''.join(
        f'<div class="check"><span class="bx {"on" if a in checked else ""}">{icon("check", 11, sw=2.2) if a in checked else ""}</span>{a}</div>'
        for a in archives[:6 if compact else 8])
    abstract = ''
    if show_abstract:
        abstract = (f'<div class="col" style="gap:6px;margin-top:12px"><span class="xs muted">Abstract</span>'
                    f'<div class="input ph">{icon("search", 14)}words in the abstract</div></div>')
    return (f'<div class="panel"><div class="hd">Filters {clear}</div>'
            f'<div class="row" style="flex-wrap:wrap;gap:6px;margin-bottom:12px">{chips}</div>'
            f'<div class="col" style="gap:6px"><span class="xs muted">Title</span>'
            f'<div class="{"col" if stack_seg else "row"}" style="gap:6px"><div class="input grow">{icon("search", 14)}quantum entanglement</div>'
            f'<div class="seg" style="align-self:flex-start"><button class="on">all words</button><button>phrase</button></div></div></div>'
            f'{abstract}'
            f'<div class="col" style="gap:2px;margin-top:12px"><span class="xs muted" style="margin-bottom:4px">Archive</span>{checks}'
            f'<button class="sm faint" style="text-align:left;height:24px">Show {len(archives) - (6 if compact else 8) + 6} more…</button></div>'
            f'<div class="col" style="gap:6px;margin-top:12px"><span class="xs muted">Submitted</span>'
            f'<div class="row"><div class="input grow mono">2019-01-01</div><span class="faint">to</span><div class="input grow mono">2024-06-30</div></div></div>'
            f'<div class="col" style="gap:6px;margin-top:12px"><span class="xs muted">Authors</span>'
            f'<div class="row"><div class="input grow mono">1</div><span class="faint">to</span><div class="input grow mono">12</div></div></div>'
            f'</div>')


def legend_panel(column='archive', values=None, selectable=True, compact=False):
    values = values or list(zip(ARCHIVES, PALETTE))
    rows = ''.join(f'<div class="row" style="height:{22 if compact else 24}px"><span class="sw" style="background:{c}"></span><span>{v}</span></div>'
                   for v, c in values)
    sel = f'<div class="select" style="height:26px">{column}{icon("chev", 14)}</div>' if selectable else f'<span class="sm muted">{column}</span>'
    return f'<div class="panel"><div class="hd">Colour {sel}</div><div class="col" style="gap:0">{rows}</div></div>'


def legend_float(column='archive', values=None, cols=1):
    values = values or list(zip(ARCHIVES, PALETTE))
    rows = ''.join(f'<div class="row" style="height:20px;gap:6px"><span class="sw" style="background:{c}"></span><span class="xs">{v}</span></div>'
                   for v, c in values)
    return (f'<div class="float" style="padding:8px 10px;min-width:120px"><div class="xs muted" style="margin-bottom:4px">{column}</div>'
            f'<div style="display:grid;grid-template-columns:repeat({cols}, minmax(0, 1fr));gap:0 14px">{rows}</div></div>')


def layer_panel(layers=(('clusters/kmeans-v1', True), ('topics/ctfidf-2026-08', True), ('boundaries/arxiv-archives', False)),
                title='Layers'):
    rows = ''.join(f'<div class="check"><span class="bx {"on" if on else ""}">{icon("check", 11, sw=2.2) if on else ""}</span><span class="mono sm">{l}</span></div>'
                   for l, on in layers)
    return f'<div class="panel"><div class="hd">{title}</div><div class="col">{rows}</div></div>'


CLUSTERS = [('quantum error correction', 1962), ('graph neural networks', 1410), ('dark matter haloes', 903),
            ('topological insulators', 811), ('sparse attention', 640), ('gravitational lensing', 514),
            ('lattice QCD', 447), ('optimal transport', 388), ('exoplanet atmospheres', 212)]


def artifact_list_panel(items=CLUSTERS, selected=0, tree=True, title='In view'):
    rows = []
    for i, (label, n) in enumerate(items):
        rows.append(f'<div class="item {"on" if i == selected else ""}"><span class="grow" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">{label}</span><span class="n">{fmt(n)}</span></div>')
        if tree and i == 0:
            rows.append('<div class="item child"><span class="grow">surface codes</span><span class="n">1,104</span></div>')
            rows.append('<div class="item child"><span class="grow">bosonic codes</span><span class="n">858</span></div>')
    return (f'<div class="panel"><div class="hd">{title} <span class="faint" style="text-transform:none;letter-spacing:0;font-weight:400">{len(items)} clusters</span></div>'
            f'<div class="list col">{"".join(rows)}</div></div>')


def item_card(title='Decoding surface codes with sparse attention transformers', fields=None, actions=True,
              note=None):
    fields = fields or [('archive', 'quant-ph'), ('primary_category', 'quant-ph'), ('submitted_at', '2024-03-12'),
                        ('author_count', '4'), ('tessera_id', '0x3f9a…c21e')]
    rows = ''.join(f'<div class="k">{k}</div><div class="v {"mono sm" if k in ("tessera_id", "submitted_at") else ""}">{v}</div>'
                   for k, v in fields)
    act = ''
    if actions:
        act = (f'<div class="row" style="margin-top:12px"><button class="btn">{icon("open", 14)}Open</button>'
               f'<button class="btn" style="border:0;color:var(--tessera-ink-2)">Copy id</button></div>')
    if note:
        act += f'<p class="xs faint" style="margin:10px 0 0">{note}</p>'
    return (f'<div class="panel"><div class="hd">Item <button class="faint" aria-label="Close">{icon("close", 14)}</button></div>'
            f'<div class="card-title" style="margin-bottom:10px">{title}</div>'
            f'<div class="field">{rows}</div>{act}</div>')


def artifact_card(label='quantum error correction', layer='clusters/kmeans-v1', key='c-0013', count=1962,
                  children=(('surface codes', 1104), ('bosonic codes', 858)), content=None):
    kids = ''.join(f'<div class="item child"><span class="grow">{l}</span><span class="n">{fmt(n)}</span></div>' for l, n in children)
    content = content or 'Papers on fault-tolerant quantum computation, decoders and code thresholds.'
    return (f'<div class="panel"><div class="hd">Cluster <button class="faint" aria-label="Close">{icon("close", 14)}</button></div>'
            f'<div class="card-title" style="margin-bottom:6px">{label}</div>'
            f'<div class="row" style="gap:6px;margin-bottom:10px"><span class="num" style="font-size:20px;font-weight:500">{fmt(count)}</span>'
            f'<span class="muted">members visible to you</span></div>'
            f'<p class="sm" style="margin:0 0 10px;color:var(--tessera-ink-2)">{content}</p>'
            f'<div class="field"><div class="k">layer</div><div class="v mono sm">{layer}</div><div class="k">key</div><div class="v mono sm">{key}</div></div>'
            f'<div class="xs muted" style="margin:12px 0 4px">Children in this view</div><div class="list col">{kids}</div>'
            f'<div class="row" style="margin-top:12px"><button class="btn">{icon("fit", 14)}Fit to cluster</button></div></div>')


def selection_panel(shown=214, matched=2114, visible=4102, items=None, shape='lasso'):
    items = items or ['Decoding surface codes with sparse attention transformers',
                      'Threshold estimates for bosonic cat codes under biased noise',
                      'A graph-state compiler for photonic cluster states',
                      'Leakage-aware syndrome extraction on heavy-hex lattices']
    rows = ''.join(f'<div class="item"><span class="grow" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">{t}</span></div>' for t in items)
    return (f'<div class="panel"><div class="hd">Selection <span class="faint" style="text-transform:none;letter-spacing:0;font-weight:400">{shape}</span></div>'
            f'<div class="kv"><div class="k">Shown inside</div><div class="v">{fmt(shown)}</div>'
            f'<div class="k">Matched inside</div><div class="v">{fmt(matched)}</div>'
            f'<div class="k">Visible inside</div><div class="v">{fmt(visible)}</div></div>'
            f'<div class="xs muted" style="margin:10px 0 4px">Shown items</div><div class="list col">{rows}'
            f'<button class="sm faint" style="text-align:left;height:26px;padding:0 6px">and {fmt(shown - len(items))} more…</button></div>'
            f'<div class="row" style="flex-wrap:wrap;gap:6px;margin-top:12px">'
            f'<button class="btn">{icon("close", 13)}Clear</button>'
            f'<button class="btn off" title="Needs the selection operand (not yet served)">{icon("filter", 13)}Filter to this</button>'
            f'<button class="btn off" title="Needs the export verb (not yet served)">Export</button></div></div>')


def mode_control(mode='pan', fit=True):
    def b(name, ic, label):
        on = ' on' if name == mode else ''
        return f'<button class="{on.strip()}" aria-label="{label}" aria-pressed="{"true" if on else "false"}">{icon(ic, 16)}</button>'
    out = b('pan', 'pan', 'Pan') + b('box', 'box', 'Box select') + b('lasso', 'lasso', 'Lasso select')
    if fit:
        out += '<div class="sep"></div>' + f'<button aria-label="Fit to extent">{icon("fit", 16)}</button>'
    return f'<div class="ctl" role="toolbar" aria-label="Map tools">{out}</div>'


def tooltip(x, y, title='Decoding surface codes with sparse attention transformers', sub='quant-ph · 2024'):
    return (f'<div class="tip" style="left:{x}px;top:{y}px"><div style="font-weight:500;line-height:1.35">{title}</div>'
            f'<div class="xs muted" style="margin-top:3px">{sub}</div></div>')


def dc_file(head_extra, body, script=None, title=''):
    """Wrap an artboard body in the Design Component envelope."""
    scr = script or ''
    return (f'<!doctype html>\n<html>\n<head>\n  <meta charset="utf-8">\n  <script src="./support.js"></script>\n</head>\n'
            f'<body>\n<x-dc>\n<helmet>\n{head_extra}\n</helmet>\n{body}\n</x-dc>\n{scr}\n</body>\n</html>\n')


THEME_SCRIPT = ('<script data-dc-script data-props=\'{"theme":{"editor":"enum","options":["light","dark"],"default":"%s","section":"Theme"}}\'>\n'
                'class Component extends DCLogic {\n  renderVals() {\n    const t = this.props.theme ?? "%s";\n'
                '    return { themeClass: t === "dark" ? "tx dark" : "tx" };\n  }\n}\n</script>')


# ----------------------------------------------------------------------------- DataMapPlot-style rendering
import colorsys


def position_colours(clusters, w, h, dark=False):
    """One colour per cluster from its position: hue by angle about the map centre, lightness by
    distance — so neighbouring clusters get related hues and the map reads as a whole."""
    out = []
    for (cx, cy, *_rest) in clusters:
        ang = math.atan2(cy - h / 2, cx - w / 2)
        hue = ((ang / (2 * math.pi)) + 0.5 + 0.45) % 1.0
        dist = math.hypot((cx - w / 2) / (w / 2), (cy - h / 2) / (h / 2))
        light = (0.64 if dark else 0.40) + 0.08 * min(dist, 1.0) * (1 if dark else -1)
        r, g, b = colorsys.hls_to_rgb(hue, light, 0.62 if dark else 0.58)
        out.append('#%02x%02x%02x' % (int(r * 255), int(g * 255), int(b * 255)))
    return out


def datamap(w, h, clusters, seed=7, n=1600, r=1.7, dark=False, noise='#b8b8b2', glow=True,
            point_opacity=None):
    """Points as a data map: per-cluster position colours, a blurred glow underneath, grey noise.
    Returns (svg, members, colours)."""
    rnd = random.Random(seed)
    cl = [(cx * w, cy * h, sx * w, sy * h, wt, cls) for cx, cy, sx, sy, wt, cls in clusters]
    colours = position_colours(cl, w, h, dark=dark)
    total_w = sum(c[4] for c in cl)
    members = {}
    for ci, (cx, cy, sx, sy, wt, cls) in enumerate(cl):
        k = int(n * wt / total_w)
        pts = []
        for _ in range(k):
            # a slightly heavy-tailed blob reads more like a real embedding than a clean gaussian
            t = rnd.gauss(0, 1)
            x = cx + rnd.gauss(0, sx) * (1 + 0.15 * abs(t))
            y = cy + rnd.gauss(0, sy) * (1 + 0.15 * abs(t))
            if 0 < x < w and 0 < y < h:
                pts.append((round(x, 1), round(y, 1)))
        members[ci] = pts
    bg = [(round(rnd.uniform(0, w), 1), round(rnd.uniform(0, h), 1)) for _ in range(n // 7)]
    po = point_opacity or (0.7 if dark else 0.62)
    parts = []
    if glow:
        # a blurred, larger copy of every cluster's points, underneath — density reads as light
        glow_parts = []
        for ci, pts in members.items():
            circles = ''.join(f'<circle cx="{x}" cy="{y}"/>' for x, y in pts[::2])
            glow_parts.append(f'<g fill="{colours[ci]}">{circles}</g>')
        parts.append(f'<g filter="url(#glow)" opacity="{0.5 if dark else 0.2}" class="glow">' + ''.join(glow_parts) + '</g>')
    noise_c = '#5a5f66' if dark else noise
    parts.append(f'<g fill="{noise_c}" opacity="0.5" class="noise">' + ''.join(f'<circle cx="{x}" cy="{y}"/>' for x, y in bg) + '</g>')
    for ci, pts in members.items():
        circles = ''.join(f'<circle cx="{x}" cy="{y}"/>' for x, y in pts)
        parts.append(f'<g fill="{colours[ci]}" opacity="{po}">{circles}</g>')
    svg = (f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%;">'
           f'<defs><filter id="glow" x="-20%" y="-20%" width="140%" height="140%"><feGaussianBlur stdDeviation="{14 if dark else 12}"/></filter></defs>'
           f'<style>circle{{r:{r}px}} .glow circle{{r:{r * 7:.1f}px}} .noise circle{{r:{r * 0.8:.1f}px}}</style>'
           + ''.join(parts) + '</svg>')
    return svg, members, colours


def smooth_path(poly):
    """Closed Catmull-Rom spline through the polygon's vertices, as cubic Béziers."""
    n = len(poly)
    if n < 3:
        return ''
    d = [f'M{poly[0][0]:.1f} {poly[0][1]:.1f}']
    for i in range(n):
        p0 = poly[(i - 1) % n]; p1 = poly[i]; p2 = poly[(i + 1) % n]; p3 = poly[(i + 2) % n]
        c1 = (p1[0] + (p2[0] - p0[0]) / 6, p1[1] + (p2[1] - p0[1]) / 6)
        c2 = (p2[0] - (p3[0] - p1[0]) / 6, p2[1] - (p3[1] - p1[1]) / 6)
        d.append(f'C{c1[0]:.1f} {c1[1]:.1f} {c2[0]:.1f} {c2[1]:.1f} {p2[0]:.1f} {p2[1]:.1f}')
    return ' '.join(d) + ' Z'


def datamap_layers(w, h, members, colours, labels, counts=None, sub_labels=None, dark=False,
                   selected=None, font='IBM Plex Sans Condensed', halo=None, boundaries=True,
                   label_ids=None, label_bounds=None):
    """Soft cluster boundaries and size-scaled labels with halos and leader lines.
    `labels` maps cluster index -> text; `sub_labels` maps cluster index -> [(text, dx, dy)]."""
    ink = '#eef0f2' if dark else '#1c1f23'
    halo = halo or ('rgba(12,14,16,0.85)' if dark else 'rgba(255,255,255,0.85)')
    out = [f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%; pointer-events:none;">']
    sizes = {ci: len(pts) for ci, pts in members.items()}
    mx = max(sizes.values()) or 1
    cents = {}
    for ci, pts in members.items():
        if len(pts) < 5:
            continue
        cx = sum(p[0] for p in pts) / len(pts); cy = sum(p[1] for p in pts) / len(pts)
        cents[ci] = (cx, cy)
        if boundaries and (label_ids is None or ci in label_ids):
            inner = sorted(pts, key=lambda p: math.hypot(p[0] - cx, p[1] - cy))[:int(len(pts) * 0.9)]
            poly = expand(hull(inner), cx, cy, by=7)
            d = smooth_path(poly)
            sel = ci == selected
            out.append(f'<path d="{d}" fill="{colours[ci]}" fill-opacity="{0.16 if sel else 0.09}" stroke="{colours[ci]}" '
                       f'stroke-opacity="{0.9 if sel else 0.35}" stroke-width="{2 if sel else 1.2}" stroke-linejoin="round"/>')
    # labels: size by cluster size, greedy vertical overlap avoidance, leader line when displaced
    placed = []
    order = sorted((ci for ci in cents if labels.get(ci)), key=lambda c: -sizes[c])
    for ci in order:
        text = labels[ci]
        cnt = f' · {counts[ci]}' if counts and ci in counts else ''
        scale = min(1.0, max(0.7, w / 1100))
        fs = (12 + 10 * math.sqrt(sizes[ci] / mx)) * scale
        bw = 0.52 * fs * len(text + cnt) + 8; bh = fs * 1.3
        cx, cy = cents[ci]
        x0, x1 = label_bounds or (0, w)
        x = min(max(cx, x0 + bw / 2 + 8), x1 - bw / 2 - 8)
        y = min(max(cy, bh), h - bh)
        for _ in range(6):
            moved = False
            for (px, py, pw, ph) in placed:
                if abs(px - x) < (pw + bw) / 2 and abs(py - y) < (ph + bh) / 2:
                    y = py + (ph + bh) / 2 + 4 if y >= py else py - (ph + bh) / 2 - 4
                    moved = True
            if not moved:
                break
        placed.append((x, y, bw, bh))
        if abs(y - cy) > bh:
            out.append(f'<line x1="{cx:.1f}" y1="{cy:.1f}" x2="{x:.1f}" y2="{y - bh / 2 if y > cy else y + bh / 2:.1f}" stroke="{ink}" stroke-opacity="0.45" stroke-width="1"/>')
            out.append(f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="2.5" fill="{ink}" fill-opacity="0.6"/>')
        out.append(f'<text x="{x:.1f}" y="{y:.1f}" text-anchor="middle" dominant-baseline="middle" font-family="{font}, sans-serif" '
                   f'font-size="{fs:.1f}" font-weight="600" letter-spacing="0.01em" fill="{ink}" paint-order="stroke" stroke="{halo}" '
                   f'stroke-width="{fs * 0.32:.1f}" stroke-linejoin="round">{text}<tspan font-weight="500" fill-opacity="0.75">{cnt}</tspan></text>')
        for (st, dx, dy) in (sub_labels or {}).get(ci, []):
            out.append(f'<text x="{x + dx:.1f}" y="{y + dy:.1f}" text-anchor="middle" dominant-baseline="middle" font-family="{font}, sans-serif" '
                       f'font-size="{12.5 * scale:.1f}" font-weight="500" fill="{ink}" fill-opacity="0.85" paint-order="stroke" stroke="{halo}" '
                       f'stroke-width="3.5" stroke-linejoin="round">{st}</text>')
    out.append('</svg>')
    return ''.join(out)


# ----------------------------------------------------------------------------- density-based rendering (v2)
def _field(pts, w, h, cell, blur_passes=2):
    nx, ny = int(w // cell) + 2, int(h // cell) + 2
    f = [[0.0] * nx for _ in range(ny)]
    for x, y in pts:
        i, j = int(x // cell), int(y // cell)
        if 0 <= i < nx and 0 <= j < ny:
            f[j][i] += 1.0
    for _ in range(blur_passes):
        g = [[0.0] * nx for _ in range(ny)]
        for j in range(ny):
            for i in range(nx):
                s = 0.0; n = 0
                for dj in (-1, 0, 1):
                    for di in (-1, 0, 1):
                        jj, ii = j + dj, i + di
                        if 0 <= jj < ny and 0 <= ii < nx:
                            s += f[jj][ii] * (2 if (di == 0 and dj == 0) else 1); n += (2 if (di == 0 and dj == 0) else 1)
                g[j][i] = s / n
        f = g
    return f, nx, ny


def density_wash(w, h, members, colours, cell=18, dark=False, alpha=None):
    """A smooth per-cluster density wash: a coarse grid of cells, opacity by density, blurred once.
    Reads as DataMapPlot's glow — a field, not a halo per point."""
    alpha = alpha if alpha is not None else (0.22 if dark else 0.09)
    parts = []
    for ci, pts in members.items():
        if not pts:
            continue
        f, nx, ny = _field(pts, w, h, cell, blur_passes=2)
        mx = max(max(row) for row in f) or 1
        rects = []
        for j in range(ny):
            for i in range(nx):
                v = f[j][i] / mx
                if v > 0.04:
                    rects.append(f'<rect x="{i * cell}" y="{j * cell}" width="{cell}" height="{cell}" opacity="{min(1.0, v * 1.4):.2f}"/>')
        parts.append(f'<g fill="{colours[ci]}">{"".join(rects)}</g>')
    return (f'<g filter="url(#wash)" opacity="{alpha}">' + ''.join(parts) + '</g>',
            f'<filter id="wash" x="-10%" y="-10%" width="120%" height="120%"><feGaussianBlur stdDeviation="{cell * 0.9:.1f}"/></filter>')


def contour(pts, w, h, cell=12, level=0.22):
    """The cluster's own density contour as a closed polygon — marching squares over a blurred
    grid, the longest loop kept. Organic and concave where the cluster is."""
    f, nx, ny = _field(pts, w, h, cell, blur_passes=3)
    mx = max(max(row) for row in f) or 1
    t = level * mx
    segs = []
    def interp(pa, va, pb, vb):
        d = (vb - va) or 1e-9
        s = (t - va) / d
        return (pa[0] + (pb[0] - pa[0]) * s, pa[1] + (pb[1] - pa[1]) * s)
    for j in range(ny - 1):
        for i in range(nx - 1):
            x0, y0 = (i + 0.5) * cell, (j + 0.5) * cell
            x1, y1 = x0 + cell, y0 + cell
            v = [f[j][i], f[j][i + 1], f[j + 1][i + 1], f[j + 1][i]]  # tl tr br bl
            idx = (v[0] > t) | ((v[1] > t) << 1) | ((v[2] > t) << 2) | ((v[3] > t) << 3)
            if idx in (0, 15):
                continue
            top = interp((x0, y0), v[0], (x1, y0), v[1]); right = interp((x1, y0), v[1], (x1, y1), v[2])
            bottom = interp((x0, y1), v[3], (x1, y1), v[2]); left = interp((x0, y0), v[0], (x0, y1), v[3])
            table = {1: [(left, top)], 2: [(top, right)], 3: [(left, right)], 4: [(right, bottom)], 5: [(left, top), (right, bottom)],
                     6: [(top, bottom)], 7: [(left, bottom)], 8: [(bottom, left)], 9: [(top, bottom)], 10: [(top, left), (bottom, right)],
                     11: [(right, bottom)], 12: [(right, left)], 13: [(top, right)], 14: [(left, top)]}
            segs.extend(table[idx])
    # chain segments into loops
    key = lambda p: (round(p[0], 1), round(p[1], 1))
    adj = {}
    for a, b in segs:
        adj.setdefault(key(a), []).append(b); adj.setdefault(key(b), []).append(a)
    seen = set(); loops = []
    for start in list(adj):
        if start in seen:
            continue
        loop = []; cur = start; prev = None
        while True:
            seen.add(cur); loop.append(cur)
            nxt = [key(p) for p in adj.get(cur, []) if key(p) != prev and key(p) != cur]
            nxt = [p for p in nxt if p not in seen] or nxt
            if not nxt:
                break
            prev, cur = cur, nxt[0]
            if cur == start or cur in seen:
                break
        if len(loop) > 6:
            loops.append(loop)
    if not loops:
        return []
    return max(loops, key=len)


def datamap_layers2(w, h, members, colours, labels, counts=None, sub_labels=None, dark=False, selected=None,
                    font='IBM Plex Sans', boundaries='all', label_bounds=None, ink=None, halo=None,
                    fs_range=(12.5, 19), descriptions=None):
    """Boundaries traced from density (selected cluster only, by default) and quieter labels: the UI
    face, semibold, a thin halo in the background colour, counts smaller and lighter."""
    ink = ink or ('#eceef0' if dark else '#24272b')
    halo = halo or ('rgba(12,14,17,0.7)' if dark else 'rgba(247,247,244,0.75)')
    out = [f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%; pointer-events:none;">']
    sizes = {ci: len(pts) for ci, pts in members.items()}
    mx = max(sizes.values()) or 1
    cents = {ci: (sum(p[0] for p in pts) / len(pts), sum(p[1] for p in pts) / len(pts)) for ci, pts in members.items() if pts}
    draw = [] if boundaries == 'none' else ([selected] if boundaries == 'selected' and selected is not None else list(cents))
    levels = (0.12, 0.32, 0.58)
    for ci in draw:
        if ci not in cents:
            continue
        sel = ci == selected
        for li, lv in enumerate(levels):
            poly = contour(members[ci], w, h, level=lv)
            if len(poly) < 8:
                continue
            d = smooth_path(poly[::2])
            base = (0.22 if dark else 0.16) + 0.06 * li
            op = (0.55 + 0.15 * li) if sel else base
            fill = f' fill="{colours[ci]}" fill-opacity="{0.07 if sel else 0.0}"' if li == 0 else ' fill="none"'
            out.append(f'<path d="{d}"{fill} stroke="{colours[ci]}" stroke-opacity="{op:.2f}" '
                       f'stroke-width="{1.2 if sel else 0.8}" stroke-linejoin="round"/>')
    scale = min(1.0, max(0.75, w / 1100))
    placed = []
    for ci in sorted((c for c in cents if labels.get(c)), key=lambda c: -sizes[c]):
        text = labels[ci]
        cnt = counts[ci] if counts and ci in counts else ''
        lo, hi = fs_range
        fs = (lo + (hi - lo) * math.sqrt(sizes[ci] / mx)) * scale
        bw = 0.56 * fs * len(text) + (0.5 * fs * (len(cnt) + 2) if cnt else 0) + 8; bh = fs * 1.35
        cx, cy = cents[ci]
        x0, x1 = label_bounds or (0, w)
        x = min(max(cx, x0 + bw / 2 + 8), x1 - bw / 2 - 8)
        y = min(max(cy, bh), h - bh)
        for _ in range(6):
            moved = False
            for (px, py, pw, ph) in placed:
                if abs(px - x) < (pw + bw) / 2 and abs(py - y) < (ph + bh) / 2:
                    y = py + (ph + bh) / 2 + 4 if y >= py else py - (ph + bh) / 2 - 4
                    moved = True
            if not moved:
                break
        placed.append((x, y, bw, bh))
        if abs(y - cy) > bh:
            ye = y - bh / 2 if y > cy else y + bh / 2
            out.append(f'<line x1="{cx:.1f}" y1="{cy:.1f}" x2="{x:.1f}" y2="{ye:.1f}" stroke="{ink}" stroke-opacity="0.4" stroke-width="0.8"/>')
            out.append(f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="2" fill="{ink}" fill-opacity="0.5"/>')
        cnt_t = f'<tspan font-weight="400" font-size="{fs * 0.82:.1f}" fill-opacity="0.72" dx="{fs * 0.35:.1f}">{cnt}</tspan>' if cnt else ''
        out.append(f'<text x="{x:.1f}" y="{y:.1f}" text-anchor="middle" dominant-baseline="middle" font-family="{font}, sans-serif" '
                   f'font-size="{fs:.1f}" font-weight="600" letter-spacing="-0.005em" fill="{ink}" paint-order="stroke" stroke="{halo}" '
                   f'stroke-width="2.5" stroke-linejoin="round">{text}{cnt_t}</text>')
        desc = (descriptions or {}).get(ci)
        if desc:
            out.append(f'<text x="{x:.1f}" y="{y + bh * 0.78:.1f}" text-anchor="middle" dominant-baseline="middle" font-family="{font}, sans-serif" '
                       f'font-size="{11 * scale:.1f}" font-style="italic" fill="{ink}" fill-opacity="0.7" paint-order="stroke" stroke="{halo}" '
                       f'stroke-width="2" stroke-linejoin="round">{desc}</text>')
        for (st, dx, dy) in (sub_labels or {}).get(ci, []):
            out.append(f'<text x="{x + dx:.1f}" y="{y + dy + (bh * 0.6 if desc else 0):.1f}" text-anchor="middle" dominant-baseline="middle" font-family="{font}, sans-serif" '
                       f'font-size="{11.5 * scale:.1f}" font-weight="500" fill="{ink}" fill-opacity="0.8" paint-order="stroke" stroke="{halo}" '
                       f'stroke-width="2" stroke-linejoin="round">{st}</text>')
    out.append('</svg>')
    return ''.join(out)


def datamap2(w, h, clusters, seed=7, n=1600, r=1.5, dark=False, wash=True, wash_alpha=None):
    """Points plus a density wash. Returns (svg, members, colours)."""
    rnd = random.Random(seed)
    cl = [(cx * w, cy * h, sx * w, sy * h, wt, cls) for cx, cy, sx, sy, wt, cls in clusters]
    colours = position_colours(cl, w, h, dark=dark)
    total_w = sum(c[4] for c in cl)
    members = {}
    for ci, (cx, cy, sx, sy, wt, cls) in enumerate(cl):
        k = int(n * wt / total_w)
        pts = []
        for _ in range(k):
            t = rnd.gauss(0, 1)
            x = cx + rnd.gauss(0, sx) * (1 + 0.15 * abs(t)); y = cy + rnd.gauss(0, sy) * (1 + 0.15 * abs(t))
            if 0 < x < w and 0 < y < h:
                pts.append((round(x, 1), round(y, 1)))
        members[ci] = pts
    bg = [(round(rnd.uniform(0, w), 1), round(rnd.uniform(0, h), 1)) for _ in range(n // 7)]
    defs = ''
    parts = []
    if wash:
        g, filt = density_wash(w, h, members, colours, dark=dark, alpha=wash_alpha)
        defs = f'<defs>{filt}</defs>'
        parts.append(g)
    noise_c = '#4c5157' if dark else '#c3c3bd'
    parts.append(f'<g fill="{noise_c}" opacity="0.55" class="noise">' + ''.join(f'<circle cx="{x}" cy="{y}"/>' for x, y in bg) + '</g>')
    po = 0.78 if dark else 0.68
    for ci, pts in members.items():
        parts.append(f'<g fill="{colours[ci]}" opacity="{po}">' + ''.join(f'<circle cx="{x}" cy="{y}"/>' for x, y in pts) + '</g>')
    svg = (f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" xmlns="http://www.w3.org/2000/svg" '
           f'style="position:absolute; inset:0; width:100%; height:100%;">{defs}'
           f'<style>circle{{r:{r}px}} .noise circle{{r:{r * 0.75:.1f}px}}</style>' + ''.join(parts) + '</svg>')
    return svg, members, colours
