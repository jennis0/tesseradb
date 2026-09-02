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
} tesseraLut;
uniform sampler2D lutTexture;
`,
  fs: '',
  source: '',
  uniformTypes: {useLut: 'f32', lutMask: 'i32', lutShift: 'i32', dull: 'f32'} as const
};

/**
 * What an unmatched mark's alpha is multiplied by while a highlight is set
 * (`highlight-and-hierarchy.md` §5.3).
 *
 * The dulled marks are still drawn — the map does not move and nothing is removed, which is the
 * whole difference between a highlight and a filter — so they have to stay legible as ground
 * while the matched ones read as the answer. Alpha alone, and not a desaturation: the colour of a
 * mark is the palette's answer about which cluster or which value it belongs to, and washing that
 * out would make the highlight change what the map says as well as what it emphasises.
 */
export const DULL_ALPHA = 0.22;

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
};

export class MarksLayer extends ScatterplotLayer<unknown, MarksLayerProps> {
  static override layerName = 'TesseraMarksLayer';
  static override defaultProps: DefaultProps<MarksLayerProps> = {
    ...(ScatterplotLayer.defaultProps as DefaultProps<MarksLayerProps>),
    useLut: false,
    lutTexture: null,
    getOrdinal: {type: 'accessor', value: 0},
    highlighting: false,
    getHighlight: {type: 'accessor', value: 1}
  };

  override getShaders() {
    const shaders = super.getShaders();
    return {
      ...shaders,
      modules: [...shaders.modules, lutUniforms],
      inject: {
        'vs:#decl': /* glsl */ `in float instanceOrdinals;
in float instanceHighlights;`,
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
color.a *= mix(tesseraLut.dull, 1.0, step(0.5, instanceHighlights));
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
          dull: this.props.highlighting ? DULL_ALPHA : 1
        }
      });
      if (texture) model.setBindings({lutTexture: texture});
    }
    super.draw(opts);
  }
}
