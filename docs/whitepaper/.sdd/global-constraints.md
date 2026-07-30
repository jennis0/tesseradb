## Global Constraints

Every task's requirements implicitly include this section.

- **Source of truth for claims:** `.ignore/*.md` for design and prior art, `probes/results.md` and `probes/phase0-memo.md` for measurements. `.ignore/` is skipped by default file-search tooling — pass the path explicitly.
- **Precedence:** the architecture design is the specification; where the contracts spec and the system architecture differ, contracts spec §0.3 governs.
- **No claim from memory.** Every competitor claim and every performance number must trace to a fact sheet entry from Task 1, which cites document and section.
- **Preserve demonstrated vs marketed scale.** deepscatter's billion-point artefact is a *static* star catalogue; Nomic's marketing says "billions" while its largest published map is 11M. Collapsing these distinctions is a factual error.
- **Synthetic-policy caveat stated plainly**, not buried: Phase 0 ran synthetic policies over a real 2.42M-paper arXiv corpus, scaled to 10⁹.
- **Invariants must match `.ignore/tessera-architecture-design.md` §4 in substance.** I2, I3, I7, I9, I10, I12 and the three retirement rules (lifecycle §3) all appear in the paper.
- **British spelling** throughout (authorisation, visualisation, colour).
- **No external resources.** No CDN scripts, external stylesheets, remote fonts, remote images, `fetch`/XHR/WebSocket. Assets inlined or embedded as `data:` URIs. Prose may contain `<a href="https://...">` citation links — those are navigation, not resource loads, and are permitted.
- **No document-level tags** in source fragments: the Artifact tool supplies `<!doctype>`, `<html>`, `<head>` and `<body>`. Fragments contain page content only, with `<style>` and `<script>` inline in that content.
- **Theme-aware:** correct under `@media (prefers-color-scheme: dark)` *and* under explicit `:root[data-theme="dark"]` / `:root[data-theme="light"]` overrides, with the explicit override winning in both directions.
- **Target length:** 8,000–10,000 words of prose plus sixteen figures.
- **The repository is not under git.** There is nothing to commit to. Wherever this plan says "checkpoint", it means: run the build, run the validator, run the render harness, and confirm all three pass before moving on. Do not run `git init`.
