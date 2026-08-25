import {chromium} from '/home/joe/code/tessera/clients/ts/node_modules/playwright/index.mjs';
import fs from 'node:fs';
const boards = JSON.parse(fs.readFileSync('canvas.json','utf8')).artboards;
const browser = await chromium.launch();
for (const b of boards) {
  let html = fs.readFileSync(b.file,'utf8');
  html = html.replace('<script src="./support.js"></script>','')
             .replace(/\{\{themeClass\}\}/g, b.file.includes('Overlay') ? 'tx dark' : 'tx')
             .replace(/<script data-dc-script[\s\S]*?<\/script>/,'').replace(/<link rel="stylesheet"[^>]*>/g,'');
  const page = await browser.newPage({viewport:{width:b.w,height:b.h}, deviceScaleFactor:1});
  await page.setContent(html, {waitUntil:'domcontentloaded'});
  await page.waitForTimeout(600);
  await page.screenshot({path:`shots/${b.file.replace('.dc.html','')}.png`});
  await page.close();
  console.log('shot', b.file);
}
await browser.close();
