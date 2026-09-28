/**
 * How big and how solid a mark is drawn, from how many marks are resident and how far in the
 * camera is.
 *
 * At a million marks a viewport is a density picture, so each mark is small and translucent and
 * the ones under it show through. As the count falls each mark is drawn larger and more solid,
 * down to about 1.5 px at 0.7 alpha for a few hundred. Zoom adds a little to both.
 *
 * The count is every mark the slab holds for the frame plus the stand-ins, render margin
 * included. It is a screen quantity used only for presentation, and is not masked.
 */
export type MarkStyle = {
  /** Radius in pixels. */
  radius: number;
  /** The alpha a mark is composited at, 0–1, as a fraction of the colour's own alpha. */
  alpha: number;
  /**
   * Whether deck feathers the disc's edge. deck's feather is a half-pixel ramp either side of the
   * radius, so below {@link ANTIALIAS_ABOVE_PX} it is most of the mark, and thousands of
   * overlapping small marks bloom into a glow. It is off there.
   */
  antialiasing: boolean;
};

/** The density scale: `0` at a hundred marks or fewer, `1` at a million or more. */
function density(marks: number): number {
  const t = (Math.log10(Math.max(1, marks)) - 2) / 4;
  return Math.min(1, Math.max(0, t));
}

/** Above this radius the half-pixel feather is an edge; below it, it is most of the mark. */
export const ANTIALIAS_ABOVE_PX = 1.4;

/**
 * The style for `marks` resident at `zoom` (deck's zoom: 0 when the world fills 512 px, +1 per
 * doubling). `fixedRadius` and `fixedAlpha` pin the radius and the alpha where a host or viewer
 * chose them; each one not pinned follows the count. At about 1,600 marks this gives 1.53 px at
 * 0.65 alpha; at a million, 1.10 px at 0.34 before the zoom term.
 */
export function markStyle(marks: number, zoom: number, fixedRadius: number | null = null, fixedAlpha: number | null = null): MarkStyle {
  const t = density(marks);
  const z = Math.min(10, Math.max(0, zoom));
  const radius = fixedRadius ?? 1.1 + 0.6 * (1 - t) + 0.05 * z;
  const alpha = fixedAlpha === null ? Math.min(0.9, 0.34 + 0.44 * (1 - t) + 0.015 * z) : Math.min(1, Math.max(0, fixedAlpha));
  return {
    radius: Math.round(radius * 100) / 100,
    alpha: Math.round(alpha * 1000) / 1000,
    antialiasing: radius >= ANTIALIAS_ABOVE_PX
  };
}

/**
 * The `opacity` prop that composites at `alpha`. deck raises its `opacity` prop to `1 / 2.2`
 * before the shader multiplies by it (its "visually linear" gamma), so the prop is the alpha
 * raised to 2.2.
 */
export function deckOpacity(alpha: number): number {
  return Math.pow(Math.min(1, Math.max(0, alpha)), 2.2);
}
