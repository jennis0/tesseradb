import type {Rgb} from '@tesseradb/deck';

/**
 * Hue, saturation and value, for the colour picker's custom area: hue in degrees from 0 to 360,
 * saturation and value from 0 to 1.
 */
export type Hsv = [number, number, number];

/** RGB as HSV. A grey keeps hue 0. */
export function hsvOf([r, g, b]: Rgb): Hsv {
  const [R, G, B] = [r / 255, g / 255, b / 255];
  const max = Math.max(R, G, B);
  const min = Math.min(R, G, B);
  const d = max - min;
  let h = 0;
  if (d > 0) {
    if (max === R) h = ((G - B) / d) % 6;
    else if (max === G) h = (B - R) / d + 2;
    else h = (R - G) / d + 4;
  }
  return [(h * 60 + 360) % 360, max === 0 ? 0 : d / max, max];
}

/** HSV as RGB, each channel rounded to an integer. */
export function rgbOfHsv([h, s, v]: Hsv): Rgb {
  const c = v * s;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = v - c;
  const [r, g, b] = h < 60 ? [c, x, 0] : h < 120 ? [x, c, 0] : h < 180 ? [0, c, x] : h < 240 ? [0, x, c] : h < 300 ? [x, 0, c] : [c, 0, x];
  return [Math.round((r + m) * 255), Math.round((g + m) * 255), Math.round((b + m) * 255)];
}
