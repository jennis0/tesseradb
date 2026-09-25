/**
 * The artifact palette. The default, `positional`, takes an artifact's hue from its angle about the
 * extent's centre and its lightness from its distance, so a colour depends only on where the
 * cluster sits and does not change under pan or across responses. A palette assigned in served
 * order would recolour every cluster whenever one entered the view. `spread` spaces hues evenly
 * over the set in angle order, which separates neighbours better and changes with the set.
 *
 * Geometry is in the wire's 32-bit grid units; the grid's midpoint is the extent's centre.
 */

/**
 * A colour as red, green, blue and alpha bytes, each from 0 to 255.
 *
 * @category Coordinates and colour
 */
export type Rgba = readonly [number, number, number, number];

/**
 * How artifacts are coloured. `positional` takes an artifact's hue from its angle about the
 * extent's centre and its lightness from its distance, so its colour depends only on where it sits
 * and does not change as the map pans or new answers arrive. `spread` spaces hues evenly round the
 * colour wheel over every artifact the session holds, in angle order, which separates neighbours
 * better and recolours as the set changes.
 *
 * @category Coordinates and colour
 */
export type PaletteKind = 'positional' | 'spread';
/**
 * The ground colours are drawn on, `light` or `dark`. The palettes choose lightness against it.
 *
 * @category Coordinates and colour
 */
export type PaletteScheme = 'light' | 'dark';

/** The grid is 2³² per axis; the extent's centre and its half-width in the same units. @internal */
export const GRID32_CENTRE = 2 ** 31;

const ALPHA = 220;

/** `h` in degrees, `s` and `l` in `[0, 1]`, to RGB bytes. @internal */
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

/** The angle about the grid's centre, in degrees `[0, 360)`, and the distance as a fraction of the half-width. @internal */
export function polarOf(centroid: readonly [number, number]): {angle: number; radius: number} {
  const dx = centroid[0] - GRID32_CENTRE;
  const dy = centroid[1] - GRID32_CENTRE;
  const angle = ((Math.atan2(dy, dx) * 180) / Math.PI + 360) % 360;
  const radius = Math.min(1, Math.hypot(dx, dy) / GRID32_CENTRE);
  return {angle, radius};
}

/** The saturation and lightness of a hue on each ground. */
function shade(radius: number, scheme: PaletteScheme): [number, number] {
  return scheme === 'dark' ? [0.62, 0.64 + 0.08 * radius] : [0.58, 0.4 - 0.08 * radius];
}

const HUE_OFFSET = (0.5 + 0.45) * 360;

/** The positional colour of one centroid. @internal */
export function positionalColour(centroid: readonly [number, number], scheme: PaletteScheme = 'dark'): Rgba {
  const {angle, radius} = polarOf(centroid);
  const [s, l] = shade(radius, scheme);
  const [r, g, b] = hslToRgb(angle + HUE_OFFSET, s, l);
  return [r, g, b, ALPHA];
}

/**
 * The colour of a point whose artifact has no colour: one the session does not know yet, or one
 * with no centroid.
 *
 * @category Coordinates and colour
 */
export const NEUTRAL: Rgba = [118, 126, 140, 200];

/**
 * One artifact's colour under the positional palette, which does not depend on the set. A caller
 * extending a colour map for a newly named ordinal uses this to match {@link artifactColours}.
 *
 * @internal
 */
export function positionalEntry(centroid: readonly [number, number] | null, scheme: PaletteScheme = 'dark'): Rgba {
  return centroid ? positionalColour(centroid, scheme) : NEUTRAL;
}

/** An artifact a colour is wanted for: its ordinal in the session table, and where it sits. @internal */
export type Placed = {ordinal: number; centroid: readonly [number, number] | null};

/**
 * A colour per artifact, by ordinal. Spread colours go round the hue circle in angle order, so map
 * neighbours are hue neighbours. An artifact with no centroid takes the neutral.
 *
 * The caller passes every artifact the session table holds: a band held under a coarser cut names
 * artifacts the current view was not served, and its points take their colours. Under `spread` the
 * hues therefore move with the table.
 *
 * @internal
 */
export function artifactColours(
  placedIn: readonly Placed[],
  kind: PaletteKind,
  scheme: PaletteScheme = 'dark'
): Map<number, Rgba> {
  const out = new Map<number, Rgba>();
  if (kind === 'positional') {
    for (const {ordinal, centroid} of placedIn) out.set(ordinal, positionalEntry(centroid, scheme));
    return out;
  }
  const placed = placedIn.filter((s) => s.centroid !== null).map((s) => ({...s, ...polarOf(s.centroid!)}));
  placed.sort((a, b) => a.angle - b.angle);
  placed.forEach(({ordinal, radius}, i) => {
    const [sat, l] = shade(radius, scheme);
    const [r, g, b] = hslToRgb((i * 360) / placed.length + HUE_OFFSET, sat, l);
    out.set(ordinal, [r, g, b, ALPHA]);
  });
  for (const {ordinal, centroid} of placedIn) if (!centroid) out.set(ordinal, NEUTRAL);
  return out;
}
