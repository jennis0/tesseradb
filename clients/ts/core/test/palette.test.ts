import {describe, expect, it} from 'vitest';
import {GRID32_CENTRE, NEUTRAL, artifactColours, hslToRgb, polarOf, positionalColour, type Placed} from '../src/palette.js';

const at = (x: number, y: number, ordinal: number): Placed => ({ordinal, centroid: [x, y]});

describe('the positional palette (§5.10, decision 0099)', () => {
  it('is a function of the centroid alone — stable under pan and across served sets', () => {
    const c = GRID32_CENTRE;
    const a = positionalColour([c + 1e9, c]);
    expect(positionalColour([c + 1e9, c])).toEqual(a);
    // The same artifact coloured among different neighbours keeps its colour.
    const alone = artifactColours([at(c + 1e9, c, 1)], 'positional').get(1);
    const crowded = artifactColours([at(c + 1e9, c, 1), at(c, c + 1e9, 2), at(c - 1e9, c, 3)], 'positional').get(1);
    expect(alone).toEqual(a);
    expect(crowded).toEqual(a);
  });

  it('takes hue from the angle about the extent centre and lightness from the distance', () => {
    const c = GRID32_CENTRE;
    expect(polarOf([c + 100, c]).angle).toBe(0);
    expect(polarOf([c, c + 100]).angle).toBe(90);
    expect(polarOf([c, c]).radius).toBe(0);
    expect(polarOf([0, c]).radius).toBe(1);
    const lum = (rgb: readonly number[]) => rgb[0]! + rgb[1]! + rgb[2]!;
    // The same hue at both distances; on a dark ground the rim is lighter, on a light ground
    // the rim is deeper (the boards' `position_colours`).
    expect(lum(positionalColour([2 ** 32 - 1, c], 'dark'))).toBeGreaterThan(lum(positionalColour([c + 1000, c], 'dark')));
    expect(lum(positionalColour([c + 1000, c], 'light'))).toBeGreaterThan(lum(positionalColour([2 ** 32 - 1, c], 'light')));
    expect(positionalColour([c + 1000, c], 'light')).not.toEqual(positionalColour([c + 1000, c], 'dark'));
  });

  it('spreads hues evenly over the served set in angle order, and neutral for an artifact with no centroid', () => {
    const c = GRID32_CENTRE;
    const spread = artifactColours([at(c + 10, c, 1), at(c, c + 10, 2), at(c - 10, c, 3), {ordinal: 4, centroid: null}], 'spread');
    // Evenly spaced from the boards' hue origin (an offset of 0.95 of the circle), on the dark ground.
    const origin = 0.95 * 360;
    expect(spread.get(1)!.slice(0, 3)).toEqual(hslToRgb(origin, 0.62, 0.64));
    expect(spread.get(2)!.slice(0, 3)).toEqual(hslToRgb(origin + 120, 0.62, 0.64));
    expect(spread.get(3)!.slice(0, 3)).toEqual(hslToRgb(origin + 240, 0.62, 0.64));
    expect(spread.get(4)).toEqual(NEUTRAL);
  });

  it('hslToRgb agrees with the usual anchors', () => {
    expect(hslToRgb(0, 1, 0.5)).toEqual([255, 0, 0]);
    expect(hslToRgb(120, 1, 0.5)).toEqual([0, 255, 0]);
    expect(hslToRgb(240, 1, 0.25)).toEqual([0, 0, 128]);
    expect(hslToRgb(0, 0, 1)).toEqual([255, 255, 255]);
  });
});
