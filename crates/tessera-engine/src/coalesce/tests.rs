use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use tessera_authz::DictStreamWriter;
use tessera_filter::RecordField;
use tessera_store::manifest::{Quantisation, SegmentsManifest, DECLARED_INCARNATION};
use tessera_store::{ExternalIdSidecar, FlushOutput};
use tessera_types::view::ViewIncarnation;
use tessera_types::{AttrLocalId, EntityId, TermId};

use super::*;
use crate::flush::SMALL_TERM_THRESHOLD;

const PARTITION: &str = "p0";
const OUT_REL: &str = "partitions/p0/coalesced/coalesce-1-1";
const VIEWS: [&str; 2] = ["quarter:2026-Q1", "quarter:2026-Q3"];
const KEYS: [&str; 5] = ["alpha", "beta", "gamma", "delta", "epsilon"];

fn policy() -> CoalescePolicy {
    CoalescePolicy {
        width: 3,
        floor_bytes: 1 << 20,
        max_input_bytes: 1 << 30,
        run_width: 3,
        run_floor_bytes: 1 << 20,
    }
}

fn all_live(_view: &str, _incarnation: ViewIncarnation) -> bool {
    true
}

/// The entities flush `flush` wrote. The build wrote 0 to 3.
fn entities_of(flush: u32) -> Vec<u32> {
    (0..3).map(|j| 10 + 10 * flush + j).collect()
}

fn flushed_entities() -> impl Iterator<Item = u32> {
    (0..4).flat_map(entities_of)
}

/// A keyword value that each flush's extent numbers differently.
fn key_of(entity: u32) -> String {
    KEYS[((entity / 10 + entity) % 5) as usize].to_string()
}

/// A reading per `(column, view)`.
type ByColumn<V> = BTreeMap<(String, Option<String>), V>;
/// The entities each word of a text column names.
type Words = BTreeMap<String, BTreeSet<u32>>;

/// One flush's values for one attribute column.
enum Values {
    Plain(Vec<u32>),
    Keyword(Vec<String>),
}

/// A prefix directory of real files, with the side-manifest and the build's digests that list
/// them.
struct Fixture {
    _dir: tempfile::TempDir,
    prefix_dir: PathBuf,
    manifest: SegmentsManifest,
    build_files: BTreeMap<String, FileDigest>,
}

impl Fixture {
    fn empty() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix_dir = dir.path().join("v00000");
        std::fs::create_dir_all(&prefix_dir).unwrap();
        Fixture {
            _dir: dir,
            prefix_dir,
            manifest: SegmentsManifest::empty(),
            build_files: BTreeMap::new(),
        }
    }

    /// The build's run and dictionary, then four flushes of every kind the coalesce takes.
    fn with_every_kind() -> Self {
        let mut fx = Fixture::empty();
        fx.write_build();
        for flush in 0..4 {
            fx.write_flush(flush);
        }
        fx
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.prefix_dir.join(rel)
    }

    fn rel(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.prefix_dir).unwrap();
        rel.to_str().unwrap().to_string()
    }

    fn digest_of(&self, rel: &str) -> FileDigest {
        tessera_store::digest_of(&self.path(rel)).unwrap()
    }

    fn digest<'a>(&mut self, rels: impl IntoIterator<Item = &'a str>) {
        for rel in rels {
            let digest = self.digest_of(rel);
            self.manifest.files.insert(rel.to_string(), digest);
        }
    }

    fn plan(&self) -> Option<CoalescePlan> {
        plan_coalesce(PARTITION, &self.manifest, &self.build_files, policy(), &all_live)
    }

    fn execute(&self, plan: CoalescePlan) -> Result<CompletedCoalesce, MaintenanceFailed> {
        let ctx = CoalesceContext {
            prefix_dir: self.prefix_dir.clone(),
            prefix: "v00000".to_string(),
            out_rel: OUT_REL.to_string(),
        };
        execute_coalesce(plan, ctx)
    }

    /// A geometry segment with its external-id run and locator. Every entity ending in 2 has no
    /// external id.
    fn write_segment(&self, seg: &str, entities: &[u32], row_base: u32) -> FlushOutput {
        let key = tessera_types::IdentityKey::from_hex("0123456789abcdef0123456789abcdef").unwrap();
        let rows = entities
            .iter()
            .map(|&e| tessera_store::FlushRow {
                entity_id: EntityId::new(e.into()),
                external_id: (e % 10 != 2).then(|| format!("ext-{e}").into_bytes()),
                x: 0.5,
                y: 0.5,
                scalars: Vec::new(),
            })
            .collect();
        let input = tessera_store::FlushInput {
            seg_id: seg,
            incarnation: DECLARED_INCARNATION,
            rows,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            identity_key: &key,
            shard_id: 0,
            scalar_schema: &[],
            row_base,
        };
        tessera_store::write_flush_segment(&self.prefix_dir, PARTITION, "s0", input, &[]).unwrap()
    }

    fn write_dict(&self, dir_rel: &str, descriptors: &[String]) -> String {
        let dir = self.path(dir_rel);
        std::fs::create_dir_all(&dir).unwrap();
        let mut writer = DictStreamWriter::new(&dir);
        for descriptor in descriptors {
            writer.append(descriptor.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        format!("{dir_rel}/terms-0.dict")
    }

    /// What the build leaves: a run and a dictionary that `MANIFEST.json` digests.
    fn write_build(&mut self) {
        let base = self.write_segment("base", &[0, 1, 2, 3], 0);
        self.build_files.extend(base.files);
        let run = base.locator_extent.expect("the base binds").external_id_run;
        self.manifest.external_id_runs.push(run);
        let path = self.write_dict("terms", &["base-0".into(), "base-1".into()]);
        self.build_files.insert(path.clone(), self.digest_of(&path));
        self.manifest.dict_extents.push(DictExtent { path, records: 2 });
    }

    fn write_flush(&mut self, flush: u32) {
        let entities = entities_of(flush);
        let seg = format!("flush-{flush}-1");
        let seg_rel = self.write_bindings(flush, &entities);

        let tier = format!("{seg_rel}/delta.arrow");
        let pairs = [
            (TermId::new(1), entities.clone()),
            (TermId::new(100 + flush), vec![entities[0]]),
        ];
        tessera_authz::write_delta_tier(&self.path(&tier), &pairs, SMALL_TERM_THRESHOLD).unwrap();
        self.digest([tier.as_str()]);
        self.manifest.deltas.push(tier);

        let descriptors: Vec<String> = (0..2).map(|j| format!("flush-{flush}-{j}")).collect();
        let path = self.write_dict(&seg_rel, &descriptors);
        self.digest([path.as_str()]);
        self.manifest.dict_extents.push(DictExtent { path, records: 2 });

        let years = entities.iter().map(|e| 2000 + e).collect();
        self.write_attr("year", None, &seg, &entities, Values::Plain(years));
        let titles = entities.iter().map(|&e| key_of(e)).collect();
        self.write_attr("title", None, &seg, &entities, Values::Keyword(titles));
        for (i, view) in VIEWS.into_iter().enumerate() {
            let moods = entities.iter().map(|e| e * 10 + i as u32).collect();
            let view = Some((view, DECLARED_INCARNATION));
            self.write_attr("mood", view, &seg, &entities, Values::Plain(moods));
        }

        self.write_record(&seg, &entities);
        self.write_text("notes", None, &seg, &entities);
        for view in VIEWS {
            self.write_text("memo", Some(view), &seg, &entities);
        }
        self.write_entity_terms(&seg, flush, &entities);
    }

    /// A flush's segment, external-id run and locator extent, listed; returns its directory.
    fn write_bindings(&mut self, flush: u32, entities: &[u32]) -> String {
        let seg = format!("flush-{flush}-1");
        let segment = self.write_segment(&seg, entities, 4 + 3 * flush);
        self.manifest.files.extend(segment.files);
        let locator = segment.locator_extent.expect("the flush binds");
        self.manifest.external_id_runs.push(locator.external_id_run.clone());
        let seg_rel = locator.path.rsplit_once('/').unwrap().0.to_string();
        self.manifest.locator_extents.push(locator);
        seg_rel
    }

    fn column_rel(column: &str, view: Option<(&str, ViewIncarnation)>) -> String {
        match view {
            None => format!("partitions/{PARTITION}/attrs/{column}"),
            Some((view, incarnation)) => {
                tessera_store::scoped_column_rel(PARTITION, column, view, incarnation)
            }
        }
    }

    fn write_attr(
        &mut self,
        column: &str,
        view: Option<(&str, ViewIncarnation)>,
        flush: &str,
        entities: &[u32],
        values: Values,
    ) {
        let (codes, dict) = match values {
            Values::Plain(codes) => (codes, None),
            Values::Keyword(keys) => {
                let mut sorted = keys.clone();
                sorted.sort_unstable();
                sorted.dedup();
                let codes = keys.iter().map(|k| sorted.binary_search(k).unwrap() as u32).collect();
                (codes, Some(sorted))
            }
        };
        let presence: croaring::Bitmap = entities.iter().copied().collect();
        let dict_keys: Option<Vec<&str>> =
            dict.as_ref().map(|d| d.iter().map(String::as_str).collect());
        let (values, presence, dict) = tessera_filter::write_extent(
            &self.path(&Self::column_rel(column, view)),
            flush,
            &tessera_filter::Codes::U32(codes.into()),
            &presence,
            dict_keys.as_deref(),
        )
        .unwrap();
        let extent = AttrExtent {
            column: column.to_string(),
            view: view.map(|(v, _)| v.to_string()),
            incarnation: view.map(|(_, i)| i),
            values: self.rel(&values),
            presence: self.rel(&presence),
            dict: dict.map(|d| self.rel(&d)),
            postings: None,
            offsets: None,
        };
        self.digest(extent.files());
        self.manifest.attr_extents.push(extent);
    }

    fn write_record(&mut self, seg: &str, entities: &[u32]) {
        let dir = format!("partitions/{PARTITION}/attrs/record/extents");
        std::fs::create_dir_all(self.path(&dir)).unwrap();
        let extent = RecordExtent {
            blocks: format!("{dir}/{seg}.blocks.bin"),
            hasrow: format!("{dir}/{seg}.hasrow.roaring"),
            directory: format!("{dir}/{seg}.directory.arrow"),
        };
        let mut writer = tessera_filter_write::RecordBlobWriter::create(
            &self.path(&extent.blocks),
            &self.path(&extent.hasrow),
            &self.path(&extent.directory),
            tessera_filter::RECORD_BLOCK_TARGET,
        )
        .unwrap();
        for &e in entities {
            let name = format!("name-{e}");
            let fields = [
                tessera_filter::RecordFieldRef {
                    tag: 0,
                    value: tessera_filter::RecordValueRef::Utf8(&name),
                },
                tessera_filter::RecordFieldRef {
                    tag: 1,
                    value: tessera_filter::RecordValueRef::U32(e),
                },
            ];
            writer.push_row(e, &fields).unwrap();
        }
        writer.finish().unwrap();
        self.digest(extent.files());
        self.manifest.record_extents.push(extent);
    }

    fn write_text(&mut self, column: &str, view: Option<&str>, seg: &str, entities: &[u32]) {
        let view = view.map(|v| (v, DECLARED_INCARNATION));
        let dir = format!("{}/text", Self::column_rel(column, view));
        std::fs::create_dir_all(self.path(&dir)).unwrap();
        let mut terms: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for &e in entities {
            for word in [format!("word-{}", e % 3), format!("only-{e}")] {
                terms.entry(word).or_default().push(e);
            }
        }
        let extent = TextExtent {
            column: column.to_string(),
            view: view.map(|(v, _)| v.to_string()),
            incarnation: view.map(|(_, i)| i),
            dict: format!("{dir}/{seg}-dict.bin"),
            postings: format!("{dir}/{seg}-postings.arrow"),
            presence: format!("{dir}/{seg}-presence.roaring"),
        };
        let words = terms.keys().map(String::as_str);
        tessera_filter::write_sorted_dict(&self.path(&extent.dict), words).unwrap();
        let per_term: Vec<Vec<u32>> = terms.into_values().collect();
        tessera_authz::write_postings(&self.path(&extent.postings), &per_term, SMALL_TERM_THRESHOLD)
            .unwrap();
        let presence: croaring::Bitmap = entities.iter().copied().collect();
        std::fs::write(self.path(&extent.presence), presence.serialize::<croaring::Portable>())
            .unwrap();
        self.digest(extent.files());
        self.manifest.text_extents.push(extent);
    }

    fn write_entity_terms(&mut self, seg: &str, flush: u32, entities: &[u32]) {
        let dir = format!("partitions/{PARTITION}/entities/terms/extents");
        std::fs::create_dir_all(self.path(&dir)).unwrap();
        let extent = EntityTermsExtent {
            hasrow: format!("{dir}/{seg}.hasrow.roaring"),
            offsets: format!("{dir}/{seg}.offsets.u32"),
            terms: format!("{dir}/{seg}.terms.u32"),
            bases: format!("{dir}/{seg}.bases.u64"),
        };
        let mut writer = tessera_store::EntityTermsWriter::create_at(
            &self.path(&extent.hasrow),
            &self.path(&extent.offsets),
            &self.path(&extent.terms),
            &self.path(&extent.bases),
        )
        .unwrap();
        for &e in entities {
            writer.push(e, &[1, 100 + flush]).unwrap();
        }
        writer.finish().unwrap();
        self.digest(extent.files());
        self.manifest.entity_terms_extents.push(extent);
    }

    /// Every term's entities, unioned over `tiers`.
    fn tier_pairs(&self, tiers: &[String]) -> BTreeMap<u32, BTreeSet<u32>> {
        let mut pairs: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for rel in tiers {
            let tier = DeltaTier::open(&self.path(rel)).unwrap();
            for term in tier.terms() {
                let mut entities = Vec::new();
                tier.posting(term).unwrap().unwrap().extend_into(&mut entities);
                pairs.entry(term.raw()).or_default().extend(entities);
            }
        }
        pairs
    }

    /// Each flushed entity's external id, and the entity each external id resolves to.
    fn external_ids(&self, manifest: &SegmentsManifest) -> Vec<(Option<Vec<u8>>, Option<u64>)> {
        self.external_ids_of(manifest, flushed_entities())
    }

    /// [`Self::external_ids`] over `entities`, through a sidecar opened from `manifest` as a
    /// restart opens it.
    fn external_ids_of(
        &self,
        manifest: &SegmentsManifest,
        entities: impl IntoIterator<Item = u32>,
    ) -> Vec<(Option<Vec<u8>>, Option<u64>)> {
        let generation = crate::Generation::synthetic(
            "v00000",
            0,
            0,
            tessera_lifecycle::Overlay::default(),
            tessera_lifecycle::IngestBuffer::default(),
        );
        let mut bundle = generation.bundle.manifest.clone();
        bundle.files = self.build_files.clone();
        bundle.entity_id_high_water = 4;
        let sidecar = ExternalIdSidecar::deferred_from_manifest(&bundle, manifest, &self.prefix_dir)
            .unwrap();
        entities
            .into_iter()
            .map(|e| {
                let external_id = sidecar.external_id_of_checked(EntityId::new(e.into()), 100);
                let entity = sidecar.resolve(format!("ext-{e}").as_bytes()).unwrap();
                (external_id.unwrap(), entity.map(EntityId::raw))
            })
            .collect()
    }

    /// The term id every descriptor resolves to through `dicts`.
    fn term_ids(&self, dicts: &[DictExtent]) -> Vec<Option<u32>> {
        let paths: Vec<PathBuf> = dicts.iter().map(|d| self.path(&d.path)).collect();
        let dict = tessera_authz::Dict::load(&paths).unwrap();
        let descriptors = ["base-0".to_string(), "base-1".to_string()]
            .into_iter()
            .chain((0..4).flat_map(|f| (0..2).map(move |j| format!("flush-{f}-{j}"))));
        descriptors.map(|d| dict.lookup(d.as_bytes()).map(TermId::raw)).collect()
    }

    /// Each column's value per entity, read through `extents` as a restart opens them.
    fn attr_values(&self, extents: &[AttrExtent]) -> ByColumn<BTreeMap<u32, String>> {
        let access = tessera_filter::Access::Read;
        let mut out: ByColumn<BTreeMap<u32, String>> = BTreeMap::new();
        let mut scratch = Vec::new();
        for extent in extents {
            let (values, presence) = (self.path(&extent.values), self.path(&extent.presence));
            let values = tessera_filter::open_extent(&values, &presence, access).unwrap();
            let dict = extent
                .dict
                .as_ref()
                .map(|d| tessera_filter::SortedDict::open(&self.path(d), access).unwrap());
            let column = out.entry((extent.column.clone(), extent.view.clone())).or_default();
            for e in flushed_entities() {
                let Some(value) = values.value_of(e) else { continue };
                let value = match &dict {
                    Some(dict) => dict.key_of(value.raw(), &mut scratch).unwrap().to_string(),
                    None => value.raw().to_string(),
                };
                column.insert(e, value);
            }
        }
        out
    }

    fn record_fields(&self, extents: &[RecordExtent]) -> BTreeMap<u32, Vec<RecordField>> {
        let mut out = BTreeMap::new();
        for extent in extents {
            let blob = tessera_filter::RecordBlob::open(
                &self.path(&extent.blocks),
                &self.path(&extent.hasrow),
                &self.path(&extent.directory),
                tessera_filter::Access::Read,
            )
            .unwrap();
            for e in flushed_entities() {
                if let Some(fields) = blob.fields_of(e).unwrap() {
                    out.insert(e, fields);
                }
            }
        }
        out
    }

    /// Each text column's entities per word, and the entities it holds any text for.
    fn text_postings(&self, extents: &[TextExtent]) -> ByColumn<(Words, BTreeSet<u32>)> {
        let mut out: ByColumn<(Words, BTreeSet<u32>)> = BTreeMap::new();
        for extent in extents {
            let key = (extent.column.clone(), extent.view.clone());
            let (words, present) = out.entry(key).or_default();
            let access = tessera_filter::Access::Read;
            let dict = tessera_filter::SortedDict::open(&self.path(&extent.dict), access).unwrap();
            let postings = self.path(&extent.postings);
            let postings = tessera_filter::ColumnPostings::open(&postings, false).unwrap();
            dict.walk(|ordinal, word| {
                let entities = postings.entities(AttrLocalId::new(ordinal)).unwrap();
                words.entry(word.to_string()).or_default().extend(entities.iter());
            })
            .unwrap();
            let bytes = std::fs::read(self.path(&extent.presence)).unwrap();
            present.extend(croaring::Bitmap::deserialize::<croaring::Portable>(&bytes).iter());
        }
        out
    }

    fn entity_terms(&self, extents: &[EntityTermsExtent]) -> BTreeMap<u32, Vec<u32>> {
        let mut out = BTreeMap::new();
        for extent in extents {
            let layer = tessera_store::EntityTerms::open(
                &self.path(&extent.hasrow),
                &self.path(&extent.offsets),
                &self.path(&extent.terms),
                &self.path(&extent.bases),
            )
            .unwrap();
            for e in flushed_entities() {
                if let Some(terms) = layer.terms_of(e).unwrap() {
                    out.insert(e, terms);
                }
            }
        }
        out
    }
}

/// The every-kind fixture, the pass run over it, and the manifest after the edit.
struct Coalesced {
    fx: Fixture,
    completed: CompletedCoalesce,
    after: SegmentsManifest,
}

fn coalesced() -> Coalesced {
    let fx = Fixture::with_every_kind();
    let plan = fx.plan().expect("every kind qualifies");
    let completed = fx.execute(plan).expect("the pass writes");
    let after = rebased(&fx.manifest, &completed).expect("nothing moved under the pass");
    Coalesced { fx, completed, after }
}

impl Coalesced {
    fn before(&self) -> &SegmentsManifest {
        &self.fx.manifest
    }

    /// `read` answers the same through the manifest after the edit as through the one before.
    fn assert_reads_same<T>(&self, read: impl Fn(&Fixture, &SegmentsManifest) -> T)
    where
        T: PartialEq + std::fmt::Debug,
    {
        assert_eq!(read(&self.fx, &self.after), read(&self.fx, self.before()));
    }

    /// Every consumed file has left `files`, and every written file is digested as it lies on
    /// disk.
    fn assert_files<'a>(
        &self,
        consumed: impl IntoIterator<Item = &'a str>,
        written: impl IntoIterator<Item = &'a str>,
    ) {
        for rel in consumed {
            assert!(!self.after.files.contains_key(rel), "{rel} is consumed but still digested");
        }
        for rel in written {
            let listed = self.after.files.get(rel);
            let listed = listed.unwrap_or_else(|| panic!("{rel} is not digested"));
            assert_eq!(listed.sha256, self.fx.digest_of(rel).sha256, "{rel}");
        }
    }
}

/// `list` with `consumed` removed and `replacement` where the first of them stood.
fn replaced(list: &[String], consumed: &[&str], replacement: &str) -> Vec<String> {
    let at = list.iter().position(|k| k == consumed[0]).expect("the window is listed");
    let kept = list.iter().filter(|k| !consumed.contains(&k.as_str()));
    let mut out: Vec<String> = kept.cloned().collect();
    out.insert(at, replacement.to_string());
    out
}

fn keys<T>(list: &[T], key: impl Fn(&T) -> &String) -> Vec<String> {
    list.iter().map(key).cloned().collect()
}

/// Coalesced tiers take their window's place and hold every term's entities.
#[test]
fn coalesced_tiers_keep_every_pair() {
    let c = coalesced();
    let m = c.completed.tier.as_ref().expect("the tiers are taken");
    let consumed: Vec<&str> = m.consumed.iter().map(String::as_str).collect();
    assert_eq!(c.after.deltas, replaced(&c.before().deltas, &consumed, &m.output.0));
    c.assert_files(consumed, [m.output.0.as_str()]);
    c.assert_reads_same(|fx, m| fx.tier_pairs(&m.deltas));
}

/// A coalesced run and its locator take the window's place behind the build's run, and every
/// entity and external id resolves as before.
#[test]
fn coalesced_runs_keep_every_binding() {
    let c = coalesced();
    let m = c.completed.run.as_ref().expect("the runs are taken");
    let runs: Vec<&str> = m.consumed.iter().map(|e| e.external_id_run.as_str()).collect();
    let before_runs = &c.before().external_id_runs;
    assert_eq!(c.after.external_id_runs, replaced(before_runs, &runs, &m.output.external_id_run));
    let locators: Vec<&str> = m.consumed.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        keys(&c.after.locator_extents, |e| &e.path),
        replaced(&keys(&c.before().locator_extents, |e| &e.path), &locators, &m.output.path)
    );
    c.assert_files(m.consumed.iter().flat_map(LocatorExtent::files), m.output.files());
    c.assert_reads_same(|fx, m| fx.external_ids(m));
}

/// A coalesced dictionary extent takes the window's place after the build's, so every
/// descriptor keeps its term id.
#[test]
fn coalesced_dictionaries_keep_every_term_id() {
    let c = coalesced();
    let m = c.completed.dict.as_ref().expect("the dictionaries are taken");
    let consumed: Vec<&str> = m.consumed.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        keys(&c.after.dict_extents, |e| &e.path),
        replaced(&keys(&c.before().dict_extents, |e| &e.path), &consumed, &m.output.path)
    );
    c.assert_files(consumed, [m.output.path.as_str()]);
    c.assert_reads_same(|fx, m| fx.term_ids(&m.dict_extents));
}

/// Each plain, keyword and scoped attribute window becomes one extent at its place in the
/// list, and every entity reads the same value through it.
#[test]
fn coalesced_attributes_keep_every_value() {
    let c = coalesced();
    let windows: BTreeSet<(&str, Option<&str>)> = c
        .completed
        .attrs
        .iter()
        .map(|m| (m.consumed.column.as_str(), m.consumed.view.as_deref()))
        .collect();
    let mood = VIEWS.map(|v| ("mood", Some(v)));
    let expected = [("year", None), ("title", None), mood[0], mood[1]];
    assert_eq!(windows, BTreeSet::from(expected));

    let mut list = keys(&c.before().attr_extents, |e| &e.values);
    for m in &c.completed.attrs {
        let consumed: Vec<&str> = m.consumed.extents.iter().map(|e| e.values.as_str()).collect();
        list = replaced(&list, &consumed, &m.output.extent.values);
        let written = m.output.extent.files();
        c.assert_files(m.consumed.extents.iter().flat_map(AttrExtent::files), written);
        assert_eq!(m.output.dict.is_some(), m.output.extent.dict.is_some());
    }
    assert_eq!(keys(&c.after.attr_extents, |e| &e.values), list);
    c.assert_reads_same(|fx, m| fx.attr_values(&m.attr_extents));
}

/// A coalesced record extent takes the window's place and every entity reads the same row.
#[test]
fn coalesced_records_keep_every_row() {
    let c = coalesced();
    let m = c.completed.record.as_ref().expect("the records are taken");
    let consumed: Vec<&str> = m.consumed.iter().map(|e| e.blocks.as_str()).collect();
    assert_eq!(
        keys(&c.after.record_extents, |e| &e.blocks),
        replaced(&keys(&c.before().record_extents, |e| &e.blocks), &consumed, &m.output.blocks)
    );
    c.assert_files(m.consumed.iter().flat_map(RecordExtent::files), m.output.files());
    c.assert_reads_same(|fx, m| fx.record_fields(&m.record_extents));
}

/// Each entity-scoped and scoped text window becomes one extent at its place in the list,
/// and every word finds the same entities.
#[test]
fn coalesced_texts_keep_every_posting() {
    let c = coalesced();
    assert_eq!(c.completed.texts.len(), 3, "notes, and memo in each view");
    let mut list = keys(&c.before().text_extents, |e| &e.dict);
    for m in &c.completed.texts {
        let consumed: Vec<&str> = m.consumed.extents.iter().map(|e| e.dict.as_str()).collect();
        list = replaced(&list, &consumed, &m.output.dict);
        c.assert_files(m.consumed.extents.iter().flat_map(TextExtent::files), m.output.files());
    }
    assert_eq!(keys(&c.after.text_extents, |e| &e.dict), list);
    c.assert_reads_same(|fx, m| fx.text_postings(&m.text_extents));
}

/// A coalesced entity-terms extent takes the window's place and every entity keeps its terms.
#[test]
fn coalesced_entity_terms_keep_every_list() {
    let c = coalesced();
    let m = c.completed.terms.as_ref().expect("the entity terms are taken");
    let consumed: Vec<&str> = m.consumed.iter().map(|e| e.terms.as_str()).collect();
    assert_eq!(
        keys(&c.after.entity_terms_extents, |e| &e.terms),
        replaced(&keys(&c.before().entity_terms_extents, |e| &e.terms), &consumed, &m.output.terms)
    );
    c.assert_files(m.consumed.iter().flat_map(EntityTermsExtent::files), m.output.files());
    c.assert_reads_same(|fx, m| fx.entity_terms(&m.entity_terms_extents));
}

/// A window with an entry gone from the list does not rebase.
#[test]
fn a_window_whose_entry_has_gone_does_not_rebase() {
    let c = coalesced();
    let tier = &c.completed.tier.as_ref().unwrap().consumed[1];
    let mut manifest = c.before().clone();
    manifest.deltas.retain(|t| t != tier);
    assert!(rebased(&manifest, &c.completed).is_none(), "a tier has gone");

    let attr = &c.completed.attrs[0].consumed.extents[1].values;
    let mut manifest = c.before().clone();
    manifest.attr_extents.retain(|e| &e.values != attr);
    assert!(rebased(&manifest, &c.completed).is_none(), "an attribute extent has gone");
}

/// A flush that lists another column's extent inside a window, and the same column's extent
/// after it, leaves the window to rebase in place.
#[test]
fn a_flush_published_during_the_pass_does_not_disturb_the_rebase() {
    let c = coalesced();
    let window = &c.completed.attrs.iter().find(|m| m.consumed.column == "year").unwrap().consumed;
    let mut manifest = c.before().clone();
    let (mut elsewhere, mut late) = (window.extents[0].clone(), window.extents[0].clone());
    elsewhere.column = "elsewhere".to_string();
    elsewhere.values = "late/elsewhere.arrow".to_string();
    late.values = "late/year.arrow".to_string();
    let second = &window.extents[1].values;
    let inside = manifest.attr_extents.iter().position(|e| &e.values == second).unwrap();
    manifest.attr_extents.insert(inside, elsewhere);
    manifest.attr_extents.push(late);

    let after = rebased(&manifest, &c.completed).expect("the windows are still in place");
    let mut list = keys(&manifest.attr_extents, |e| &e.values);
    for m in &c.completed.attrs {
        let consumed: Vec<&str> = m.consumed.extents.iter().map(|e| e.values.as_str()).collect();
        list = replaced(&list, &consumed, &m.output.extent.values);
    }
    assert_eq!(keys(&after.attr_extents, |e| &e.values), list);
}

/// Three layers of `title`, written for real, each keyword or plain as `keyword` says.
fn title_flushes(keyword: [bool; 3]) -> Fixture {
    let mut fx = Fixture::empty();
    for (flush, keyword) in (0..3).zip(keyword) {
        let entities = entities_of(flush);
        let values = match keyword {
            true => Values::Keyword(entities.iter().map(|&e| key_of(e)).collect()),
            false => Values::Plain(entities.clone()),
        };
        fx.write_attr("title", None, &format!("flush-{flush}-1"), &entities, values);
    }
    fx
}

/// A keyword layer whose ordinals reach past its own dictionary fails the pass, which leaves
/// nothing to publish.
#[test]
fn a_keyword_window_the_merge_refuses_fails_the_pass() {
    let fx = title_flushes([true; 3]);
    let faulted = fx.manifest.attr_extents[1].dict.clone().unwrap();
    tessera_filter::write_sorted_dict(&fx.path(&faulted), ["a"]).unwrap();
    let plan = fx.plan().expect("the window is planned");
    assert!(fx.execute(plan).is_err());
}

/// A window of one column mixing keyword layers with a plain one fails the pass.
#[test]
fn a_window_mixing_keyword_and_plain_layers_is_refused() {
    let fx = title_flushes([true, true, false]);
    let plan = fx.plan().expect("the window is planned");
    assert_eq!(plan.attrs[0].extents.len(), 3);
    assert!(fx.execute(plan).is_err());
}

fn digest(size: u64) -> FileDigest {
    FileDigest {
        size,
        sha256: "0".repeat(64),
    }
}

/// `flushes` flushes of tiers, runs, dictionaries and two interleaved columns, digested but
/// never written, behind the build's run and dictionary.
fn listed(flushes: u64) -> (SegmentsManifest, BTreeMap<String, FileDigest>) {
    let build_files = BTreeMap::from([
        ("entities/external-ids-0.arrow".to_string(), digest(4096)),
        ("terms/terms-0.dict".to_string(), digest(4096)),
    ]);
    let mut manifest = SegmentsManifest {
        dict_extents: vec![DictExtent {
            path: "terms/terms-0.dict".to_string(),
            records: 4,
        }],
        external_id_runs: vec!["entities/external-ids-0.arrow".to_string()],
        ..SegmentsManifest::empty()
    };
    for i in 0..flushes {
        let seg = format!("segments/flush-{i}");
        for name in ["delta.arrow", "external-ids.arrow", "ext-locator.u32", "terms-0.dict"] {
            manifest.files.insert(format!("{seg}/{name}"), digest(1024));
        }
        manifest.deltas.push(format!("{seg}/delta.arrow"));
        manifest.external_id_runs.push(format!("{seg}/external-ids.arrow"));
        manifest.locator_extents.push(LocatorExtent {
            path: format!("{seg}/ext-locator.u32"),
            entity_lo: i * 10,
            entity_hi: i * 10 + 9,
            external_id_run: format!("{seg}/external-ids.arrow"),
        });
        manifest.dict_extents.push(DictExtent {
            path: format!("{seg}/terms-0.dict"),
            records: 1,
        });
        for column in ["title", "department"] {
            list_attr(&mut manifest, column, None, i);
        }
    }
    (manifest, build_files)
}

/// Lists one flush's extent of `column`, or of a view's column at an incarnation.
fn list_attr(
    manifest: &mut SegmentsManifest,
    column: &str,
    view: Option<(&str, ViewIncarnation)>,
    flush: u64,
) -> AttrExtent {
    let dir = match view {
        None => format!("attrs/{column}"),
        Some((view, incarnation)) => format!("attrs/{column}/{view}/{incarnation}"),
    };
    let extent = AttrExtent {
        column: column.to_string(),
        view: view.map(|(v, _)| v.to_string()),
        incarnation: view.map(|(_, i)| i),
        values: format!("{dir}/flush-{flush}.arrow"),
        presence: format!("{dir}/flush-{flush}.roaring"),
        dict: None,
        postings: None,
        offsets: None,
    };
    manifest.files.insert(extent.values.clone(), digest(1024));
    manifest.files.insert(extent.presence.clone(), digest(64));
    manifest.attr_extents.push(extent.clone());
    extent
}

fn plan(manifest: &SegmentsManifest, build: &BTreeMap<String, FileDigest>) -> Option<CoalescePlan> {
    plan_coalesce(PARTITION, manifest, build, policy(), &all_live)
}

fn window_len(plan: &CoalescePlan, column: &str) -> Option<usize> {
    plan.attrs.iter().find(|w| w.column == column).map(|w| w.extents.len())
}

/// The build's run and dictionary are never taken, and every later entry is, whether the
/// side-manifest digests it or a fold has moved its digest into `MANIFEST.json`.
#[test]
fn the_builds_run_and_dictionary_are_never_taken() {
    let (mut manifest, mut build_files) = listed(3);
    for folded in [false, true] {
        if folded {
            build_files.extend(std::mem::take(&mut manifest.files));
        }
        let plan = plan(&manifest, &build_files).expect("a plan");
        assert_eq!(plan.tiers, manifest.deltas, "folded: {folded}");
        let runs: Vec<&String> = plan.locators.iter().map(|e| &e.external_id_run).collect();
        let expected: Vec<&String> = manifest.external_id_runs[1..].iter().collect();
        assert_eq!(runs, expected, "folded: {folded}");
        let dicts: Vec<&String> = plan.dicts.iter().map(|e| &e.path).collect();
        let expected: Vec<&String> = manifest.dict_extents[1..].iter().map(|e| &e.path).collect();
        assert_eq!(dicts, expected, "folded: {folded}");
    }
}

/// An entry naming a file neither manifest digests is not taken.
#[test]
fn an_entry_with_an_undigested_file_is_not_taken() {
    let (mut manifest, build_files) = listed(3);
    for name in ["delta.arrow", "ext-locator.u32", "terms-0.dict"] {
        manifest.files.remove(&format!("segments/flush-1/{name}"));
    }
    manifest.files.remove("attrs/title/flush-1.roaring");
    let plan = plan(&manifest, &build_files).expect("a plan");
    assert!(plan.tiers.is_empty() && plan.locators.is_empty() && plan.dicts.is_empty());
    let columns: Vec<&str> = plan.attrs.iter().map(|w| w.column.as_str()).collect();
    assert_eq!(columns, ["department"]);
}

/// Flushes whose entity ids interleave, as two views' flushes from one commit window do, have
/// overlapping locator spans. They coalesce into one extent over the union of the spans, and every
/// entity and external id resolves both ways as before, through a sidecar opened as a restart
/// opens it.
#[test]
fn overlapping_locator_spans_coalesce_and_keep_every_binding() {
    let mut fx = Fixture::empty();
    fx.write_build();
    let flushes = [vec![10, 13, 16], vec![11, 14, 17], vec![12, 15, 18]];
    for (flush, entities) in flushes.iter().enumerate() {
        fx.write_bindings(flush as u32, entities);
    }
    let entities = 10..=18;
    let before = fx.external_ids_of(&fx.manifest, entities.clone());
    for (entity, (key, resolved)) in entities.clone().zip(&before) {
        let expected = (entity % 10 != 2).then(|| format!("ext-{entity}").into_bytes());
        assert_eq!(key, &expected, "entity {entity} names its key");
        assert_eq!(*resolved, expected.map(|_| u64::from(entity)), "ext-{entity} names its entity");
    }

    let plan = fx.plan().expect("a plan");
    assert_eq!(plan.locators.len(), 3, "the overlapping extents are taken");
    let completed = fx.execute(plan).expect("the pass writes");
    let after = rebased(&fx.manifest, &completed).expect("nothing moved under the pass");
    let merged = &completed.run.as_ref().expect("the runs are coalesced").output;
    assert_eq!((merged.entity_lo, merged.entity_hi), (10, 18), "the union of the spans");
    assert_eq!(fx.external_ids_of(&after, entities), before);
}

/// A window whose entries fall in two size classes is not taken.
#[test]
fn a_window_spanning_two_size_classes_is_not_taken() {
    let (mut manifest, build_files) = listed(3);
    manifest.files.insert(manifest.deltas[1].clone(), digest(64 << 20));
    let plan = plan(&manifest, &build_files).expect("the other kinds qualify");
    assert!(plan.tiers.is_empty());
}

/// Under the built-in policy the runs are taken four at a time while the other kinds wait for
/// eight, and runs below the run floor share one size class with a run far larger than the other
/// kinds' floor.
#[test]
fn the_runs_take_their_own_narrower_window_and_floor() {
    let (mut manifest, build_files) = listed(4);
    manifest
        .files
        .insert(manifest.external_id_runs[1].clone(), digest(8 << 20));
    let plan = plan_coalesce(
        PARTITION,
        &manifest,
        &build_files,
        CoalescePolicy::default(),
        &all_live,
    )
    .expect("the runs qualify");
    assert_eq!(plan.locators.len(), 4);
    assert!(plan.tiers.is_empty() && plan.dicts.is_empty() && plan.attrs.is_empty());
}

/// Fewer entries than the width plan nothing.
#[test]
fn nothing_is_taken_below_the_width() {
    let (manifest, build_files) = listed(2);
    assert!(plan(&manifest, &build_files).is_none());
}

/// A column over the input cap takes a narrower window, or none if one extent alone exceeds it,
/// and its neighbour is unaffected.
#[test]
fn the_input_cap_narrows_one_columns_window_and_stalls_only_that_column() {
    let (mut manifest, build_files) = listed(4);
    for extent in manifest.attr_extents.iter().filter(|e| e.column == "title") {
        manifest.files.insert(extent.values.clone(), digest(2 << 20));
    }
    let mut policy = policy();
    policy.max_input_bytes = 5 << 20;
    let narrowed = plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).unwrap();
    assert_eq!(window_len(&narrowed, "title"), Some(2));
    assert_eq!(window_len(&narrowed, "department"), Some(3));

    policy.max_input_bytes = 1 << 20;
    let stalled = plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).unwrap();
    assert_eq!(window_len(&stalled, "title"), None);
    assert_eq!(window_len(&stalled, "department"), Some(3));
}

/// A layer's dictionary counts toward the input cap with its values.
#[test]
fn a_layers_dictionary_counts_toward_the_input_cap() {
    let (mut manifest, build_files) = listed(0);
    for flush in 0..3 {
        let mut extent = list_attr(&mut manifest, "submitter", None, flush);
        let dict = format!("attrs/submitter/flush-{flush}.dict");
        manifest.files.insert(extent.values.clone(), digest(1 << 20));
        manifest.files.insert(extent.presence.clone(), digest(0));
        manifest.files.insert(dict.clone(), digest(1 << 20));
        extent.dict = Some(dict);
        *manifest.attr_extents.last_mut().unwrap() = extent;
    }
    let mut policy = policy();
    policy.max_input_bytes = 4 << 20;
    let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).unwrap();
    assert_eq!(window_len(&plan, "submitter"), Some(2), "three are 6 MiB with dictionaries");
}

/// Each column's window is taken from its own extents, and a column whose layers carry
/// dictionaries is taken like any other.
#[test]
fn each_column_takes_a_window_of_its_own_extents() {
    let (mut manifest, build_files) = listed(4);
    for extent in manifest.attr_extents.iter_mut().filter(|e| e.column == "title") {
        let dict = format!("{}.dict", extent.values);
        manifest.files.insert(dict.clone(), digest(64));
        extent.dict = Some(dict);
    }
    let plan = plan(&manifest, &build_files).expect("a plan");
    assert_eq!(plan.attrs.len(), 2);
    for window in &plan.attrs {
        assert_eq!(window.extents.len(), 3);
        assert!(window.extents.iter().all(|e| e.column == window.column));
        assert!(window.extents.iter().all(|e| e.dict.is_some() == (window.column == "title")));
    }
}

/// A group-scoped column gets one window per view, holding only that view's extents, and an
/// entity-scoped column's window names no view.
#[test]
fn a_scoped_column_gets_one_window_per_view() {
    let (mut manifest, build_files) = listed(3);
    for flush in 0..3 {
        for view in VIEWS {
            list_attr(&mut manifest, "mood", Some((view, DECLARED_INCARNATION)), flush);
        }
    }
    let plan = plan(&manifest, &build_files).expect("a plan");
    let mut views = BTreeSet::new();
    for window in &plan.attrs {
        assert_eq!(window.extents.len(), 3);
        if window.column == "mood" {
            views.insert(window.view.as_deref().expect("a scoped window names its view"));
            assert_eq!(window.incarnation, Some(DECLARED_INCARNATION));
        } else {
            assert_eq!((window.view.as_deref(), window.incarnation), (None, None));
        }
        assert!(window.extents.iter().all(|e| e.key() == window.key()));
    }
    assert_eq!(views, BTreeSet::from(VIEWS));
}

/// Extents of a view's dropped incarnation are not planned, and the live incarnation's are.
#[test]
fn a_dead_incarnations_window_is_not_planned() {
    let (mut manifest, build_files) = listed(0);
    let view = VIEWS[0];
    for flush in 0..3 {
        for incarnation in [0, 4] {
            list_attr(&mut manifest, "mood", Some((view, incarnation)), flush);
        }
    }
    let is_live = |v: &str, incarnation| v == view && incarnation == 4;
    let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy(), &is_live).unwrap();
    assert_eq!(plan.attrs.len(), 1);
    assert_eq!(plan.attrs[0].incarnation, Some(4));
    assert!(plan.attrs[0].extents.iter().all(|e| e.incarnation == Some(4)));
}
