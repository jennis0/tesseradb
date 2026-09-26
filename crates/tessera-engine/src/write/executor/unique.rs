//! Declaring `unique` on a column that exists, and removing it.
//!
//! A declaration builds the column's index over the values already stored before it is in force.
//! The build runs in rounds off this thread: the first reads every flushed value; each later one
//! reads the entities the flushes published since the round before it started. Every round also
//! checks the buffered values against every run written so far. Once a round finishes with no
//! flush published during it, the values buffered since it started are checked here, and the
//! declaration commits: a `UniqueDeclare` record, a side-manifest naming the runs, and a
//! generation whose live entries are the buffer's. A value held twice anywhere refuses the
//! declaration and removes its runs.
//!
//! No fold starts while a declaration is building, and a declaration waits for a running fold:
//! a fold rewrites every run and every layer a round reads. Denies are never held behind one.

use super::*;

use std::collections::VecDeque;

use tessera_store::manifest::{BaseKeyRun, FileDigest, UniqueIndexRuns};
use tessera_store::unique::{UniqueKey, WrittenUniqueRun};

/// The buffered values arriving during a round that the commit checks against the runs itself;
/// more than this and another round checks them off this thread.
const COMMIT_CHECK_MAX: usize = 4096;

/// The memory a round's sort holds before it spills.
const ROUND_MEMORY: usize = 256 << 20;

/// A round's result, handed back to this thread.
pub(crate) struct CompletedRound {
    attempt: u64,
    outcome: Result<crate::unique::RoundOutput, String>,
}

/// The declarations made and not yet answered.
pub(in crate::write) struct UniqueDeclarations {
    pub(super) rounds: Background<CompletedRound>,
    waiting: VecDeque<(String, Reply<bool>)>,
    current: Option<Pending>,
}

/// The declaration being built.
struct Pending {
    attribute: String,
    at: usize,
    reply: Reply<bool>,
    /// The first round's runs, which have disjoint key ranges.
    base: Vec<(WrittenUniqueRun, FileDigest)>,
    /// Every later round's.
    live: Vec<(WrittenUniqueRun, FileDigest)>,
    /// The entities flushes have published since the running round started.
    flushed_since: croaring::Bitmap,
    /// The buffered values the last round checked.
    checked: FxHashMap<EntityId, UniqueKey>,
    attempt: u64,
    rounds: u64,
}

impl UniqueDeclarations {
    pub(in crate::write) fn new(bell: std::sync::mpsc::SyncSender<()>) -> Self {
        UniqueDeclarations {
            rounds: Background::new(bell),
            waiting: VecDeque::new(),
            current: None,
        }
    }

    /// Whether a declaration is building or waiting to.
    pub(super) fn busy(&self) -> bool {
        self.current.is_some() || !self.waiting.is_empty()
    }

    /// Note the entities a published flush consumed or filled: the next round reads them.
    pub(super) fn flushed(&mut self, consumed: &[EntityId], filled: &[EntityId]) {
        if let Some(pending) = &mut self.current {
            for entity in consumed.iter().chain(filled) {
                if let Ok(entity) = u32::try_from(entity.raw()) {
                    pending.flushed_since.add(entity);
                }
            }
        }
    }
}

impl Pending {
    fn runs(&self) -> impl Iterator<Item = &(WrittenUniqueRun, FileDigest)> {
        self.base.iter().chain(&self.live)
    }

    /// Remove every run this declaration wrote; nothing names them.
    fn discard(&self) {
        for (run, _) in self.runs() {
            let _ = std::fs::remove_file(&run.path);
        }
    }
}

impl Executor {
    /// `PUT /control/attributes` on a column that exists, differing in `unique` alone. A removal
    /// commits at once; a declaration is queued and answered when its index is built.
    pub(super) fn begin_unique_declare(&mut self, name: String, unique: bool, reply: Reply<bool>) {
        if !unique {
            self.commit_unique_removal(name, reply);
            return;
        }
        self.unique_declarations.waiting.push_back((name, reply));
        self.advance_unique_declarations();
    }

    /// Start the next waiting declaration, unless one is building or a fold is outstanding.
    pub(super) fn advance_unique_declarations(&mut self) {
        if self.unique_declarations.current.is_some() || self.fold.outstanding() {
            return;
        }
        let Some((attribute, reply)) = self.unique_declarations.waiting.pop_front() else {
            return;
        };
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        let Some(at) = manifest.declared_scalars.iter().position(|d| d.name == attribute) else {
            reply.fail(ExecError::AttributeRefused {
                detail: format!("attribute '{attribute}' is not declared"),
            });
            return self.advance_unique_declarations();
        };
        if manifest.declared_scalars[at].unique {
            reply.ack(true);
            return self.advance_unique_declarations();
        }
        // Every entity a flush or a build can have given a value: the point region.
        let high_water = generation
            .bundle
            .partitions
            .values()
            .map(|p| p.manifest.entity_id_high_water)
            .fold(manifest.entity_id_high_water, u64::max);
        let mut wanted = croaring::Bitmap::new();
        wanted.add_range(0..high_water.min(u64::from(u32::MAX)) as u32);
        let attempt = self.unique_declarations.rounds.next_attempt();
        self.unique_declarations.current = Some(Pending {
            attribute,
            at,
            reply,
            base: Vec::new(),
            live: Vec::new(),
            flushed_since: croaring::Bitmap::new(),
            checked: FxHashMap::default(),
            attempt,
            rounds: 0,
        });
        self.start_unique_round(generation, wanted);
    }

    /// Hand one round to its own thread: it reads the corpus, and the pool serves requests.
    fn start_unique_round(&mut self, generation: Arc<Generation>, wanted: croaring::Bitmap) {
        let prefix_dir = self.prefix_dir(&generation);
        let Some(pending) = &mut self.unique_declarations.current else {
            return;
        };
        pending.rounds += 1;
        pending.flushed_since = croaring::Bitmap::new();
        let Some(partition) = generation.bundle.partitions.keys().next().cloned() else {
            return;
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let input = crate::unique::RoundInput {
            at: pending.at,
            wanted,
            prior: pending.runs().map(|(run, _)| run.path.clone()).collect(),
            out_dir: prefix_dir.join(tessera_store::unique::index_dir_rel(
                &partition,
                &pending.attribute,
            )),
            prefix_dir,
            stem: format!("declared-{stamp}-{}", pending.rounds),
            memory_budget: ROUND_MEMORY,
            generation,
        };
        let attempt = pending.attempt;
        let unit = self.unique_declarations.rounds.start();
        #[cfg(feature = "fault-injection")]
        let switches = Arc::clone(&self.deps.switches);
        let spawned = std::thread::Builder::new()
            .name("tessera-unique".to_string())
            .spawn(move || {
                let outcome = crate::unique::build_round(input);
                #[cfg(feature = "fault-injection")]
                switches.hold_unique_round_if_paused();
                unit.complete(CompletedRound { attempt, outcome });
            });
        if let Err(e) = spawned {
            let pending = self.unique_declarations.current.take().expect("held above");
            pending.discard();
            pending.reply.fail(ExecError::AttributeRefused {
                detail: format!(
                    "attribute '{}': the index could not be built ({e}); declare it again",
                    pending.attribute
                ),
            });
        }
    }

    /// Take every finished round: refuse, start another, or commit. Also starts a waiting
    /// declaration a fold held back.
    pub(super) fn publish_completed_unique_rounds(&mut self) -> bool {
        let mut any = false;
        while let Some(done) = self.unique_declarations.rounds.next_completed() {
            any = true;
            self.take_round(done);
        }
        if any {
            self.unique_declarations.rounds.drained();
        }
        self.advance_unique_declarations();
        any
    }

    fn take_round(&mut self, done: CompletedRound) {
        let Some(pending) = &mut self.unique_declarations.current else {
            return;
        };
        if pending.attempt != done.attempt {
            return;
        }
        let output = match done.outcome {
            Ok(output) => output,
            Err(detail) => {
                let pending = self.unique_declarations.current.take().expect("held above");
                pending.discard();
                tracing::error!(attribute = %pending.attribute, %detail, "a unique index build failed");
                pending.reply.fail(ExecError::AttributeRefused {
                    detail: format!(
                        "attribute '{}': the index could not be built ({detail}); declare it \
                         again",
                        pending.attribute
                    ),
                });
                return;
            }
        };
        let first = pending.rounds == 1;
        if first {
            pending.base.extend(output.runs);
        } else {
            pending.live.extend(output.runs);
        }
        pending.checked = output.checked;
        if output.duplicates > 0 {
            self.refuse_unique_declare(output.duplicates, &output.examples);
            return;
        }
        let generation = self.generation.load_full();
        if !pending.flushed_since.is_empty() {
            let wanted = std::mem::take(&mut pending.flushed_since);
            self.start_unique_round(generation, wanted);
            return;
        }
        // The values buffered since the round started, checked here where they are few.
        let at = pending.at;
        let ty = generation.bundle.manifest.declared_scalars[at].arrow_type;
        let arrived: Vec<(EntityId, UniqueKey, WalScalar)> =
            crate::unique::buffered_scalars(&generation.buffer)
                .into_iter()
                .filter(|(entity, _)| !generation.overlay.is_deleted(*entity))
                .filter_map(|(entity, scalars)| {
                    let value = scalars.get(at)?;
                    let key = tessera_store::unique::key_of(ty, value)?;
                    (pending.checked.get(&entity) != Some(&key))
                        .then(|| (entity, key, value.clone()))
                })
                .collect();
        if arrived.len() > COMMIT_CHECK_MAX {
            self.start_unique_round(generation, croaring::Bitmap::new());
            return;
        }
        match self.check_arrived(&generation, &arrived) {
            Ok(None) => self.commit_unique_declare(),
            Ok(Some((count, examples))) => self.refuse_unique_declare(count, &examples),
            Err(detail) => {
                let pending = self.unique_declarations.current.take().expect("held above");
                pending.discard();
                pending.reply.fail(ExecError::AttributeRefused {
                    detail: format!(
                        "attribute '{}': the index could not be read ({detail}); declare it \
                         again",
                        pending.attribute
                    ),
                });
            }
        }
    }

    /// The values buffered since the last round started, against the other buffered values and
    /// against every run the declaration wrote: the duplicates found, or `None`.
    fn check_arrived(
        &self,
        generation: &Generation,
        arrived: &[(EntityId, UniqueKey, WalScalar)],
    ) -> Result<Option<(u64, Vec<String>)>, String> {
        let pending = self
            .unique_declarations
            .current
            .as_ref()
            .expect("a declaration is building");
        if arrived.is_empty() {
            return Ok(None);
        }
        let mut examples: Vec<String> = Vec::new();
        let mut keys_found: Vec<UniqueKey> = Vec::new();
        let mut note = |key: UniqueKey, value: &WalScalar| {
            if !keys_found.contains(&key) {
                keys_found.push(key);
                if examples.len() < tessera_store::unique::DUPLICATE_EXAMPLES {
                    examples.push(tessera_store::unique::value_text(value));
                }
            }
        };
        let mut held: FxHashMap<UniqueKey, EntityId> = pending
            .checked
            .iter()
            .filter(|(entity, _)| !generation.overlay.is_deleted(**entity))
            .map(|(entity, key)| (*key, *entity))
            .collect();
        for (entity, key, value) in arrived {
            if held.insert(*key, *entity).is_some_and(|other| other != *entity) {
                note(*key, value);
            }
        }
        let kind = tessera_store::unique::KeyKind::of(
            generation.bundle.manifest.declared_scalars[pending.at].arrow_type,
        )
        .ok_or_else(|| "the column cannot be unique".to_string())?;
        let prefix_dir = self.prefix_dir(generation);
        let runs = UniqueIndexRuns {
            attribute: pending.attribute.clone(),
            base: Vec::new(),
            live: pending
                .runs()
                .map(|(run, _)| tessera_store::unique::relative(&prefix_dir, &run.path))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?,
        };
        let index = tessera_store::unique::UniqueIndex::open(&runs, kind, &prefix_dir, None)
            .map_err(|e| e.to_string())?;
        let keys: Vec<UniqueKey> = arrived.iter().map(|(_, key, _)| *key).collect();
        for (i, holder) in index.lookup(&keys).map_err(|e| e.to_string())? {
            let holder = EntityId::new(u64::from(holder));
            if holder != arrived[i].0 && !generation.overlay.is_deleted(holder) {
                note(arrived[i].1, &arrived[i].2);
            }
        }
        let count = keys_found.len() as u64;
        Ok((count > 0).then_some((count, examples)))
    }

    fn refuse_unique_declare(&mut self, count: u64, examples: &[String]) {
        let pending = self
            .unique_declarations
            .current
            .take()
            .expect("a declaration is building");
        pending.discard();
        pending.reply.fail(ExecError::UniqueTaken {
            detail: tessera_store::unique::duplicates_message(&pending.attribute, count, examples),
        });
    }

    /// Make the built index durable and in force: the log record, a side-manifest naming the
    /// runs, and a generation serving them with the buffer's live entries.
    fn commit_unique_declare(&mut self) {
        let pending = self
            .unique_declarations
            .current
            .take()
            .expect("a declaration is building");
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let prefix_dir = self.prefix_dir(&generation);
        let declared = |runs: &[(WrittenUniqueRun, FileDigest)]| {
            runs.iter()
                .map(|(run, digest)| {
                    let base = run.as_base(&prefix_dir)?;
                    Ok(tessera_lifecycle::wal::DeclaredRun {
                        path: base.path,
                        sha256: digest.sha256.clone(),
                        size: digest.size,
                        first_key: base.first_key,
                        last_key: base.last_key,
                    })
                })
                .collect::<Result<Vec<_>, tessera_store::StoreError>>()
        };
        let (base, live) = match (declared(&pending.base), declared(&pending.live)) {
            (Ok(base), Ok(live)) => (base, live),
            (Err(e), _) | (_, Err(e)) => {
                pending.discard();
                pending.reply.fail(ExecError::AttributeRefused {
                    detail: format!("attribute '{}': {e}", pending.attribute),
                });
                return;
            }
        };
        let record = WalRecord::UniqueDeclare {
            attribute: pending.attribute.clone(),
            unique: true,
            base: base.clone(),
            live: live.clone(),
        };
        if let Err(e) = self.make_durable(&[&record], "a unique declaration") {
            pending.discard();
            pending.reply.fail(e);
            return;
        }
        let index = UniqueIndexRuns {
            attribute: pending.attribute.clone(),
            base: base
                .iter()
                .map(|run| BaseKeyRun {
                    path: run.path.clone(),
                    first_key: run.first_key.clone(),
                    last_key: run.last_key.clone(),
                })
                .collect(),
            live: live.iter().map(|run| run.path.clone()).collect(),
        };
        let files: Vec<(String, FileDigest)> = base
            .iter()
            .chain(&live)
            .zip(pending.runs())
            .map(|(run, (_, digest))| (run.path.clone(), digest.clone()))
            .collect();
        self.publish_unique_change(&generation, &pending.attribute, Some((index, files)), started);
        pending.reply.ack(true);
    }

    /// Stop a column being unique: the log record, then a side-manifest without its index.
    fn commit_unique_removal(&mut self, attribute: String, reply: Reply<bool>) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let record = WalRecord::UniqueDeclare {
            attribute: attribute.clone(),
            unique: false,
            base: Vec::new(),
            live: Vec::new(),
        };
        if let Err(e) = self.make_durable(&[&record], "a unique removal") {
            reply.fail(e);
            return;
        }
        self.publish_unique_change(&generation, &attribute, None, started);
        reply.ack(true);
    }

    /// Publish the empty index of a unique column declared new at a running service.
    pub(super) fn publish_new_unique_index(&mut self, attribute: &str, started: std::time::Instant) {
        let generation = self.generation.load_full();
        let runs = UniqueIndexRuns {
            attribute: attribute.to_string(),
            base: Vec::new(),
            live: Vec::new(),
        };
        self.publish_unique_change(&generation, attribute, Some((runs, Vec::new())), started);
    }

    /// Publish a column's index added (`Some`) or removed (`None`): a side-manifest, then the
    /// generation. The change is already durable in the log, so a side-manifest that cannot be
    /// written leaves the change in force and owed to the next publication.
    fn publish_unique_change(
        &mut self,
        generation: &Arc<Generation>,
        attribute: &str,
        index: Option<(UniqueIndexRuns, Vec<(String, FileDigest)>)>,
        started: std::time::Instant,
    ) {
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return;
        };
        let mut manifest = partition_data.manifest.clone();
        let unique = index.is_some();
        match index {
            Some((runs, files)) => {
                manifest.unique_indexes.retain(|held| held.attribute != attribute);
                manifest.unique_indexes.push(runs);
                manifest.files.extend(files);
            }
            None => {
                if let Some(at) = manifest
                    .unique_indexes
                    .iter()
                    .position(|held| held.attribute == attribute)
                {
                    let removed = manifest.unique_indexes.remove(at);
                    for file in removed.files() {
                        manifest.files.remove(file);
                    }
                }
            }
        }
        write_deny_state(&mut manifest, &generation.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &generation.vocabularies,
            &generation.bundle.manifest.vocabularies,
        );
        let n = match self
            .side_manifests
            .allocate_manifest_n(&self.deps.bundle_root, &self.health)
        {
            Ok(n) => Some(n),
            Err(e) => {
                tracing::error!(error = %e, "a unique declaration's side-manifest number could not be allocated");
                None
            }
        };
        let committed = n.is_some_and(|n| {
            match self.commit_side_manifest(
                partition_data,
                &self.prefix_dir(generation),
                partition,
                n,
                &mut manifest,
                None,
            ) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(error = %e, "a unique declaration's side-manifest could not be committed");
                    false
                }
            }
        });
        if !committed {
            // Durable in the log; the next publication writes it.
            self.side_manifests.behind_live = true;
        }
        let served_n = match (committed, n) {
            (true, Some(n)) => n,
            _ => partition_data.segments_n,
        };
        let bundle = match generation.bundle.with_manifest(
            partition,
            tessera_store::read::PublishedManifest {
                manifest,
                n: served_n,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                tracing::error!(error = %e, "ALARM: a unique declaration could not be applied to the served bundle; a restart applies it");
                return;
            }
        };
        let served = tessera_store::unique::with_unique_flags(
            &bundle.manifest,
            &bundle.partitions[partition].manifest,
        );
        let bundle = bundle.with_views(served);
        let unique_indexes = match tessera_store::unique::UniqueIndexes::open(
            &bundle.manifest,
            &bundle.partitions[partition].manifest,
            &self.prefix_dir(generation),
            Some(&generation.unique),
        ) {
            Ok(indexes) => Arc::new(indexes),
            Err(e) => {
                tracing::error!(error = %e, "ALARM: a unique index was published and could not be opened; a restart opens it");
                return;
            }
        };
        let mut unique_live = (*generation.unique_live).clone();
        unique_live.set_unique(&bundle.manifest, &generation.buffer, attribute, unique);
        let filter_columns = match bundle
            .manifest
            .declared_scalars
            .iter()
            .find(|d| d.name == attribute)
        {
            Some(d) => Arc::new(
                generation
                    .filter_columns
                    .with_unique(d, &bundle.manifest.vocabularies),
            ),
            None => Arc::clone(&generation.filter_columns),
        };
        let next = generation.with(|g| {
            g.bundle = bundle;
            g.unique = unique_indexes;
            g.unique_live = Arc::new(unique_live);
            g.filter_columns = filter_columns;
        });
        self.publish(next, started);
    }
}
