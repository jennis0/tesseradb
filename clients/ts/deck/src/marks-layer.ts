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
} tesseraLut;
uniform sampler2D lutTexture;
`,
  fs: '',
  source: '',
  uniformTypes: {useLut: 'f32', lutMask: 'i32', lutShift: 'i32'} as const
};

export type MarksLayerProps = ScatterplotLayerProps & {
  /** Whether the fill colour comes from the lookup texture rather than the colour attribute. */
  useLut?: boolean;
  lutTexture?: Texture | null;
  /** The ordinal per mark — bound as a buffer through `data.attributes`, never read per mark. */
  getOrdinal?: number | ((d: unknown) => number);
};

export class MarksLayer extends ScatterplotLayer<unknown, MarksLayerProps> {
  static override layerName = 'TesseraMarksLayer';
  static override defaultProps: DefaultProps<MarksLayerProps> = {
    ...(ScatterplotLayer.defaultProps as DefaultProps<MarksLayerProps>),
    useLut: false,
    lutTexture: null,
    getOrdinal: {type: 'accessor', value: 0}
  };

  override getShaders() {
    const shaders = super.getShaders();
    return {
      ...shaders,
      modules: [...shaders.modules, lutUniforms],
      inject: {
        'vs:#decl': /* glsl */ `in float instanceOrdinals;`,
        'vs:DECKGL_FILTER_COLOR': /* glsl */ `\
if (tesseraLut.useLut > 0.5) {
  int o = int(instanceOrdinals + 0.5);
  ivec2 at = ivec2(o & tesseraLut.lutMask, o >> tesseraLut.lutShift);
  vec4 lutColour = texelFetch(lutTexture, at, 0);
  color = vec4(lutColour.rgb, lutColour.a * layer.opacity);
}
`
      }
    };
  }

  override initializeState(): void {
    super.initializeState();
    this.getAttributeManager()!.addInstanced({
      instanceOrdinals: {size: 1, type: 'float32', accessor: 'getOrdinal', defaultValue: 0}
    });
  }

  override draw(opts: Parameters<ScatterplotLayer['draw']>[0]): void {
    const model = (this.state as {model?: {shaderInputs: {setProps(p: unknown): void}; setBindings(b: Record<string, unknown>): void}}).model;
    const texture = this.props.lutTexture ?? null;
    if (model) {
      model.shaderInputs.setProps({tesseraLut: {useLut: this.props.useLut && texture ? 1 : 0, lutMask: LUT_WIDTH - 1, lutShift: LUT_SHIFT}});
      if (texture) model.setBindings({lutTexture: texture});
    }
    super.draw(opts);
  }
}
