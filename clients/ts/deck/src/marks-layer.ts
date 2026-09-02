import type {Texture} from '@luma.gl/core';
import type {DefaultProps} from '@deck.gl/core';
import {ScatterplotLayer, type ScatterplotLayerProps} from '@deck.gl/layers';
import {LUT_SHIFT, LUT_WIDTH} from './lut.js';

/**
 * The mark layer: deck's `ScatterplotLayer` with one more per-instance attribute — the session
 * ordinal — and the lookup-texture read in its vertex shader (design §5.10).
 *
 * When `useLut` is on, the fill colour is `lut[ordinal]`; off, it is the column colour the slab
 * wrote per point. The switch is a uniform, so cluster-to-column and back cost no upload at all.
 * The ordinal travels as a `float32` attribute: integers are exact to 2²⁴ there, which is far
 * past any range the texture could hold, and it spares the integer-attribute path deck does not
 * otherwise use.
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
 * What an unmatched mark's alpha is multiplied by while a highlight is set
 * (`highlight-and-hierarchy.md` §5.3).
 *
 * The dulled marks are still drawn — the map does not move and nothing is removed, which is the
 * whole difference between a highlight and a filter — so they have to stay legible as ground
 * while the matched ones read as the answer.
 *
 * **Recalibrated on the owner's reading of the rung 3 map, 2026-09-02, where the highlight was
 * reported as showing no visible difference.** 0.22 is a mark's own alpha times 0.22, which on a
 * dense region is nothing of the sort: a million marks at 1.1 px overdraw the same pixels several
 * times over, and several coats of 0.22 composite back to a wash barely below the undulled one.
 * Three things carry the distinction instead of one, and the two new ones are what make it read
 * where overdraw is highest:
 *
 * - the alpha, now {@link DULL_ALPHA};
 * - the radius, {@link DULL_RADIUS_SCALE} against {@link LIT_RADIUS_SCALE} — a lit mark is drawn
 *   larger than an unlit one, so a lit point in dense ground is not the same disc in a different
 *   shade;
 * - the colour, pulled {@link DULL_GREY} of the way to neutral. The earlier note here argued
 *   against any desaturation, on the ground that a mark's colour is the palette's answer about
 *   which cluster it belongs to. That still holds for the lit marks, which keep their colour
 *   exactly; the dulled ones are ground, and reading a cluster's colour off ground the highlight
 *   has pushed to the back is not something the interface offers anyway.
 *
 * **The draw order carries the rest** — see {@link MarksLayerProps.highlightPass}. None of it
 * moves a single mark in or out of the draw: every mark the server served is still drawn, which
 * is `budget.ts`'s rule and I7's.
 */
export const DULL_ALPHA = 0.12;

/** How far a dulled mark's colour is pulled to neutral grey, 0 = its own colour, 1 = grey. */
export const DULL_GREY = 0.8;

/** The neutral a dulled mark is pulled towards — mid grey, so it reads on a light or dark ground. */
export const DULL_NEUTRAL = 0.5;

/** A dulled mark's radius, as a fraction of the frame's mark radius. */
export const DULL_RADIUS_SCALE = 0.8;

/** A lit mark's radius, as a multiple of the frame's mark radius. */
export const LIT_RADIUS_SCALE = 1.7;

/**
 * Which marks a pass draws. `'all'` is every map with no highlight set and costs nothing extra.
 *
 * Under a highlight the marks are drawn **twice**: `'dull'` over the whole set, then `'lit'` over
 * the whole set again. Instances are rasterised in buffer order, so a lit mark drawn in the one
 * pass is buried under every unlit mark that happens to sit later in the slab — which at a
 * million marks is most of them. The second pass puts every lit mark over every dulled one for
 * the cost of a vertex shader that collapses the marks it is not drawing to zero size.
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
  /** The ordinal per mark — bound as a buffer through `data.attributes`, never read per mark. */
  getOrdinal?: number | ((d: unknown) => number);
  /**
   * Whether a highlight is set. Off — the ordinary map — every mark draws at its own alpha and
   * the attribute is not read at all, so a client with no highlight pays nothing for this.
   */
  highlighting?: boolean;
  /** The highlight bit per mark, bound as a buffer the same way the ordinal is. */
  getHighlight?: number | ((d: unknown) => number);
  /**
   * Which half of the highlight this pass draws (see {@link HighlightPass}). `'all'` — the
   * default, and the only value a map with no highlight ever uses — draws every mark.
   */
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
        // A lit mark is drawn larger than a dulled one, and the pass that is not drawing this
        // mark collapses it to nothing — the cheapest discard there is, before rasterisation.
        'vs:DECKGL_FILTER_SIZE': /* glsl */ `\
float tesseraLit = step(0.5, instanceHighlights);
size *= mix(tesseraLut.dullRadius, tesseraLut.litRadius, tesseraLit) * tesseraInPass(tesseraLit);
`,
        // The colour first, from whichever source is on, then the highlight over it — so a
        // dulled mark is the same colour it would have been, at a lower alpha, under either
        // colouring. `dull` is 1.0 with no highlight set, which is the whole of the switch.
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
      // Defaults to 1 — *matched*, which is what every mark is when no highlight is set.
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
