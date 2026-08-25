# Client-component mock-ups — the generator

The design canvas for [`client-components.md`](../../../design/client-components.md) ("Tessera
Client Components", published 2026-08-24) was built from these files, and can be rebuilt from
them; the rendered `.dc.html` boards are output and are not committed.

- `gen.py` — the shared pieces: the data-map renderer (positional palette, density wash, traced
  contours, sized labels), the icons, and the default explorer's tokens and component markup.
- `build.py` — the ten boards and `canvas.json`. `python3 build.py` writes them beside itself.
- `shoot.mjs` — renders each board headlessly through the client workspace's Playwright, for a
  look before saving; it strips the font links, so it shows fallback faces.

The boards are illustrative: the arXiv cluster names and counts are shaped like the demo's, and
every record, incident and image on them is invented.
