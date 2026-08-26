import type {Artifact} from './types.js';

/**
 * The artifact palette (design §5.10, decision 0099's default): **positional**. An artifact's
 * hue comes from its angle about the corpus extent's centre and its lightness from its distance,
 * so a colour is a pure function of where a cluster sits — stable under pan, stable across
 * responses, converging as a zoom narrows the angles on screen. A palette assigned in served
 * order would recolour every cluster whenever one entered the view.
 *
 * The alternative, at the owner's choice, is **spread**: hues spaced evenly over the served set
 * ordered by angle, which separates neighbours better and changes with the served set.
 *
 * Geometry arrives in the wire's 32-bit grid units (contracts §3.2); the centre is the grid's
 * midpoint, which is the corpus extent's centre by construction of the quantisation.
 */

export type Rgba = readonly [number, number, number, number];

export type PaletteKind = 'positional' | 'spread';
/** The ground a colour is drawn on — the boards' light and dark values differ in lightness. */
export type PaletteScheme = 'light' | 'dark';

/** The grid is 2³² per axis; the extent's centre and its half-width in the same units. */
export const GRID32_CENTRE = 2 ** 31;

const ALPHA = 220;

/** `h` in degrees, `s` and `l` in `[0, 1]` — to RGB bytes. */
export function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  const hue = ((h % 360) + 360) % 360;
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const x = c * (1 - Math.abs(((hue / 60) % 2) - 1));
  const m = l - c / 2;
  let r = 0;
  let g = 0;
  let b = 0;
  if (hue < 60) [r, g, b] = [c, x, 0];
  else if (hue < 120) [r, g, b] = [x, c, 0];
  else if (hue < 180) [r, g, b] = [0, c, x];
  else if (hue < 240) [r, g, b] = [0, x, c];
  else if (hue < 300) [r, g, b] = [x, 0, c];
  else [r, g, b] = [c, 0, x];
  return [Math.round((r + m) * 255), Math.round((g + m) * 255), Math.round((b + m) * 255)];
}

/** The angle about the grid's centre, in degrees `[0, 360)`, and the distance as a fraction of the half-width. */
export function polarOf(centroid: readonly [number, number]): {angle: number; radius: number} {
  const dx = centroid[0] - GRID32_CENTRE;
  const dy = centroid[1] - GRID32_CENTRE;
  const angle = ((Math.atan2(dy, dx) * 180) / Math.PI + 360) % 360;
  const radius = Math.min(1, Math.hypot(dx, dy) / GRID32_CENTRE);
  return {angle, radius};
}

/** The lightness and saturation of a hue on each ground (`gen.py`'s `position_colours`). */
function shade(radius: number, scheme: PaletteScheme): [number, number] {
  return scheme === 'dark' ? [0.62, 0.64 + 0.08 * radius] : [0.58, 0.4 - 0.08 * radius];
}

/** The hue offset the boards apply, so the map's quadrants take the boards' colours. */
const HUE_OFFSET = (0.5 + 0.45) * 360;

/** The positional colour of one centroid. */
export function positionalColour(centroid: readonly [number, number], scheme: PaletteScheme = 'dark'): Rgba {
  const {angle, radius} = polarOf(centroid);
  const [s, l] = shade(radius, scheme);
  const [r, g, b] = hslToRgb(angle + HUE_OFFSET, s, l);
  return [r, g, b, ALPHA];
}

/** The neutral: *not known here yet*, which is what a point wears until the wire names it. */
export const NEUTRAL: Rgba = [118, 126, 140, 200];

/**
 * A colour per served artifact, by ordinal. Positional colours ignore the set; spread colours
 * are assigned around the hue circle in angle order, so neighbours on the map are neighbours in
 * hue and the whole circle is used however few are served. An artifact with no centroid takes
 * the neutral.
 */
export function artifactColours(
  served: readonly {ordinal: number; artifact: Artifact}[],
  kind: PaletteKind,
  scheme: PaletteScheme = 'dark'
): Map<number, Rgba> {
  const out = new Map<number, Rgba>();
  if (kind === 'positional') {
    for (const {ordinal, artifact} of served) {
      out.set(ordinal, artifact.centroid ? positionalColour(artifact.centroid, scheme) : NEUTRAL);
    }
    return out;
  }
  const placed = served.filter((s) => s.artifact.centroid !== null).map((s) => ({...s, ...polarOf(s.artifact.centroid!)}));
  placed.sort((a, b) => a.angle - b.angle);
  placed.forEach(({ordinal, radius}, i) => {
    const [sat, l] = shade(radius, scheme);
    const [r, g, b] = hslToRgb((i * 360) / placed.length + HUE_OFFSET, sat, l);
    out.set(ordinal, [r, g, b, ALPHA]);
  });
  for (const {ordinal, artifact} of served) if (!artifact.centroid) out.set(ordinal, NEUTRAL);
  return out;
}
