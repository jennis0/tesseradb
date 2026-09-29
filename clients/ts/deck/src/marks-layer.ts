import type {Texture} from '@luma.gl/core';
import type {DefaultProps} from '@deck.gl/core';
import {ScatterplotLayer, type ScatterplotLayerProps} from '@deck.gl/layers';
import {LUT_SHIFT, LUT_WIDTH} from './lut.js';

/**
 * The mark layer: deck's `ScatterplotLayer` with three more per-instance attributes, the session
 * ordinal, the highlight bit and the size fraction, and a lookup-texture read in its vertex shader.
 *
 * With `useLut` on, the fill colour is `lut[ordinal]`; off, it is the colour the slab wrote per
 * mark. The switch is a uniform, so changing colouring uploads nothing. The ordinal is a `float32`
 * attribute, exact to 2²⁴, which covers any range the texture can hold.
 */

/** The uniform block the vertex and fragment stages share. */
const LUT_BLOCK = /* glsl */ `\
layout(std140) uniform tesseraLutUniforms {
  float useLut;
  highp int lutMask;
  highp int lutShift;
  float dull;
  float dullGrey;
  float dullRadius;
  float litRadius;
  float pass;
  float sizing;
  float sizeMin;
  float sizeMax;
  vec3 dullColour;
} tesseraLut;
`;

const lutUniforms = {
  name: 'tesseraLut',
  vs: `${LUT_BLOCK}uniform sampler2D lutTexture;
`,
  fs: LUT_BLOCK,
  source: '',
  uniformTypes: {
    useLut: 'f32',
    lutMask: 'i32',
    lutShift: 'i32',
    dull: 'f32',
    dullGrey: 'f32',
    dullRadius: 'f32',
    litRadius: 'f32',
    pass: 'f32',
    sizing: 'f32',
    sizeMin: 'f32',
    sizeMax: 'f32',
    dullColour: 'vec3<f32>'
  } as const
};

/**
 * What an unmatched mark's alpha is multiplied by while a highlight is set.
 *
 * Under a highlight every served mark is still drawn. The unmatched ones are set back three ways:
 * this alpha, a light grey in place of their colour ({@link DULL_COLOUR}), and a separate draw pass
 * under the matched marks ({@link HighlightPass}). The matched marks keep their colour exactly,
 * draw larger ({@link LIT_RADIUS_SCALE}) and carry a glow of their own colour, so a sparse
 * highlight can be found at a glance.
 */
export const DULL_ALPHA = 0.5;

/** How far a dulled mark's colour is taken to {@link DULL_COLOUR}, 0 = its own colour, 1 = the grey. */
export const DULL_GREY = 1;

/**
 * The colour of a dulled mark per ground, as RGB from 0 to 1: a light grey on a light ground and
 * a dark grey on a dark one, each a little off the ground so the unmatched marks still show where
 * they are.
 */
export const DULL_COLOUR: Record<'light' | 'dark', [number, number, number]> = {
  light: [201 / 255, 201 / 255, 195 / 255],
  dark: [74 / 255, 78 / 255, 85 / 255]
};

/** A dulled mark's radius, as a fraction of the frame's mark radius. */
export const DULL_RADIUS_SCALE = 1;

/** A lit mark's radius, as a multiple of the frame's mark radius. */
export const LIT_RADIUS_SCALE = 1.4;

/** The glow's radius, as a multiple of a lit mark's radius. */
export const GLOW_RADIUS_SCALE = 4;

/**
 * The glow's alpha at its centre, as a fraction of the lit mark's. It falls linearly to nothing at
 * its edge, and follows the frame's alpha, so a dense highlight's glows overlap into a tint and do
 * not cover the map.
 */
export const GLOW_ALPHA = 0.5;

/**
 * Which marks a pass draws. With no highlight there is one pass, `'all'`.
 *
 * Under a highlight the marks are drawn three times, `'dull'`, `'glow'` then `'lit'`, each over the
 * whole set. Instances are rasterised in buffer order, so in one pass a lit mark is buried under
 * every unlit mark later in the slab. Each pass collapses the marks it does not draw to zero size
 * in the vertex shader. `'glow'` draws the lit marks as wide soft discs under the lit pass.
 */
export type HighlightPass = 'all' | 'dull' | 'glow' | 'lit';

/** {@link HighlightPass} as the shader's uniform: 0 all, 1 dull only, 2 lit only, 3 the glow. */
function passCode(pass: HighlightPass): number {
  return pass === 'dull' ? 1 : pass === 'lit' ? 2 : pass === 'glow' ? 3 : 0;
}

/** The width in pixels of the ring a mark with no value draws, under sizing by a column. */
export const HOLLOW_RING_PX = 1.5;

export type MarksLayerProps = ScatterplotLayerProps & {
  /** Whether the fill colour comes from the lookup texture rather than the colour attribute. */
  useLut?: boolean;
  lutTexture?: Texture | null;
  /** The ordinal per mark, bound as a buffer through `data.attributes`. */
  getOrdinal?: number | ((d: unknown) => number);
  /** Whether a highlight is set. Off, every mark draws at its own alpha and size. */
  highlighting?: boolean;
  /** The highlight bit per mark, bound as a buffer the same way the ordinal is. */
  getHighlight?: number | ((d: unknown) => number);
  /** Which part of the highlight this pass draws; see {@link HighlightPass}. */
  highlightPass?: HighlightPass;
  /** The colour a dulled mark takes, RGB from 0 to 1; see {@link DULL_COLOUR}. */
  dullColour?: [number, number, number];
  /**
   * The radii in pixels of the smallest and largest size, under sizing by a column; null draws
   * every mark at `getRadius`. Under sizing, `getRadius` must be `max`.
   */
  sizing?: {min: number; max: number} | null;
  /** The size fraction per mark, from 0 to 1, or -1 for no value, bound as the ordinal is. */
  getSize?: number | ((d: unknown) => number);
};

export class MarksLayer extends ScatterplotLayer<unknown, MarksLayerProps> {
  static override layerName = 'TesseraMarksLayer';
  static override defaultProps: DefaultProps<MarksLayerProps> = {
    ...(ScatterplotLayer.defaultProps as DefaultProps<MarksLayerProps>),
    useLut: false,
    lutTexture: null,
    getOrdinal: {type: 'accessor', value: 0},
    highlighting: false,
    getHighlight: {type: 'accessor', value: 1},
    highlightPass: 'all',
    dullColour: DULL_COLOUR.dark,
    sizing: null,
    getSize: {type: 'accessor', value: 0}
  };

  override getShaders() {
    const shaders = super.getShaders();
    return {
      ...shaders,
      modules: [...shaders.modules, lutUniforms],
      inject: {
        'vs:#decl': /* glsl */ `in float instanceOrdinals;
in float instanceHighlights;
in float instanceSizes;
out float vTesseraHollow;
// 1.0 where this mark is drawn by the pass in force, 0.0 where another pass draws it. The dull pass
// draws the unlit marks; the lit and glow passes draw the lit ones.
float tesseraInPass(float lit) {
  if (tesseraLut.pass < 0.5) return 1.0;
  return tesseraLut.pass < 1.5 ? 1.0 - lit : lit;
}
// The multiple of the layer's radius this mark draws at. A lit mark is drawn larger than a dulled
// one, and its glow larger still; the passes not drawing this mark collapse it to zero size. Under
// sizing the layer's radius is the largest size, and the mark's own size is a fraction of it.
float tesseraSizeFactor() {
  float lit = step(0.5, instanceHighlights);
  float glow = step(2.5, tesseraLut.pass);
  float own = tesseraLut.sizing > 0.5 ? mix(tesseraLut.sizeMin, tesseraLut.sizeMax, max(instanceSizes, 0.0)) / tesseraLut.sizeMax : 1.0;
  return own * mix(tesseraLut.dullRadius, tesseraLut.litRadius, lit) * mix(1.0, ${GLOW_RADIUS_SCALE.toFixed(3)}, glow) * tesseraInPass(lit);
}`,
        'vs:DECKGL_FILTER_SIZE': /* glsl */ `\
size *= tesseraSizeFactor();
`,
        // The fragment stage measures the disc against the radius drawn, and draws a mark with no
        // value under sizing as a ring.
        'vs:#main-end': /* glsl */ `\
outerRadiusPixels *= tesseraSizeFactor();
vTesseraHollow = tesseraLut.sizing > 0.5 && instanceSizes < 0.0 ? 1.0 : 0.0;
`,
        'fs:#decl': /* glsl */ `in float vTesseraHollow;
`,
        // A ring's inside is clear except to picking, so a mark with no value is found where it is drawn.
        'fs:#main-end': /* glsl */ `\
if (vTesseraHollow > 0.5 && picking.isActive < 0.5) {
  float inner = outerRadiusPixels - ${HOLLOW_RING_PX.toFixed(3)};
  fragColor.a *= smoothstep(inner - 0.5, inner + 0.5, length(unitPosition) * outerRadiusPixels);
}
`,
        // The colour from whichever source is on, then the highlight over it. With no highlight
        // `dull` is 1.0 and `dullGrey` 0.0, so the colour passes through.
        'vs:DECKGL_FILTER_COLOR': /* glsl */ `\
if (tesseraLut.useLut > 0.5) {
  int o = int(instanceOrdinals + 0.5);
  ivec2 at = ivec2(o & tesseraLut.lutMask, o >> tesseraLut.lutShift);
  vec4 lutColour = texelFetch(lutTexture, at, 0);
  color = vec4(lutColour.rgb, lutColour.a * layer.opacity);
}
float lit = step(0.5, instanceHighlights);
color.rgb = mix(mix(color.rgb, tesseraLut.dullColour, tesseraLut.dullGrey), color.rgb, lit);
color.a *= mix(tesseraLut.dull, 1.0, lit) * tesseraInPass(lit) * mix(1.0, ${GLOW_ALPHA.toFixed(3)}, step(2.5, tesseraLut.pass));
`,
        // The glow fades from its centre to nothing at its edge.
        'fs:DECKGL_FILTER_COLOR': /* glsl */ `\
if (tesseraLut.pass > 2.5) {
  color.a *= 1.0 - min(1.0, length(geometry.uv));
}
`
      }
    };
  }

  override initializeState(): void {
    super.initializeState();
    this.getAttributeManager()!.addInstanced({
      instanceOrdinals: {size: 1, type: 'float32', accessor: 'getOrdinal', defaultValue: 0},
      // 1 is matched, which every mark is when no highlight is set.
      instanceHighlights: {size: 1, type: 'float32', accessor: 'getHighlight', defaultValue: 1},
      instanceSizes: {size: 1, type: 'float32', accessor: 'getSize', defaultValue: 0}
    });
  }

  override draw(opts: Parameters<ScatterplotLayer['draw']>[0]): void {
    const model = (this.state as {model?: {shaderInputs: {setProps(p: unknown): void}; setBindings(b: Record<string, unknown>): void}}).model;
    const texture = this.props.lutTexture ?? null;
    if (model) {
      model.shaderInputs.setProps({
        tesseraLut: {
          useLut: this.props.useLut && texture ? 1 : 0,
          lutMask: LUT_WIDTH - 1,
          lutShift: LUT_SHIFT,
          dull: this.props.highlighting ? DULL_ALPHA : 1,
          dullGrey: this.props.highlighting ? DULL_GREY : 0,
          dullRadius: this.props.highlighting ? DULL_RADIUS_SCALE : 1,
          litRadius: this.props.highlighting ? LIT_RADIUS_SCALE : 1,
          pass: this.props.highlighting ? passCode(this.props.highlightPass ?? 'all') : 0,
          dullColour: this.props.dullColour ?? DULL_COLOUR.dark,
          sizing: this.props.sizing ? 1 : 0,
          sizeMin: this.props.sizing?.min ?? 1,
          sizeMax: Math.max(this.props.sizing?.max ?? 1, 1e-3)
        }
      });
      if (texture) model.setBindings({lutTexture: texture});
    }
    super.draw(opts);
  }
}
