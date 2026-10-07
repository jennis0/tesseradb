import {describe, expect, it} from 'vitest';
import {DEFAULT_PALETTE, NEUTRAL, PALETTES, artifactColour, artifactColours, paletteOfSize, paletteSize, type PaletteName} from '../src/palette.js';

const hexOf = (c: readonly number[]) => `#${c.slice(0, 3).map((v) => v.toString(16).padStart(2, '0')).join('')}`;

describe('the cluster palettes', () => {
  it('are Okabe-Ito, Tableau 10, Tableau 20 and Kelly, with 8, 10, 20 and 22 colours, Tableau 10 by default', () => {
    expect(Object.keys(PALETTES)).toEqual(['okabe-ito', 'tableau10', 'tableau20', 'kelly']);
    expect((Object.keys(PALETTES) as PaletteName[]).map(paletteSize)).toEqual([8, 10, 20, 22]);
    expect(DEFAULT_PALETTE).toBe('tableau10');
    expect(PALETTES['okabe-ito'].colours.map(hexOf)).toEqual(['#e69f00', '#56b4e9', '#009e73', '#f0e442', '#0072b2', '#d55e00', '#cc79a7', '#000000']);
    expect(PALETTES.kelly.colours.map(hexOf).slice(0, 3)).toEqual(['#f2f3f4', '#222222', '#f3c300']);
    expect(PALETTES.tableau20.colours.map(hexOf).at(-1)).toBe('#d7b5a6');
  });

  it('colour a slot from the palette at its index, and a null slot or one past the end neutral', () => {
    expect(hexOf(artifactColour('tableau10', 0))).toBe('#4e79a7');
    expect(hexOf(artifactColour('tableau10', 9))).toBe('#bab0ac');
    expect(hexOf(artifactColour('okabe-ito', 7))).toBe('#000000');
    expect(artifactColour('tableau10', null)).toEqual(NEUTRAL);
    expect(artifactColour('okabe-ito', 8)).toEqual(NEUTRAL);
  });

  it('take a colour chosen for an artifact in place of its slot’s, slotted or not', () => {
    const chosen = [1, 2, 3, 255] as const;
    expect(artifactColour('tableau10', 4, chosen)).toEqual(chosen);
    expect(artifactColour('tableau10', null, chosen)).toEqual(chosen);
  });

  it('colour each ordinal by its slot in the palette of the size it was served for, a size no palette has neutral, and a colour chosen for its layer over it', () => {
    const colours = artifactColours(
      [
        {ordinal: 1, layer: 'a', tesseraId: 10n, slot: 2, paletteSize: 10},
        {ordinal: 2, layer: 'a', tesseraId: 11n, slot: 2, paletteSize: 8},
        {ordinal: 3, layer: 'a', tesseraId: 12n, slot: null, paletteSize: 10},
        {ordinal: 4, layer: 'a', tesseraId: 13n, slot: 5, paletteSize: 10},
        {ordinal: 5, layer: 'a', tesseraId: 14n, slot: 2, paletteSize: 9},
        {ordinal: 6, layer: 'a', tesseraId: 15n, slot: null, paletteSize: null},
        {ordinal: 7, layer: 'b', tesseraId: 13n, slot: 2, paletteSize: 10}
      ],
      new Map([['a', new Map([[13n, [9, 9, 9, 255] as const]])]])
    );
    expect(colours.get(7)).toEqual(PALETTES.tableau10.colours[2]);
    expect(colours.get(1)).toEqual(PALETTES.tableau10.colours[2]);
    expect(colours.get(2)).toEqual(PALETTES['okabe-ito'].colours[2]);
    expect(colours.get(3)).toEqual(NEUTRAL);
    expect(colours.get(4)).toEqual([9, 9, 9, 255]);
    expect(colours.get(5)).toEqual(NEUTRAL);
    expect(colours.get(6)).toEqual(NEUTRAL);
  });

  it('are named by their sizes, each a size of its own', () => {
    expect([8, 10, 20, 22, 9, null].map(paletteOfSize)).toEqual(['okabe-ito', 'tableau10', 'tableau20', 'kelly', null, null]);
  });
});
