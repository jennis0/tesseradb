import type {Texture} from '@luma.gl/core';
import type {DefaultProps} from '@deck.gl/core';
import {ScatterplotLayer, type ScatterplotLayerProps} from '@deck.gl/layers';
import {LUT_SHIFT, LUT_WIDTH} from './lut.js';

/**
 * The mark layer: deck's `ScatterplotLayer` with two more per-instance attributes, the session
 * ordinal and the highlight bit, and a lookup-texture read in its vertex shader.
 *
 * With `useLut` on, the fill colour is `lut[ordinal]`; off, it is the colour the slab wrote per
 * mark. The switch is a uniform, so changing colouring uploads nothing. The ordinal is a `float32`
 * attribute, exact to 2²⁴, which covers any range the texture can hold.
 */

const lutUniforms = {
  name: 'tesseraLut',
  vs: /* glsl */ `\
layout(std140) uniform tesseraLutUniforms {
  float useLut;
  highp int lutMask;
  highp int lutShift;
  float dull;
  float dullGrey;
  float dullRadius;
  float litRadius;
  float pass;
} tesseraLut;
uniform sampler2D lutTexture;
`,
  fs: '',
  source: '',
  uniformTypes: {
    useLut: 'f32',
    lutMask: 'i32',
    lutShift: 'i32',
    dull: 'f32',
    dullGrey: 'f32',
    dullRadius: 'f32',
    litRadius: 'f32',
    pass: 'f32'
  } as const
};

/**
 * What an unmatched mark's alpha is multiplied by while a highlight is set.
 *
 * Under a highlight every served mark is still drawn. The unmatched ones are set back four ways,
 * because on a dense region overdraw composites a lower alpha alone back to nearly full: this
 * alpha, a smaller radius ({@link DULL_RADIUS_SCALE} against {@link LIT_RADIUS_SCALE}), a colour
 * pulled {@link DULL_GREY} of the way to grey, and a separate draw pass under the matched marks
 * ({@link HighlightPass}). Matched marks keep their colour exactly.
 */
export const DULL_ALPHA = 0.12;

/** How far a dulled mark's colour is pulled to neutral grey, 0 = its own colour, 1 = grey. */
export const DULL_GREY = 0.8;

/** The grey a dulled mark is pulled towards, mid grey so it reads on a light or dark ground. */
export const DULL_NEUTRAL = 0.5;

/** A dulled mark's radius, as a fraction of the frame's mark radius. */
export const DULL_RADIUS_SCALE = 0.8;

/** A lit mark's radius, as a multiple of the frame's mark radius. */
export const LIT_RADIUS_SCALE = 1.7;

/**
 * Which marks a pass draws. With no highlight there is one pass, `'all'`.
 *
 * Under a highlight the marks are drawn twice, `'dull'` then `'lit'`, each over the whole set.
 * Instances are rasterised in buffer order, so in one pass a lit mark is buried under every
 * unlit mark later in the slab. Each pass collapses the marks it does not draw to zero size in
 * the vertex shader.
 */
export type HighlightPass = 'all' | 'dull' | 'lit';

/** {@link HighlightPass} as the shader's uniform: 0 all, 1 dull only, 2 lit only. */
function passCode(pass: HighlightPass): number {
  return pass === 'dull' ? 1 : pass === 'lit' ? 2 : 0;
}

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
  /** Which half of the highlight this pass draws; see {@link HighlightPass}. */
  highlightPass?: HighlightPass;
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
    highlightPass: 'all'
  };

  override getShaders() {
    const shaders = super.getShaders();
    return {
      ...shaders,
      modules: [...shaders.modules, lutUniforms],
      inject: {
        'vs:#decl': /* glsl */ `in float instanceOrdinals;
in float instanceHighlights;
// 1.0 where this mark is drawn by the pass in force, 0.0 where the other pass draws it.
float tesseraInPass(float lit) {
  return tesseraLut.pass < 0.5 ? 1.0 : step(abs(tesseraLut.pass - 1.0 - lit), 0.5);
}`,
        // A lit mark is drawn larger than a dulled one; the pass not drawing this mark collapses
        // it to zero size before rasterisation.
        'vs:DECKGL_FILTER_SIZE': /* glsl */ `\
float tesseraLit = step(0.5, instanceHighlights);
size *= mix(tesseraLut.dullRadius, tesseraLut.litRadius, tesseraLit) * tesseraInPass(tesseraLit);
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
color.rgb = mix(mix(color.rgb, vec3(${DULL_NEUTRAL.toFixed(3)}), tesseraLut.dullGrey), color.rgb, lit);
color.a *= mix(tesseraLut.dull, 1.0, lit) * tesseraInPass(lit);
`
      }
    };
  }

  override initializeState(): void {
    super.initializeState();
    this.getAttributeManager()!.addInstanced({
      instanceOrdinals: {size: 1, type: 'float32', accessor: 'getOrdinal', defaultValue: 0},
      // 1 is matched, which every mark is when no highlight is set.
      instanceHighlights: {size: 1, type: 'float32', accessor: 'getHighlight', defaultValue: 1}
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
          pass: this.props.highlighting ? passCode(this.props.highlightPass ?? 'all') : 0
        }
      });
      if (texture) model.setBindings({lutTexture: texture});
    }
    super.draw(opts);
  }
}
