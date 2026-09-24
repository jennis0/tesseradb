import type {Device} from '@luma.gl/core';

/**
 * A device that counts: every buffer and texture it hands out records its writes, so a test can
 * assert that a colouring change rewrote the lookup texture and not the attributes. Nothing draws.
 */
export type FakeDevice = Device & {
  bufferWrites: number;
  textureWrites: number;
  /** Writes per buffer, keyed by creation order — the slab creates positions, colours, picking, ordinals. */
  writesByBuffer: number[];
  /** The region of each texture write, in rows — what a test asserts a patch wrote and no more. */
  textureRegions: {y: number; height: number}[];
};

/** What the fake device hands out: a buffer or texture that records whether it was destroyed. */
export type FakeResource = {destroyed: boolean};

export function fakeDevice(): FakeDevice {
  const dev = {
    bufferWrites: 0,
    textureWrites: 0,
    writesByBuffer: [] as number[],
    textureRegions: [] as {y: number; height: number}[],
    createBuffer(_props: unknown) {
      const at = dev.writesByBuffer.length;
      dev.writesByBuffer.push(0);
      const buffer = {
        destroyed: false,
        write: () => {
          dev.bufferWrites += 1;
          dev.writesByBuffer[at]! += 1;
        },
        destroy: () => {
          buffer.destroyed = true;
        }
      };
      return buffer;
    },
    createTexture(props: {width: number; height: number}) {
      const texture = {
        destroyed: false,
        width: props.width,
        height: props.height,
        writeData: (_data: unknown, options?: {y?: number; height?: number}) => {
          dev.textureWrites += 1;
          dev.textureRegions.push({y: options?.y ?? 0, height: options?.height ?? props.height});
        },
        destroy: () => {
          texture.destroyed = true;
        }
      };
      return texture;
    }
  };
  return dev as unknown as FakeDevice;
}
