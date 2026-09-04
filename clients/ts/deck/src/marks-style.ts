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
  /**
   * Whether deck feathers the disc's edge. deck's antialiasing is a **half-pixel** ramp either
   * side of the radius, so a mark at 1.1 px is more feather than disc: its footprint reaches
   * 1.6 px and most of that area is a soft ramp. Thousands of those overlapping is the glow the
   * owner read as blooming, and it is worst exactly where the marks are smallest. So the feather
   * is kept where it buys a smooth edge on a mark big enough to have one, and dropped below
   * {@link ANTIALIAS_ABOVE_PX}, where it is the mark.
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
 * The style for `marks` resident at `zoom` (deck's: 0 when the world fills 512 px, +1 per
 * doubling). `fixedRadius` pins the radius where a host asked for one; the alpha still follows.
 *
 * **Recalibrated on the owner's review of the 2.4M map, 2026-08-26.** The band was
 * `1.2 + 1.0(1−t) + 0.08z` px at `0.5 + 0.3(1−t) + 0.02z`, which at a million resident marks put
 * a 1.2 px mark at half alpha under a half-pixel feather — a dense region bloomed rather than
 * reading as dense. The low end is pinned where it was: at the boards' own count (about 1,600
 * marks, `t ≈ 0.29`) this is 1.53 px at 0.65, against the boards' 1.5 px at 0.68 light / 0.78
 * dark. Everything above it comes down — at a million marks 1.10 px at 0.34 before the zoom term,
 * against 1.20 px at 0.50 — and the feather goes with it, which is about **half the ink** a mark
 * laid down before: alpha times the footprint the feather reaches, 0.34 × π·1.1² against
 * 0.5 × π·1.45².
 */
export function markStyle(marks: number, zoom: number, fixedRadius: number | null = null): MarkStyle {
  const t = density(marks);
  const z = Math.min(10, Math.max(0, zoom));
  const radius = fixedRadius ?? 1.1 + 0.6 * (1 - t) + 0.05 * z;
  const alpha = Math.min(0.9, 0.34 + 0.44 * (1 - t) + 0.015 * z);
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
