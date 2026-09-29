import {afterEach, describe, expect, it, vi} from 'vitest';
import {FloatingList} from '../src/float.js';

/** A list that opens over what sits below it: when it follows its control, and when it stops. */

afterEach(() => {
  vi.restoreAllMocks();
  document.body.innerHTML = '';
});

const rect = (top: number, height: number) => ({top, bottom: top + height, left: 0, right: 300, width: 300, height, x: 0, y: top, toJSON: () => ({})}) as DOMRect;

describe('a floating list', () => {
  it('follows its control each frame while open, and asks for no frame once it closes', () => {
    const frames: FrameRequestCallback[] = [];
    vi.spyOn(globalThis, 'requestAnimationFrame').mockImplementation((cb) => frames.push(cb));
    vi.spyOn(globalThis, 'cancelAnimationFrame').mockImplementation(() => {});
    const anchor = document.createElement('div');
    const list = Object.assign(document.createElement('div'), {showPopover: () => {}});
    document.body.append(anchor, list);
    let open = true;
    const float = new FloatingList(() => (open ? {list, anchor} : null));
    float.update();
    expect(frames).toHaveLength(1);
    frames.shift()!(0);
    expect(frames).toHaveLength(1);
    open = false;
    frames.shift()!(0);
    expect(frames).toHaveLength(0);
  });

  it('hides while its control is scrolled out of the card that holds it', () => {
    vi.spyOn(globalThis, 'requestAnimationFrame').mockImplementation(() => 1);
    const card = document.createElement('div');
    card.style.overflowY = 'auto';
    const anchor = document.createElement('div');
    card.append(anchor);
    const list = Object.assign(document.createElement('div'), {showPopover: () => {}});
    document.body.append(card, list);
    card.getBoundingClientRect = () => rect(0, 200);
    anchor.getBoundingClientRect = () => rect(250, 30);
    new FloatingList(() => ({list, anchor})).update();
    expect(list.style.visibility).toBe('hidden');
    anchor.getBoundingClientRect = () => rect(100, 30);
    new FloatingList(() => ({list, anchor})).update();
    expect(list.style.visibility).toBe('');
  });

  it('sits under its control in the card where the browser has no top layer', () => {
    const anchor = document.createElement('div');
    const list = document.createElement('div');
    Object.defineProperty(list, 'showPopover', {value: undefined});
    anchor.append(list);
    Object.defineProperty(anchor, 'offsetHeight', {value: 30});
    Object.defineProperty(anchor, 'offsetWidth', {value: 280});
    document.body.append(anchor);
    new FloatingList(() => ({list, anchor})).update();
    expect([list.style.position, list.style.top, list.style.width]).toEqual(['absolute', '34px', '280px']);
  });
});
