// Correctness only — swiftshader invalidates timings, not counts or errors.
import {chromium} from 'playwright';
const browser = await chromium.launch({args: ['--use-gl=swiftshader','--enable-unsafe-swiftshader','--disable-gpu-sandbox']});
const page = await browser.newPage({viewport: {width: 1000, height: 700}});
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));
page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
await page.goto('http://localhost:5173/?trace=1', {waitUntil: 'load'});
await page.waitForFunction(() => (window.__tesseraProbe?.marks ?? 0) > 0, null, {timeout: 90000});
await page.waitForTimeout(4000);
for (let i = 0; i < 3; i++) { await page.mouse.move(500,350); await page.mouse.wheel(0,-300); await page.waitForTimeout(900); }
await page.waitForTimeout(2000);
for (const dx of [-320, 320, -320]) {
  await page.mouse.move(500,350); await page.mouse.down();
  for (let s=1; s<=8; s++) { await page.mouse.move(500+dx*s/8, 350); await page.waitForTimeout(16); }
  await page.mouse.up(); await page.waitForTimeout(1400);
}
await page.mouse.wheel(0, 300); await page.waitForTimeout(1500);   // zoom out — the stale-stand-in case
const dump = await page.evaluate(() => window.__tesseraTrace.toJSON());
const kinds = {};
for (const e of dump.events) kinds[e.kind] = (kinds[e.kind] ?? 0) + 1;
console.log('events:', kinds);
const refresh = dump.events.filter((e) => e.kind === 'refresh');
const derive = dump.events.filter((e) => e.kind === 'derive');
console.log(`refresh: ${refresh.length} (mean ${refresh.length ? (refresh.reduce((a,e)=>a+e.ms,0)/refresh.length).toFixed(1) : '-'} ms), derive: ${derive.length} (mean ${derive.length ? (derive.reduce((a,e)=>a+e.ms,0)/derive.length).toFixed(1) : '-'} ms)`);
const split = dump.events.filter((e) => e.kind === 'split');
console.log(`split: ${split.length}, max single ${Math.max(0,...split.map(e=>e.ms)).toFixed(1)} ms (summed per response, sliced within)`);
console.log('bar:', await page.evaluate(() => document.querySelector('div[style*="fixed"] span')?.textContent ?? 'missing'));
console.log('marks:', await page.evaluate(() => window.__tesseraProbe));
console.log('errors:', errors.length ? errors.slice(0,4) : 'none');
await browser.close();
