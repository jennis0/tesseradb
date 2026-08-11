# Decisions

One file per settled decision. Each is immutable once written: if a decision is later reversed,
the new decision gets its own file and says what it supersedes. Nothing here is edited to reflect
a change of mind — that would destroy the record it exists to keep.

A decision belongs here when it was **chosen rather than derived**: where a competent engineer
could reasonably have gone the other way, and where the next person to touch the area will
otherwise re-litigate it from scratch. Derivable facts belong in [`../design/`](../design/);
measurements belong in [`../evidence/`](../evidence/).

**Format.** Context, the decision, why, the evidence, and what it supersedes. Short. If a memo
already *is* the record, the entry is a pointer rather than a copy.

Many of these were recovered from execution ledgers that were never committed — they existed on
one machine, in files git had been told to ignore. That is the failure this directory exists to
prevent.

| # | Date | Decision |
|---|---|---|
| [0001](0001-rust-build-pipeline.md) | 2026-07-28 | The build pipeline is Rust, not Python — one binary, two modes |
| [0002](0002-no-real-label-rerun.md) | 2026-07-28 | No real-label rerun; synthetic-corpus evidence is accepted as final |
| [0003](0003-external-id-as-boundary-identity.md) | 2026-07-29 | External ID becomes the boundary identity; `node_id` is dropped |
| [0004](0004-external-id-sidecar-transitional.md) | 2026-07-29 | The external-ID sidecar is transitional, with three inherited conditions |
| [0005](0005-tessera-id-keyed-bijection.md) | 2026-07-29 | `tessera_id` is a keyed bijection, not a 128-bit random |
| [0006](0006-per-session-handles-retired.md) | 2026-07-29 | Per-session `u32` handles are retired from the viewer plane |
| [0007](0007-k-max-marks-500.md) | 2026-07-30 | *K*<sub>max</sub> raised from 128 to 500 |
| [0008](0008-candidate-list-route-declined.md) | 2026-07-31 | The candidate-list selection route is declined |
| [0009](0009-relative-perf-gate.md) | 2026-07-31 | The per-track performance gate is a relative no-regression baseline |
| [0010](0010-allowlist-is-contention-control.md) | 2026-08-01 | The track allowlist is contention control, not a design constraint |
| [0011](0011-health-probes-off-control-plane.md) | 2026-08-01 | `/healthz` and `/readyz` leave the control plane |
| [0012](0012-panic-over-plausible-503.md) | 2026-08-01 | A loud panic behind an unconstructable type, over a plausible 503 |
| [0013](0013-mark-specified-vs-implemented.md) | 2026-08-01 | Specified-but-unbuilt machinery is marked per claim |
| [0014](0014-i10-weakened-to-construction.md) | 2026-08-01 | I10 is weakened to what the construction defends |
| [0015](0015-plans-retired-for-epics.md) | 2026-08-01 | Plans are retired; work is tracked as capability epics in GitHub issues |
| [0016](0016-segments-filename-unpadded.md) | 2026-08-01 | The `SEGMENTS-<n>.json` filename grammar is unpadded, and a reader must refuse other forms |
| [0017](0017-c4-covers-published-timing.md) | 2026-08-01 | C4 covers published timing as well as inferable timing |
| [0018](0018-manifest-disposition-split-is-contract.md) | 2026-08-01 | The side-manifest disposition split is interchange contract |
| [0019](0019-i13-lettered-properties.md) | 2026-08-01 | I13 names three lettered properties, and I13a is an addition |
| [0020](0020-no-auth-data-retained-beside-a-mask.md) | 2026-08-01 | No authorisation data is retained beside a mask |
| [0021](0021-rust-not-jvm.md) | 2026-07-28 | Rust rather than the JVM, and what that costs |
| [0022](0022-deepscatter-rejected.md) | 2026-07-28 | deepscatter is rejected, on licence and on architecture |
| [0023](0023-derivable-quantities-are-not-disclosures.md) | 2026-08-01 | A quantity derivable from published data is not a disclosure |
| [0024](0024-leak-register-scope-is-viewer-inference.md) | 2026-08-01 | The leak register covers what a viewer can infer, not data at rest |
| [0025](0025-rotation-is-a-session-invalidation-event.md) | 2026-08-01 | A key rotation invalidates sessions; identifiers are not stable across them |
| [0026](0026-idset-stamp-version.md) | 2026-08-01 | Three concepts that shared the word "epoch" get three words |
| [0027](0027-i5-is-unverified.md) | 2026-08-01 | I5 is unverified, and the specification says so |
| [0028](0028-postings-requirement-and-the-pair-relation.md) | 2026-08-01 | What the postings build requires, and who needs the pair relation |
| [0029](0029-view-key.md) | 2026-08-01 | A fourth concept called "epoch": the view key |
| [0030](0030-determinism-is-not-a-guarantee.md) | 2026-08-01 | Response determinism is an implementation detail, not a guarantee |
| [0031](0031-decode-tiers-are-specified-not-promised.md) | 2026-08-01 | The decode tiers are described where selection is specified |
| [0032](0032-delete-the-dead-handle-table.md) | 2026-08-01 | The per-session handle allocation goes; the type stays |
| [0033](0033-both-lanes-group-commit.md) | 2026-08-01 | Both write lanes group-commit, in separate windows |
| [0034](0034-the-window-does-not-linger.md) | 2026-08-01 | A commit window closes when its queue drains; it does not linger |
| [0035](0035-session-sweep-runs-on-growth-not-on-a-timer.md) | 2026-08-01 | The session registry sheds on growth, not on a timer |
| [0036](0036-per-connection-body-ceiling-not-a-connection-cap.md) | 2026-08-01 | The buffered-body window gets a per-connection ceiling, not a connection cap |
| [0037](0037-canary-canonicalisation-joins-on-tessera-id.md) | 2026-08-01 | The canary comparison canonicalises on `tessera_id`, not on `fx_key` |
| [0038](0038-fsync-offset-is-a-sidecar-not-a-command.md) | 2026-08-01 | Crash realism reads the WAL's sync sidecar; `fsync_offset()` is not built |
| [0039](0039-multi-valued-categoricals-are-slow-path-only.md) | 2026-08-01 | Multi-valued categoricals are a slow-path capability only |
| [0040](0040-quantisation-is-slice-scoped-index-config.md) | 2026-08-02 | The quantisation extent is slice-scoped index configuration, immutable at runtime |
| [0041](0041-pins-become-a-staleness-stamp.md) | 2026-08-03 | A pin becomes a staleness stamp, not retained geometry |
| [0042](0042-a-dictionary-extent-never-repeats-a-descriptor.md) | 2026-08-03 | A dictionary extent never repeats a descriptor, and the loader enforces it |
| [0043](0043-geometry-maintenance-never-blocks-a-request.md) | 2026-08-03 | Flush, merge and compaction never block a request; their effects are invisible to the viewer |
| [0044](0044-invisible-means-stale-serve-plus-background-refresh.md) | 2026-08-04 | "Invisible" means stale-serve plus background refresh; merge splits, and publishes as its own swap |
| [0045](0045-inert-config-keys-are-deleted.md) | 2026-08-04 | Inert configuration keys are deleted, not kept parsed |
| [0046](0046-the-priority-column-is-cut.md) | 2026-08-04 | The `priority` column is cut from `columns.arrow` |
| [0047](0047-edit-is-delete-plus-reingest.md) | 2026-08-04 | Edit is delete + re-ingest, and a deleted entity is forgotten at the boundary |
| [0048](0048-no-deployments-exist-so-delete-rather-than-support.md) | 2026-08-05 | No deployments exist, so delete rather than support |
| [0049](0049-the-merge-ladder-saturates-and-the-cap-stays.md) | 2026-08-05 | The merge ladder saturates at the cap, and the cap stays until the merge streams |
| [0050](0050-a-fold-invalidates-the-term-index-and-every-fragment.md) | 2026-08-06 | A fold invalidates the term index and every mask fragment |
| [0051](0051-a-compaction-emits-one-base-segment-plus-its-flight.md) | 2026-08-06 | A compaction emits one base segment, plus whatever its flight published |
| [0052](0052-the-folds-page-cache-mitigation-is-a-hint-not-a-throttle.md) | 2026-08-06 | The fold's page-cache mitigation is `MADV_SEQUENTIAL`, not a read throttle |
| [0053](0053-a-folds-aftermath-is-a-cache-miss-not-a-refusal.md) | 2026-08-06 | A fold's aftermath is a cache miss, not a refusal |
| [0054](0054-a-bundle-artefacts-writer-lives-in-tessera-store.md) | 2026-08-06 | A bundle artefact's writer lives in `tessera-store` |
| [0055](0055-the-folds-fragment-sweep-is-everything-present-at-the-flip.md) | 2026-08-06 | The fold's fragment sweep is everything present at the flip |
| [0056](0056-a-folds-schedule-is-a-gated-window-not-a-pure-timer.md) | 2026-08-07 | A fold's schedule is a gated window, not a pure timer |
| [0057](0057-rows-frozen-is-declined-and-the-staging-list-is-deferred.md) | 2026-08-07 | Rows-frozen is declined, and the flip's staging list is deferred |
| [0058](0058-a-single-flight-racer-waits-rather-than-being-refused.md) | 2026-08-09 | A single-flight racer waits for the build rather than being refused |
| [0059](0059-per-principal-admission-is-not-capped.md) | 2026-08-09 | Per-principal admission is not capped; the wait budget bounds it instead |
| [0060](0060-a-stream-lives-at-most-the-whole-stream-deadline.md) | 2026-08-11 | A streamed response lives at most the whole-stream deadline |
| [0061](0061-i13a-forbids-undetectable-partials-not-streaming.md) | 2026-08-11 | I13a forbids undetectable or incorrect partials, not streamed truncation |
