# Archive

Frozen material. Nothing here is current, and nothing here should be executed, cited as
authority, or edited. It is kept because the reasoning is often still worth reading and
because the record of what was decided, and when, has value that the outcome alone does not.

**The current sources of truth are [`../design/`](../design/) for architecture, `../decisions/`
for settled decisions, `../evidence/` for measurements and prior art, and GitHub issues for
work in progress.**

## What is here

### `whitepaper/` and `reference/` — the public HTML pages

Sources for `tessera-white-paper.html` and `tessera-processes.html`, frozen 2026-07-30. The
built artifacts have been deleted. Both are stale on two independent counts:

- **Never finished.** `whitepaper/src/90-footer.html` still contains an empty
  `<section id="citations">`. The publish task was briefed and never ran.
- **Factually superseded.** Part III and the Part VI leak register describe the wire identity
  as "per-session opaque handles". Those were retired on 2026-07-29 in favour of the keyed FPE
  `tessera_id`, a string that appears nowhere in the paper. The performance figures predate the
  rebuild, and the fact sheets cite the architecture design at r18/r19 and contracts at r3/r4
  against current r24 and r11.

The toolchain (`build.py`, `validate.py`, `render_check.py`) still works and the design system
in `whitepaper/src/00-style.html` is worth reusing. **Regenerate from `../design/` rather than
repairing these files** — too much has changed for an edit pass to be trustworthy.

### `plans/` — every implementation plan and spent spec

Plans are no longer a maintained artifact. Each conflated three things with different
lifespans, and they are now kept apart: design rationale in `../design/`, settled decisions in
`../decisions/`, work status in GitHub issues, and per-task instructions in the disposable SDD
workspace. Each file here carries a banner recording whether it executed.

Their checkboxes were never the record — completion was tracked in ledgers that were not
committed. Do not read an unticked box as unfinished work.

`plans/bench-baselines/` holds two JSON baselines. `2m4-criterion-baseline.json` is still the
relative no-regression gate; `2026-07-29-1e9-k-sweep.json` no longer measures this engine.

### `visualisation.md` — the client architecture

Superseded by [`../design/client-interaction.md`](../design/client-interaction.md). It was
written before the deck.gl client existed and describes two deployment profiles over one data
contract; the client that was actually built, and the protocol it speaks, are specified in the
client-interaction document and its children. Its §8 record of rejected alternatives is the
part still worth reading.
