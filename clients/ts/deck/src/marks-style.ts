/**
 * How big and how solid a mark is drawn, as a function of how many marks are resident and how
 * far in the camera is (design client-components §5.10; the boards' `datamap2`).
 *
 * At a million marks a viewport is a density picture and each mark must let the ones under it
 * show through, so the marks are small and translucent; as the count falls each mark stands for
 * more and is drawn larger and more solid, until a few hundred draw as the boards' 1.5 px dots at
 * 0.7 alpha. Zoom adds a little to both, since a mark at depth 8 is a sample of a smaller area
 * than the same mark at the overview and should read as one.
 *
 * The count is the resident set — every mark the slab holds for the frame plus the stand-ins —
 * which includes the render margin: a screen fact, never a masked quantity, and only ever a
 * presentation input.
 */
export type MarkStyle = {
  /** Radius in pixels. */
  radius: number;
  /** The alpha a mark is composited at, 0–1, as a fraction of the colour's own alpha. */
  alpha: number;
};

/** The density scale: `0` at a hundred marks or fewer, `1` at a million or more. */
function density(marks: number): number {
  const t = (Math.log10(Math.max(1, marks)) - 2) / 4;
  return Math.min(1, Math.max(0, t));
}

/**
 * The style for `marks` resident at `zoom` (deck's: 0 when the world fills 512 px, +1 per
 * doubling). `fixedRadius` pins the radius where a host asked for one; the alpha still follows.
 */
export function markStyle(marks: number, zoom: number, fixedRadius: number | null = null): MarkStyle {
  const t = density(marks);
  const z = Math.min(10, Math.max(0, zoom));
  const radius = fixedRadius ?? 1.2 + 1.0 * (1 - t) + 0.08 * z;
  const alpha = Math.min(0.95, 0.5 + 0.3 * (1 - t) + 0.02 * z);
  return {radius: Math.round(radius * 100) / 100, alpha: Math.round(alpha * 1000) / 1000};
}

/**
 * The `opacity` prop that composites at `alpha`. deck raises its `opacity` prop to `1 / 2.2`
 * before the shader multiplies by it (its "visually linear" gamma), so the prop is the alpha
 * raised to 2.2.
 */
export function deckOpacity(alpha: number): number {
  return Math.pow(Math.min(1, Math.max(0, alpha)), 2.2);
}
