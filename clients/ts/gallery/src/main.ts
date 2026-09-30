import '@tesseradb/components';
import './gallery.css';
import {SECTIONS, type Context, type Section, type Specimen} from './specimens.js';

/**
 * The gallery page. Every choice is in the query string, so a screenshot script can address any
 * combination: `scheme` (light, dark), `theme` (default, editorial, console), `width` (natural,
 * 280, 420, wide) and `el` (all, or one element's name without `tessera-`); `state`, where given,
 * keeps only the specimens whose state contains it, so a page holds as few maps as a shot needs;
 * `shoot` unsticks the header for a screenshot script. A change reloads the page, so every element starts from its own
 * store again.
 */

const CHOICES = {
  scheme: ['light', 'dark'],
  theme: ['default', 'editorial', 'console'],
  width: ['natural', '280', '420', 'wide'],
  el: ['all', ...SECTIONS.map((s) => s.name)]
} as const;
type Choice = keyof typeof CHOICES;

const params = new URLSearchParams(location.search);
const chosen = (key: Choice): string => {
  const value = params.get(key);
  return value && (CHOICES[key] as readonly string[]).includes(value) ? value : CHOICES[key][0];
};
const state = {scheme: chosen('scheme'), theme: chosen('theme'), width: chosen('width'), el: chosen('el')};

const root = document.documentElement;
root.style.colorScheme = state.scheme;
root.dataset.theme = state.theme;
if (params.has('shoot')) root.dataset.shoot = '';

function controls(): HTMLElement {
  const header = document.createElement('header');
  header.className = 'controls';
  header.innerHTML = '<h1>Tessera components</h1>';
  const labels: Record<Choice, string> = {scheme: 'Scheme', theme: 'Theme', width: 'Width', el: 'Element'};
  for (const key of Object.keys(CHOICES) as Choice[]) {
    const label = document.createElement('label');
    label.textContent = labels[key];
    const select = document.createElement('select');
    select.name = key;
    for (const value of CHOICES[key]) select.add(new Option(key === 'el' && value !== 'all' ? `tessera-${value}` : value, value, false, state[key] === value));
    select.addEventListener('change', () => {
      params.set(key, select.value);
      location.search = params.toString();
    });
    label.append(select);
    header.append(label);
  }
  if (state.theme !== 'default') {
    const hint = document.createElement('span');
    hint.className = 'hint';
    hint.textContent = 'This theme sets its own ground, so the scheme does not move it.';
    header.append(hint);
  }
  return header;
}

/** The frame's width: the specimen's own where it pins one, else the chosen one, else natural. */
function frameWidth(s: Specimen): string {
  if (s.pinned) return `${s.pinned}px`;
  switch (state.width) {
    case '280':
    case '420':
      return `${state.width}px`;
    case 'wide':
      return '960px';
    default:
      return s.wide ? '100%' : s.fit ? 'fit-content' : '336px';
  }
}

type Mounted = {section: Section; specimen: Specimen; el: HTMLElement | null; frame: HTMLElement};

function renderSection(section: Section, ctx: Context, mounted: Mounted[]): HTMLElement {
  const box = document.createElement('section');
  box.className = 'element';
  box.dataset.el = section.name;
  box.innerHTML = `<h2>&lt;${section.tag}&gt;</h2>`;
  if (section.note) {
    const note = document.createElement('p');
    note.className = 'note';
    note.textContent = section.note;
    box.append(note);
  }
  const grid = document.createElement('div');
  grid.className = 'specimens';
  const only = params.get('state');
  for (const specimen of section.specimens) {
    if (only && !specimen.state.includes(only)) continue;
    const figure = document.createElement('figure');
    figure.className = `specimen${specimen.wide && state.width === 'natural' ? ' wide' : ''}`;
    figure.dataset.state = specimen.state;
    const caption = document.createElement('figcaption');
    caption.innerHTML = `<code>${section.tag}</code> · `;
    caption.append(specimen.state);
    const frame = document.createElement('div');
    frame.className = `frame${section.name === 'count' ? ' inline' : ''}`;
    frame.style.width = frameWidth(specimen);
    let el: HTMLElement | null = null;
    try {
      el = specimen.build(ctx);
      frame.append(el);
    } catch (error) {
      frame.classList.add('failed');
      frame.textContent = String(error);
      console.error(`gallery: ${section.tag} · ${specimen.state} did not build`, error);
    }
    figure.append(caption, frame);
    grid.append(figure);
    mounted.push({section, specimen, el, frame});
  }
  box.append(grid);
  return box;
}

async function main(): Promise<void> {
  document.body.append(controls());
  const stage = document.createElement('main');
  stage.className = 'stage';
  document.body.append(stage);
  const ctx: Context = {scheme: state.scheme === 'dark' ? 'dark' : 'light'};
  const mounted: Mounted[] = [];
  for (const section of SECTIONS) {
    if (state.el !== 'all' && state.el !== section.name) continue;
    stage.append(renderSection(section, ctx, mounted));
  }
  await Promise.all(
    mounted.map(async ({section, specimen, el, frame}) => {
      if (!el) return;
      try {
        await (el as HTMLElement & {updateComplete?: Promise<unknown>}).updateComplete;
        await specimen.ready?.(el);
      } catch (error) {
        frame.classList.add('failed');
        console.error(`gallery: ${section.tag} · ${specimen.state} did not settle`, error);
      }
    })
  );
  // Lit updates scheduled by the waits above, then two frames for the paint.
  await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  root.dataset.ready = 'true';
}

void main();
