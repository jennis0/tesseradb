// Reproduce the zoom-out overdraw burst and dump the audit's forensics.
import {chromium} from 'playwright';
const browser = await chromium.launch({args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox']});
const page = await browser.newPage({viewport: {width: 1000, height: 700}});
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));
await page.goto('http://localhost:5173/?trace=1', {waitUntil: 'load'});
await page.waitForFunction(() => (window.__tesseraProbe?.marks ?? 0) > 0, null, {timeout: 120000});
await page.waitForTimeout(3000);
// Deep in: six notches at centre, settling between, so exact ground accumulates at depth.
for (let i = 0; i < 6; i++) { await page.mouse.move(500, 350); await page.mouse.wheel(0, -300); await page.waitForTimeout(1200); }
// A sideways pan at depth so the deep layer holds ground the coarse view will stand in from.
await page.mouse.move(500, 350); await page.mouse.down();
for (let s = 1; s <= 10; s++) { await page.mouse.move(500 - 30 * s, 350); await page.waitForTimeout(30); }
await page.mouse.up(); await page.waitForTimeout(1500);
// Hard zoom out — the reported artifact: squares not reducing.
for (let i = 0; i < 8; i++) { await page.mouse.wheel(0, 300); await page.waitForTimeout(150); }
await page.mouse.down(); for (let s = 1; s <= 6; s++) { await page.mouse.move(500 + 25 * s, 350); await page.waitForTimeout(40); } await page.mouse.up();
await page.waitForTimeout(2500);
const dump = await page.evaluate(() => window.__tesseraTrace.toJSON());
const t0 = dump.events[0].t;
const od = dump.events.filter((e) => e.kind === 'overdraw');
console.log('overdraw events:', od.length);
for (const e of od) console.log(`t=${((e.t - t0) / 1000).toFixed(1)}s d=${e.depth} r=${e.r} was(e=${e.wasExact},s=${e.wasStand}) cur(e=${e.curExact},s=${e.curStand}) srcΔ=${e.src}`);
const dens = dump.events.filter((e) => e.kind === 'density' && (e.up > 100 || e.worst > 4));
console.log('density bursts:', dens.length);
for (const e of dens.slice(0, 12)) console.log(`t=${((e.t - t0) / 1000).toFixed(1)}s d=${e.depth} n=${e.n} up=${e.up} dn=${e.down} worst=${e.worst}`);
const der = dump.events.filter((e) => e.kind === 'derive' || e.kind === 'refresh');
console.log('derive/refresh depths tail:', der.slice(-25).map((e) => `${e.kind[0]}${e.depth ?? '?'}`).join(' '));
console.log('errors:', errors.length ? errors.slice(0, 3) : 'none');
await browser.close();
