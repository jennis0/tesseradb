//! Declaring `unique` on a column that exists, and removing it.
//!
//! A declaration builds the column's index over the values already stored before it is in force.
//! The build runs in rounds off this thread, one shard at a time: each shard's first round reads
//! every value it has flushed; each later one reads the entities its flushes published since its
//! round before started. A round checks its values against every run written so far in any shard,
//! and every shard's buffered values against those runs and each other. Once no round is left and
//! no flush has published since a shard's last round started, the values buffered since the last
//! round started are checked here, and the declaration commits: a `UniqueDeclare` record naming
//! each run's shard, a side-manifest naming each shard's runs, and a generation whose live entries
//! are the buffers'. A value held twice anywhere refuses the declaration and removes its runs.
//!
//! No fold starts while a declaration is building, and a declaration waits for a running fold:
//! a fold rewrites every run and every layer a round reads. Denies are never held behind one.

use super::*;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use mosaica_store::manifest::{FileDigest, UniqueIndexRuns};
use mosaica_store::unique::{UniqueKey, WrittenUniqueRun};
use mosaica_types::ItemAt;

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

/// One run a declaration wrote.
struct Run {
    shard: ShardId,
    /// Written by its shard's first round, whose runs have disjoint key ranges.
    base: bool,
    written: WrittenUniqueRun,
    digest: FileDigest,
}

/// The declaration being built.
struct Pending {
    attribute: String,
    at: usize,
    reply: Reply<bool>,
    runs: Vec<Run>,
    /// The rounds still to run: each shard, and the flushed entities of it to read, or `None` for
    /// every entity it holds when the round starts.
    queue: VecDeque<(ShardId, Option<croaring::Bitmap>)>,
    /// The shard the running round reads.
    running: Option<ShardId>,
    /// The shards a round has read.
    read: BTreeSet<ShardId>,
    /// Per shard, the entities its flushes have published since its last round started.
    flushed_since: BTreeMap<ShardId, croaring::Bitmap>,
    /// The buffered values the last round checked.
    checked: FxHashMap<ItemAt, UniqueKey>,
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

    /// Note the entities a published flush of `shard` consumed: a later round reads them.
    pub(super) fn flushed(&mut self, shard: ShardId, consumed: &[EntityId]) {
        if let Some(pending) = &mut self.current {
            let since = pending.flushed_since.entry(shard).or_default();
            for entity in consumed {
                if let Ok(entity) = u32::try_from(entity.raw()) {
                    since.add(entity);
                }
            }
        }
    }
}

impl Pending {
    /// Remove every run this declaration wrote; nothing names them.
    fn discard(&self) {
        for run in &self.runs {
            let _ = std::fs::remove_file(&run.written.path);
        }
    }

    /// Every run written so far, each with its shard, relative to `prefix_dir`.
    fn relative_runs(
        &self,
        prefix_dir: &std::path::Path,
    ) -> Result<Vec<(ShardId, String)>, mosaica_store::StoreError> {
        self.runs
            .iter()
            .map(|run| {
                Ok((
                    run.shard,
                    mosaica_store::unique::relative(prefix_dir, &run.written.path)?,
                ))
            })
            .collect()
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
        let attempt = self.unique_declarations.rounds.next_attempt();
        self.unique_declarations.current = Some(Pending {
            attribute,
            at,
            reply,
            runs: Vec::new(),
            queue: generation
                .shards()
                .map(|shard| (shard.id(), None))
                .collect(),
            running: None,
            read: BTreeSet::new(),
            flushed_since: BTreeMap::new(),
            checked: FxHashMap::default(),
            attempt,
            rounds: 0,
        });
        self.start_unique_round(generation);
    }

    /// Hand the next queued round to its own thread: it reads the corpus, and the pool serves
    /// requests.
    fn start_unique_round(&mut self, generation: Arc<Generation>) {
        let prefix_dir = self.prefix_dir(&generation);
        let Some(pending) = &mut self.unique_declarations.current else {
            return;
        };
        let Some((shard, wanted)) = pending.queue.pop_front() else {
            return;
        };
        pending.rounds += 1;
        pending.running = Some(shard);
        // The round reads every entity of the shard flushed before it starts.
        pending.flushed_since.remove(&shard);
        let Some(partition) = generation.bundle.partitions.keys().next().cloned() else {
            return;
        };
        // Every entity a flush or a build can have given a value: the shard's point region.
        let wanted = wanted.unwrap_or_else(|| {
            let high_water = generation
                .shard(shard)
                .map_or(0, |shard| shard.data.record.high_water);
            let mut every = croaring::Bitmap::new();
            every.add_range(0..high_water.min(u64::from(u32::MAX)) as u32);
            every
        });
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let input = crate::unique::RoundInput {
            shard,
            at: pending.at,
            wanted,
            prior: pending
                .runs
                .iter()
                .map(|run| (run.shard, run.written.path.clone()))
                .collect(),
            out_dir: prefix_dir
                .join(ShardLayout::new(&partition, shard).unique_index_rel(&pending.attribute)),
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
            .name("mosaica-unique".to_string())
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
        let shard = pending.running.take().expect("a round was running");
        let base = pending.read.insert(shard);
        pending
            .runs
            .extend(output.runs.into_iter().map(|(written, digest)| Run {
                shard,
                base,
                written,
                digest,
            }));
        pending.checked = output.checked;
        if output.duplicates > 0 {
            self.refuse_unique_declare(output.duplicates, &output.examples);
            return;
        }
        let generation = self.generation.load_full();
        if pending.queue.is_empty() {
            let flushed = std::mem::take(&mut pending.flushed_since);
            pending.queue.extend(
                flushed
                    .into_iter()
                    .filter(|(_, since)| !since.is_empty())
                    .map(|(shard, since)| (shard, Some(since))),
            );
        }
        if !pending.queue.is_empty() {
            self.start_unique_round(generation);
            return;
        }
        // The values buffered since the round started, checked here where they are few.
        let at = pending.at;
        let ty = generation.bundle.manifest.declared_scalars[at].arrow_type;
        let buffers = generation
            .shards()
            .map(|shard| (shard.id(), &*shard.buffer));
        let arrived: Vec<(ItemAt, UniqueKey, WalScalar)> = crate::unique::buffered_items(buffers)
            .into_iter()
            .filter(|(item, _)| {
                !generation
                    .shard_of(item.shard)
                    .overlay
                    .is_deleted(item.entity)
            })
            .filter_map(|(item, scalars)| {
                let value = scalars.get(at)?;
                let key = mosaica_store::unique::key_of(ty, value)?;
                (pending.checked.get(&item) != Some(&key)).then(|| (item, key, value.clone()))
            })
            .collect();
        if arrived.len() > COMMIT_CHECK_MAX {
            // A round reading nothing in a shard checks every buffered value off this thread.
            if let Some(shard) = generation.shards().next() {
                pending
                    .queue
                    .push_back((shard.id(), Some(croaring::Bitmap::new())));
            }
            self.start_unique_round(generation);
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
    /// against every run the declaration wrote in any shard: the duplicates found, or `None`.
    fn check_arrived(
        &self,
        generation: &Generation,
        arrived: &[(ItemAt, UniqueKey, WalScalar)],
    ) -> Result<Option<(u64, Vec<String>)>, String> {
        let deleted = |item: &ItemAt| {
            generation
                .shard(item.shard)
                .is_none_or(|shard| shard.overlay.is_deleted(item.entity))
        };
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
                if examples.len() < mosaica_store::unique::DUPLICATE_EXAMPLES {
                    examples.push(mosaica_store::unique::value_text(value));
                }
            }
        };
        let mut held: FxHashMap<UniqueKey, ItemAt> = pending
            .checked
            .iter()
            .filter(|(item, _)| !deleted(item))
            .map(|(item, key)| (*key, *item))
            .collect();
        for (item, key, value) in arrived {
            if held.insert(*key, *item).is_some_and(|other| other != *item) {
                note(*key, value);
            }
        }
        let kind = mosaica_store::unique::KeyKind::of(
            generation.bundle.manifest.declared_scalars[pending.at].arrow_type,
        )
        .ok_or_else(|| "the column cannot be unique".to_string())?;
        let prefix_dir = self.prefix_dir(generation);
        let runs = pending
            .relative_runs(&prefix_dir)
            .map_err(|e| e.to_string())?;
        let keys: Vec<UniqueKey> = arrived.iter().map(|(_, key, _)| *key).collect();
        let holders = crate::unique::run_holders(
            generation,
            &pending.attribute,
            kind,
            &prefix_dir,
            &runs,
            &keys,
        )?;
        for (i, holder) in holders {
            if holder != arrived[i].0 {
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
            detail: mosaica_store::unique::duplicates_message(&pending.attribute, count, examples),
        });
    }

    /// Make the built index durable and in force: the log record, a side-manifest naming each
    /// shard's runs, and a generation serving them with the buffers' live entries.
    fn commit_unique_declare(&mut self) {
        let pending = self
            .unique_declarations
            .current
            .take()
            .expect("a declaration is building");
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let prefix_dir = self.prefix_dir(&generation);
        let declared: Result<Vec<(bool, mosaica_lifecycle::wal::DeclaredRun)>, _> = pending
            .runs
            .iter()
            .map(|run| {
                let base = run.written.as_base(&prefix_dir)?;
                Ok::<_, mosaica_store::StoreError>((
                    run.base,
                    mosaica_lifecycle::wal::DeclaredRun {
                        shard: run.shard,
                        path: base.path,
                        sha256: run.digest.sha256.clone(),
                        size: run.digest.size,
                        first_key: base.first_key,
                        last_key: base.last_key,
                    },
                ))
            })
            .collect();
        let declared = match declared {
            Ok(declared) => declared,
            Err(e) => {
                pending.discard();
                pending.reply.fail(ExecError::AttributeRefused {
                    detail: format!("attribute '{}': {e}", pending.attribute),
                });
                return;
            }
        };
        let files: Vec<(String, FileDigest)> = pending
            .runs
            .iter()
            .zip(&declared)
            .map(|(run, (_, declared))| (declared.path.clone(), run.digest.clone()))
            .collect();
        let (base, live): (Vec<_>, Vec<_>) = declared.into_iter().partition(|(base, _)| *base);
        let base: Vec<_> = base.into_iter().map(|(_, run)| run).collect();
        let live: Vec<_> = live.into_iter().map(|(_, run)| run).collect();
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
        let index = DeclaredIndex { base, live, files };
        self.publish_unique_change(&generation, &pending.attribute, Some(index), started);
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
        let index = DeclaredIndex {
            base: Vec::new(),
            live: Vec::new(),
            files: Vec::new(),
        };
        self.publish_unique_change(&generation, attribute, Some(index), started);
    }

    /// Publish a column's index added (`Some`) or removed (`None`) in every shard: a
    /// side-manifest, then the generation. The change is already durable in the log, so a
    /// side-manifest that cannot be written leaves the change in force and owed to the next
    /// publication.
    fn publish_unique_change(
        &mut self,
        generation: &Arc<Generation>,
        attribute: &str,
        index: Option<DeclaredIndex>,
        started: std::time::Instant,
    ) {
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return;
        };
        let mut manifest = partition_data.side_manifest();
        let unique = index.is_some();
        for record in &mut manifest.shards.records {
            let runs = index.as_ref().map(|index| {
                crate::unique::declared_runs(attribute, record.id, &index.base, &index.live)
            });
            let removed = mosaica_store::unique::set_unique_index(
                &mut manifest.partition,
                record,
                attribute,
                runs,
            );
            for file in removed.iter().flat_map(UniqueIndexRuns::files) {
                manifest.partition.files.remove(file);
            }
        }
        if let Some(index) = index {
            manifest.partition.files.extend(index.files);
        }
        write_deny_state(
            &mut manifest,
            shard_overlays(generation),
            &generation.layer_spaces,
            |space, deleted| self.live.owed_deletions(space, deleted),
        );
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
        let committed = n.and_then(|n| {
            match self.commit_side_manifest(
                partition_data,
                &self.prefix_dir(generation),
                partition,
                n,
                &mut manifest,
                None,
            ) {
                Ok(refs) => Some((n, refs)),
                Err(e) => {
                    tracing::error!(error = %e, "a unique declaration's side-manifest could not be committed");
                    None
                }
            }
        });
        let (served_n, refs) = match committed {
            Some(committed) => committed,
            None => {
                // Durable in the log; the next publication writes it.
                self.side_manifests.behind_live = true;
                (partition_data.segments_n, partition_data.refs.clone())
            }
        };
        let bundle = match generation.bundle.with_manifest(
            partition,
            mosaica_store::read::PublishedManifest {
                manifest,
                n: served_n,
                refs,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                tracing::error!(error = %e, "ALARM: a unique declaration could not be applied to the served bundle; a restart applies it");
                return;
            }
        };
        let served = mosaica_store::unique::with_unique_flags(
            &bundle.manifest,
            &bundle.partitions[partition].manifest,
        );
        let bundle = bundle.with_views(served);
        let declared = bundle
            .manifest
            .declared_scalars
            .iter()
            .find(|d| d.name == attribute);
        let mut shards = Vec::new();
        for shard in generation.shards() {
            let unique_indexes = match reopen_unique(
                &self.prefix_dir(generation),
                &bundle,
                &bundle.partitions[partition],
                shard,
            ) {
                Ok(indexes) => indexes,
                Err(e) => {
                    tracing::error!(error = %e, "ALARM: a unique index was published and could not be opened; a restart opens it");
                    return;
                }
            };
            let filter_columns = match declared {
                Some(d) => Arc::new(
                    shard
                        .filter_columns
                        .with_unique(d, &bundle.manifest.vocabularies),
                ),
                None => Arc::clone(&shard.filter_columns),
            };
            shards.push((shard.id(), unique_indexes, filter_columns));
        }
        let mut unique_live = (*generation.unique_live).clone();
        unique_live.set_unique(
            &bundle.manifest,
            generation
                .shards()
                .map(|shard| (shard.id(), &*shard.buffer)),
            attribute,
            unique,
        );
        let ids: Vec<ShardId> = shards.iter().map(|(id, ..)| *id).collect();
        let next = generation.with_shards(ids, |g, parts| {
            g.bundle = bundle;
            g.unique_live = Arc::new(unique_live);
            for (id, unique_indexes, filter_columns) in shards {
                let s = parts.of(id);
                s.unique = unique_indexes;
                s.filter_columns = filter_columns;
            }
        });
        self.publish(next, started);
    }
}

/// A declaration's runs, as its log record names them, and each run's file digest.
struct DeclaredIndex {
    base: Vec<mosaica_lifecycle::wal::DeclaredRun>,
    live: Vec<mosaica_lifecycle::wal::DeclaredRun>,
    files: Vec<(String, FileDigest)>,
}
