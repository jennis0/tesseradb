/**
 * The cluster palettes. The server gives each cluster a slot below the palette's size, chosen so
 * that clusters drawn beside each other differ, and the same at any zoom, box, budget or filter.
 * The client maps a slot to the palette's colour at that index. A request names the palette's size
 * as `palette_size`, so the slots of one size mean nothing under another.
 */

/**
 * A colour as red, green, blue and alpha bytes, each from 0 to 255.
 *
 * @category Coordinates and colour
 */
export type Rgba = readonly [number, number, number, number];

/**
 * A cluster palette, by name: `okabe-ito` (Okabe-Ito, 8 colours, distinguishable under the common
 * colour-vision deficiencies), `tableau10` (Tableau 10, 10 colours), `tableau20` (Tableau 20, 20
 * colours in light and dark pairs) or `kelly` (Kelly's 22 colours of maximum contrast).
 *
 * @category Coordinates and colour
 */
export type PaletteName = 'okabe-ito' | 'tableau10' | 'tableau20' | 'kelly';

/**
 * A cluster palette: its title and a line describing it, as a menu shows them, and its colours,
 * whose number is the `palette_size` a request sends.
 *
 * @category Coordinates and colour
 */
export type Palette = {
  /** The palette's name as a menu shows it. */
  title: string;
  /** One line on the palette for a menu: how many colours it has, and what they are chosen for. */
  description: string;
  /** The colours, in slot order. */
  colours: readonly Rgba[];
};

const ALPHA = 220;

function hex(...colours: string[]): Rgba[] {
  return colours.map((c) => [parseInt(c.slice(1, 3), 16), parseInt(c.slice(3, 5), 16), parseInt(c.slice(5, 7), 16), ALPHA]);
}

/**
 * The cluster palettes, by name, in the order a menu lists them.
 *
 * @category Coordinates and colour
 */
export const PALETTES: Readonly<Record<PaletteName, Palette>> = {
  'okabe-ito': {
    title: 'Okabe-Ito',
    description: '8 colours · colour-blind safe',
    colours: hex('#e69f00', '#56b4e9', '#009e73', '#f0e442', '#0072b2', '#d55e00', '#cc79a7', '#000000')
  },
  tableau10: {
    title: 'Tableau 10',
    description: '10 colours',
    colours: hex('#4e79a7', '#f28e2b', '#e15759', '#76b7b2', '#59a14f', '#edc948', '#b07aa1', '#ff9da7', '#9c755f', '#bab0ac')
  },
  tableau20: {
    title: 'Tableau 20',
    description: '20 colours, in light and dark pairs',
    colours: hex(
      '#4e79a7', '#a0cbe8', '#f28e2b', '#ffbe7d', '#59a14f', '#8cd17d', '#b6992d', '#f1ce63', '#499894', '#86bcb6',
      '#e15759', '#ff9d9a', '#79706e', '#bab0ac', '#d37295', '#fabfd2', '#b07aa1', '#d4a6c8', '#9d7660', '#d7b5a6'
    )
  },
  kelly: {
    title: 'Kelly',
    description: '22 colours, most distinct',
    colours: hex(
      '#f2f3f4', '#222222', '#f3c300', '#875692', '#f38400', '#a1caf1', '#be0032', '#c2b280', '#848482', '#008856', '#e68fac',
      '#0067a5', '#f99379', '#604e97', '#f6a600', '#b3446c', '#dcd300', '#882d17', '#8db600', '#654522', '#e25822', '#2b3d26'
    )
  }
};

/**
 * The palette a store colours clusters with until another is chosen.
 *
 * @category Coordinates and colour
 */
export const DEFAULT_PALETTE: PaletteName = 'tableau10';

/**
 * How many colours `palette` has: the `palette_size` a request for its slots sends.
 *
 * @category Coordinates and colour
 */
export function paletteSize(palette: PaletteName): number {
  return PALETTES[palette].colours.length;
}

/**
 * The colour of a point whose artifact has no colour: one the session does not know yet, or one
 * served with no slot.
 *
 * @category Coordinates and colour
 */
export const NEUTRAL: Rgba = [118, 126, 140, 200];

/**
 * A cluster's colour: `chosen` where the host set one for it, else its slot's colour in `palette`,
 * else {@link NEUTRAL} for a null slot or one past the palette's end.
 *
 * @category Coordinates and colour
 */
export function artifactColour(palette: PaletteName, slot: number | null, chosen?: Rgba): Rgba {
  if (chosen) return chosen;
  return (slot === null ? undefined : PALETTES[palette].colours[slot]) ?? NEUTRAL;
}

/**
 * An artifact a colour is wanted for: its ordinal in the session table, its `tessera_id`, its slot
 * and the palette size the slot was served under.
 *
 * @internal
 */
export type Slotted = {ordinal: number; tesseraId: bigint; slot: number | null; paletteSize: number | null};

/**
 * A colour per artifact, by ordinal, under `palette`, with the colours `chosen` by `tessera_id` in
 * place of the palette's. A slot served under another palette size is no slot here.
 *
 * @internal
 */
export function artifactColours(artifacts: readonly Slotted[], palette: PaletteName, chosen: ReadonlyMap<bigint, Rgba> = new Map()): Map<number, Rgba> {
  const out = new Map<number, Rgba>();
  for (const a of artifacts) out.set(a.ordinal, slottedColour(a, palette, chosen));
  return out;
}

/** One artifact's colour, as {@link artifactColours} gives it. @internal */
export function slottedColour(a: Omit<Slotted, 'ordinal'>, palette: PaletteName, chosen: ReadonlyMap<bigint, Rgba>): Rgba {
  return artifactColour(palette, a.paletteSize === paletteSize(palette) ? a.slot : null, chosen.get(a.tesseraId));
}
