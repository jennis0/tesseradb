use std::collections::BTreeSet;

use tessera_store::manifest::SegmentsManifest;

use super::plan::select_window;
use super::*;
use crate::flush::SMALL_TERM_THRESHOLD;

const PARTITION: &str = "p0";

fn policy() -> CoalescePolicy {
    CoalescePolicy {
        width: 3,
        floor_bytes: 1 << 20,
        max_input_bytes: 1 << 30,
    }
}

/// A real, empty coalesced tier — the reader `publish_coalesce` installs on the generation.
/// Built rather than stubbed because `CompletedCoalesce` carries the opened reader, and a test
/// double there would be a second definition of what a tier is.
fn tier_at(dir: &std::path::Path) -> (String, Arc<DeltaTier>) {
    let path = dir.join("delta.arrow");
    tessera_authz::write_delta_tier(&path, &[], SMALL_TERM_THRESHOLD).expect("a tier writes");
    (
        "c/delta.arrow".to_string(),
        Arc::new(DeltaTier::open(&path).expect("it opens")),
    )
}

fn digest(size: u64) -> FileDigest {
    FileDigest {
        size,
        sha256: "0".repeat(64),
    }
}

/// A manifest with `flushes` flushes' worth of entity-space artefacts on every axis, plus the
/// build's own run and dictionary extent — which is the arrangement `tessera build` leaves and
/// every selection rule below is stated against.
fn manifest_with(flushes: u64) -> (SegmentsManifest, BTreeMap<String, FileDigest>) {
    let build_files: BTreeMap<String, FileDigest> = [
        ("entities/external-ids-0.arrow".to_string(), digest(4096)),
        ("terms/terms-0.dict".to_string(), digest(4096)),
    ]
    .into_iter()
    .collect();

    let mut manifest = SegmentsManifest {
        dict_extents: vec![DictExtent {
            path: "terms/terms-0.dict".to_string(),
            records: 4,
        }],
        external_id_runs: vec!["entities/external-ids-0.arrow".to_string()],
        ..SegmentsManifest::empty()
    };
    for i in 0..flushes {
        let seg = format!("partitions/{PARTITION}/views/s0/segments/flush-{i}-1");
        for name in [
            "delta.arrow",
            "external-ids.arrow",
            "ext-locator.u32",
            "terms-0.dict",
        ] {
            manifest.files.insert(format!("{seg}/{name}"), digest(1024));
        }
        manifest.deltas.push(format!("{seg}/delta.arrow"));
        manifest
            .external_id_runs
            .push(format!("{seg}/external-ids.arrow"));
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
        // Two filterable columns, both extended by every flush — so the list interleaves them
        // exactly as a flush leaves it, and a selection that read the list rather than a
        // column's own subsequence would take one of each.
        for column in COLUMNS {
            let extent = attr_extent_at(PARTITION, column, &format!("flush-{i}-1"));
            manifest.files.insert(extent.values.clone(), digest(1024));
            manifest.files.insert(extent.presence.clone(), digest(64));
            manifest.attr_extents.push(extent);
        }
    }
    (manifest, build_files)
}

/// The fixture roster: every view of every extent here is live at the build's incarnation, so
/// the liveness filter is a no-op and each test is about the axis it names. The one test that
/// is about the filter supplies its own.
fn all_live(_view: &str, _incarnation: tessera_types::view::ViewIncarnation) -> bool {
    true
}

/// The two filterable columns every fixture manifest carries extents for.
const COLUMNS: [&str; 2] = ["title", "department"];

/// One flush's extent for one **view's** column of a group-scoped family (`views.md` §5).
fn scoped_extent_at(partition: &str, column: &str, view: &str, flush: &str) -> AttrExtent {
    let (group, key) = view.split_once(':').expect("a view of a group");
    let dir = format!("partitions/{partition}/attrs/{column}/{group}/{key}/extents");
    AttrExtent {
        // Present exactly when `view` is (decision 0115); the fixture's views are the build's.
        incarnation: Some(tessera_store::manifest::DECLARED_INCARNATION),
        column: column.to_string(),
        view: Some(view.to_string()),
        values: format!("{dir}/{flush}.arrow"),
        presence: format!("{dir}/{flush}.roaring"),
        dict: None,
        postings: None,
        offsets: None,
    }
}

/// One flush's extent for one view's column at a **named** incarnation, so a test can put two
/// incarnations of one key in the list (decision 0115).
fn scoped_extent_of(
    partition: &str,
    column: &str,
    view: &str,
    incarnation: tessera_types::view::ViewIncarnation,
    flush: &str,
) -> AttrExtent {
    let mut extent = scoped_extent_at(partition, column, view, flush);
    extent.incarnation = Some(incarnation);
    extent
}

/// **A dead incarnation's extents are not coalesced** (decision 0115).
///
/// The hazard is not wasted work. `coalesced_column_rel` derives the output path from
/// `(column, view)` and nothing else, so a dead incarnation's window and the live one's
/// resolve to the *same* files — two merges, one path, each truncating the other's mapped
/// output. The live view would then serve whichever landed last, under a digest describing
/// neither. Skipping the dead window is what makes the path collision unreachable, and it is
/// also correct on its own terms: those files are the fold's to reclaim.
///
/// **Mutation this kills:** drop the `live_window` guard in `plan_coalesce` and the plan
/// carries two windows for one view id.
#[test]
fn a_dead_incarnations_window_is_not_planned() {
    let (mut manifest, build_files) = manifest_with(0);
    let view = "quarter:2026-Q1";
    // The key was dropped at incarnation 0 and created again at 4. Both incarnations' extents
    // are in the list, because the fold that reclaims the first has not run.
    for i in 0..3 {
        for incarnation in [0, 4] {
            let extent = scoped_extent_of(
                PARTITION,
                "mood",
                view,
                incarnation,
                &format!("flush-{i}-{incarnation}"),
            );
            manifest.files.insert(extent.values.clone(), digest(1024));
            manifest.files.insert(extent.presence.clone(), digest(64));
            manifest.attr_extents.push(extent);
        }
    }
    let dead_extents: Vec<AttrExtent> = manifest
        .attr_extents
        .iter()
        .filter(|e| e.incarnation == Some(0))
        .cloned()
        .collect();
    assert_eq!(dead_extents.len(), 3, "the fixture holds the dead ones too");

    let plan = plan_coalesce(
        PARTITION,
        &manifest,
        &build_files,
        policy(),
        // Incarnation 4 is what the roster says this key is now.
        &|v: &str, incarnation| v == view && incarnation == 4,
    )
    .expect("the live incarnation's window still qualifies");
    assert_eq!(
        plan.attrs.len(),
        1,
        "one window, and it is the live incarnation's — two would write one path twice"
    );
    let window = &plan.attrs[0];
    assert_eq!(window.view.as_deref(), Some(view));
    assert_eq!(window.incarnation, Some(4));
    assert!(
        window.extents.iter().all(|e| e.incarnation == Some(4)),
        "no dead extent is inside the live window either"
    );
    // And the dead extents are left exactly as they were: the plan consumes none of them, so
    // the fold still finds them to omit.
    let consumed: BTreeSet<&str> = plan
        .attrs
        .iter()
        .flat_map(|w| w.extents.iter())
        .map(|e| e.values.as_str())
        .collect();
    assert!(
        dead_extents
            .iter()
            .all(|e| !consumed.contains(e.values.as_str())),
        "the dead incarnation's files are untouched"
    );
}

/// **A scoped family's window is its `(column, view)`'s, not its column's** (`views.md` §5).
///
/// A family has one column per view of its group and they share the column's *name*, so a
/// selection keyed on the name alone would put two views' extents in one window — and the
/// merge would then write one file claiming both views' entities, under one view's directory.
/// Every answer either view gave afterwards would be a plausible wrong one, which is why this
/// is asserted on the plan rather than left to the pass.
#[test]
fn a_scoped_familys_window_is_one_views_own() {
    let (mut manifest, build_files) = manifest_with(0);
    // Three extents per view, interleaved exactly as two views flushing in turn leave them, so
    // a selection reading the list rather than each column's own subsequence would take one of
    // each.
    for i in 0..3 {
        for view in ["quarter:2026-Q1", "quarter:2026-Q3"] {
            let extent = scoped_extent_at(PARTITION, "mood", view, &format!("flush-{i}-1"));
            manifest.files.insert(extent.values.clone(), digest(1024));
            manifest.files.insert(extent.presence.clone(), digest(64));
            manifest.attr_extents.push(extent);
        }
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(
        plan.attrs.len(),
        2,
        "one window per view, not one per column"
    );
    for window in &plan.attrs {
        assert_eq!(window.column, "mood");
        let view = window
            .view
            .as_deref()
            .expect("a scoped window names its view");
        assert!(
            window
                .extents
                .iter()
                .all(|e| e.view.as_deref() == Some(view)),
            "{view}'s window holds only {view}'s extents"
        );
        let (group, key) = view.split_once(':').unwrap();
        assert!(
            window
                .extents
                .iter()
                .all(|e| e.values.contains(&format!("/{group}/{key}/"))),
            "{view}'s extents live under its own directory"
        );
    }
    let views: BTreeSet<&str> = plan
        .attrs
        .iter()
        .filter_map(|w| w.view.as_deref())
        .collect();
    assert_eq!(
        views,
        BTreeSet::from(["quarter:2026-Q1", "quarter:2026-Q3"]),
        "both views' columns are taken"
    );
}

/// **And an entity-scoped column's window is still keyed on the column alone**, which is what
/// makes the pair above the identity rather than the view: a bundle with no family at all
/// plans exactly what it planned before the field existed.
#[test]
fn an_entity_scoped_window_names_no_view() {
    let (manifest, build_files) = manifest_with(3);
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert!(!plan.attrs.is_empty(), "the fixture's own columns qualify");
    assert!(
        plan.attrs.iter().all(|w| w.view.is_none()),
        "a declared column belongs to no view"
    );
}

fn attr_extent_at(partition: &str, column: &str, flush: &str) -> AttrExtent {
    let dir = format!("partitions/{partition}/attrs/{column}/extents");
    AttrExtent {
        incarnation: None,
        column: column.to_string(),
        view: None,
        values: format!("{dir}/{flush}.arrow"),
        presence: format!("{dir}/{flush}.roaring"),
        dict: None,
        postings: None,
        offsets: None,
    }
}

/// A completed pass carrying one coalesced extent per planned window, with an opened column
/// standing in for the merged one. The reader is real — an empty extent is still a column —
/// because `CompletedCoalesce` carries the opened reader and a double there would be a second
/// definition of what an extent is.
fn completed_attrs(plan: &CoalescePlan, out_rel: &str) -> Vec<crate::filter::OpenedExtent> {
    plan.attrs
        .iter()
        .map(|window| {
            let column_rel =
                coalesced_column_rel(out_rel, &window.column, window.view.as_deref());
            crate::filter::OpenedExtent {
                extent: AttrExtent {
                    incarnation: window.incarnation,
                    column: window.column.clone(),
                    view: window.view.clone(),
                    values: format!("{column_rel}/values.arrow"),
                    presence: format!("{column_rel}/presence.roaring"),
                    dict: None,
                    postings: None,
                    offsets: None,
                },
                values: Arc::new(
                    tessera_filter::ValueColumn::partial(
                        tessera_filter::Codes::U32(Vec::<u32>::new().into()),
                        croaring::Bitmap::new(),
                    )
                    .expect("an empty extent"),
                ),
                dict: None,
            }
        })
        .collect()
}

/// **The build's own artefacts are never taken**, on any axis. Rewriting a file
/// `MANIFEST.json` digests means writing a new prefix — compaction under another name — and the
/// base locator's ordinals are positions in the build's runs, so consuming one renumbers the
/// whole reverse direction for every entity the build knew about.
///
/// The base run is excluded because no locator extent names it; the base dictionary because
/// the dictionary kind skips its first entry.
#[test]
fn the_builds_own_run_and_dictionary_extent_are_never_selected() {
    let (manifest, build_files) = manifest_with(3);
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert!(
        !plan
            .runs
            .contains(&"entities/external-ids-0.arrow".to_string()),
        "the build's run: {:?}",
        plan.runs
    );
    assert!(
        !plan.dicts.iter().any(|e| e.path == "terms/terms-0.dict"),
        "the build's dictionary extent: {:?}",
        plan.dicts
    );
}

/// **A fold's carry-forward must not freeze the dictionary axis.** A fold digest-names every
/// carried file in the new prefix's `MANIFEST.json` (durability for the hard links,
/// compaction §4) and carries `dict_extents` forward verbatim — the one guarded axis it does
/// not rebuild. Judging eligibility by that digest home froze every carried extent, so the
/// axis ratcheted linearly in the fold count — the endurance tier measured 6 → 58 across 24
/// fold cycles, against write-path §7's claim that the coalesce bounds it. Eligibility is
/// positional instead: the base dictionary — always first — is never taken, and every later
/// extent stays takeable whichever files map digests it.
///
/// **Mutation:** restore the `is_build` test on the dictionary axis and this plans no
/// dictionary window; admit the first entry and the window starts at the base.
#[test]
fn a_folds_carried_dictionary_extents_are_still_selected() {
    let (mut manifest, mut build_files) = manifest_with(3);
    // A fold's publication: every carried file's digest moves to the new prefix's
    // `MANIFEST.json` and the side-manifest's own files map starts empty — every digest a
    // fold publishes goes in `MANIFEST.json` (compaction §4).
    build_files.extend(std::mem::take(&mut manifest.files));

    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let dicts: Vec<&str> = plan.dicts.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        dicts,
        [
            format!("partitions/{PARTITION}/views/s0/segments/flush-0-1/terms-0.dict"),
            format!("partitions/{PARTITION}/views/s0/segments/flush-1-1/terms-0.dict"),
            format!("partitions/{PARTITION}/views/s0/segments/flush-2-1/terms-0.dict"),
        ],
        "the carried extents coalesce, and the base dictionary is not among them"
    );
}

/// After a fold, the tiers and runs it carried forward are digested in the new prefix's
/// `MANIFEST.json`. They are still taken, and the base run, which no locator extent names, is not.
#[test]
fn a_folds_carried_tiers_and_runs_are_still_selected() {
    let (mut manifest, mut build_files) = manifest_with(3);
    build_files.extend(std::mem::take(&mut manifest.files));

    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.tiers, manifest.deltas);
    assert_eq!(plan.runs, manifest.external_id_runs[1..]);
    assert!(!plan.runs.contains(&"entities/external-ids-0.arrow".to_string()));
}

/// An entry naming a file neither manifest digests is not taken, on any kind.
#[test]
fn an_entry_with_an_undigested_file_is_not_selected() {
    let (mut manifest, build_files) = manifest_with(3);
    let seg = format!("partitions/{PARTITION}/views/s0/segments/flush-1-1");
    for name in ["delta.arrow", "ext-locator.u32", "terms-0.dict"] {
        manifest.files.remove(&format!("{seg}/{name}"));
    }
    let title = manifest
        .attr_extents
        .iter()
        .find(|e| e.column == "title" && e.values.contains("flush-1-1"))
        .expect("the fixture's title extent")
        .presence
        .clone();
    manifest.files.remove(&title);

    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert!(plan.tiers.is_empty());
    assert!(plan.runs.is_empty());
    assert!(plan.dicts.is_empty());
    let columns: Vec<&str> = plan.attrs.iter().map(|w| w.column.as_str()).collect();
    assert_eq!(columns, ["department"]);
}

/// Locator extents whose spans overlap are not one span. `external_id_of_checked` finds an
/// extent by the first span containing the entity, so a coalesced extent overlapping another
/// would answer one entity's ordinal against another run's keys.
#[test]
fn overlapping_locator_spans_are_refused_on_the_run_axis() {
    let (mut manifest, build_files) = manifest_with(3);
    manifest.locator_extents[1].entity_lo = 0; // now overlaps extent 0
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert!(
        plan.runs.is_empty(),
        "the run axis must not select across overlapping spans: {:?}",
        plan.runs
    );
    assert!(
        !plan.tiers.is_empty(),
        "the tier axis is independent and still qualifies"
    );
}

/// Size tiering is what bounds write amplification: without it the pass re-reads the artefact
/// it produced last round, for ever. A window straddling two size classes is not selected.
#[test]
fn a_window_spanning_two_size_classes_is_not_selected() {
    let (mut manifest, build_files) = manifest_with(3);
    let big = manifest.deltas[1].clone();
    manifest.files.insert(big, digest(64 << 20));
    let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live);
    assert!(
        plan.as_ref().is_none_or(|p| p.tiers.is_empty()),
        "a 64 MiB tier and two 1 KiB ones are not one class"
    );
}

/// Below the width nothing is selected — the ordinary answer at all but one tick in `width`.
#[test]
fn nothing_is_selected_below_the_width() {
    let (manifest, build_files) = manifest_with(2);
    assert!(plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).is_none());
}

/// **The coalesced entry lands where the window was, never at the end.** On the run axis that
/// is recency, which decision 0047's newest-binding-first resolution reads off list order; on
/// the dictionary axis it is every ordinal after the window.
///
/// **Mutation:** push instead of splice and the coalesced run becomes the newest, so a key it
/// carries an old binding for outranks the flush that re-bound it.
#[test]
fn the_coalesced_entry_takes_the_windows_position() {
    let (mut manifest, build_files) = manifest_with(4);
    let mut policy = policy();
    policy.width = 3;
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");

    let dir = tempfile::TempDir::new().unwrap();
    let attrs = completed_attrs(&plan, "c");
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: Some((
            "c/external-ids.arrow".to_string(),
            LocatorExtent {
                path: "c/ext-locator.u32".to_string(),
                entity_lo: plan.locators[0].entity_lo,
                entity_hi: plan.locators[plan.locators.len() - 1].entity_hi,
                external_id_run: "c/external-ids.arrow".to_string(),
            },
        )),
        dict: Some(DictExtent {
            path: "c/terms-0.dict".to_string(),
            records: 3,
        }),
        attrs,
        record: None,
        texts: Vec::new(),
        terms: None,
        files: [("c/delta.arrow".to_string(), digest(3072))]
            .into_iter()
            .collect(),
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(rebase_into(&mut manifest, &completed));

    assert_eq!(manifest.deltas.len(), 2, "3 tiers became 1, 1 untouched");
    assert_eq!(manifest.deltas[0], "c/delta.arrow");
    assert_eq!(
        manifest.external_id_runs[0], "entities/external-ids-0.arrow",
        "the build's run stays listed first — the base locator's ordinals resolve inside it"
    );
    assert_eq!(manifest.external_id_runs[1], "c/external-ids.arrow");
    assert_eq!(
        manifest.dict_extents[0].path, "terms/terms-0.dict",
        "the build's dictionary extent keeps ordinal 0"
    );
    assert_eq!(manifest.dict_extents[1].path, "c/terms-0.dict");
    assert!(
        !manifest
            .files
            .keys()
            .any(|k| k.contains("flush-0-1/delta.arrow")),
        "a consumed file leaves the files map"
    );
}

/// **The attribute axis selects per column, over that column's own subsequence.**
///
/// The selection unit is the column because that is the identity the format carries — an
/// `AttrExtent` records no flush, and filter-index §2.5 forbids recovering one from the path.
///
/// **Mutation:** select over `attr_extents` as one list and each window holds both columns'
/// extents, which the merge then refuses as interleaved — after the pass has done its IO.
#[test]
fn the_attribute_axis_selects_a_window_of_each_columns_own_extents() {
    let (manifest, build_files) = manifest_with(4);
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.attrs.len(), 2, "one window per column");
    for window in &plan.attrs {
        assert_eq!(window.extents.len(), 3, "the policy's width, per column");
        assert!(
            window.extents.iter().all(|e| e.column == window.column),
            "a window took another column's extent: {:?}",
            window.extents
        );
    }
}

/// **The input cap applies per column, and narrows the window rather than stalling the axis.**
///
/// The cap bounds the pass transient — the window's values and presence held during the
/// merge — so a text column whose values outgrow it must stall *itself* and never its
/// neighbours; and where it can still take a narrower window it takes one, because reverting to
/// unbounded file growth is the failure this axis exists to prevent.
#[test]
fn a_column_over_the_input_cap_narrows_its_window_and_stalls_only_itself() {
    let (mut manifest, build_files) = manifest_with(4);
    let mut policy = policy();
    policy.max_input_bytes = 5 << 20;
    // `title`'s extents are 2 MiB each: three exceed the cap, two do not. Same size tier
    // throughout, so it is the cap doing the narrowing and not the ladder.
    for extent in manifest.attr_extents.iter().filter(|e| e.column == "title") {
        manifest
            .files
            .insert(extent.values.clone(), digest(2 << 20));
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");
    let window = |column: &str| {
        plan.attrs
            .iter()
            .find(|w| w.column == column)
            .map(|w| w.extents.len())
    };
    assert_eq!(window("title"), Some(2), "narrowed to what fits the cap");
    assert_eq!(window("department"), Some(3), "the neighbour is unaffected");

    // And a column one extent of which alone exceeds the cap is genuinely uncoalesceable: it
    // waits for the fold rather than being coalesced over the bound it was given.
    policy.max_input_bytes = 1 << 20;
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");
    assert!(
        !plan.attrs.iter().any(|w| w.column == "title"),
        "a column whose single extent exceeds the cap must not be selected"
    );
}

/// **A layer's dictionary counts toward the input cap.**
///
/// The cap bounds the pass transient, and for a column whose values are ordinals the dictionary
/// is the half that grows with distinct values rather than with entities — on a near-unique
/// column, the larger half (records §7). Sizing the window on values and presence alone would
/// bound the cheap term and let the expensive one through.
///
/// Stated against [`select_window`] directly, with the two size functions side by side, and
/// then against [`plan_coalesce`], whose per-column narrowing must reach the same answer.
#[test]
fn a_layers_dictionary_counts_toward_the_input_cap() {
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    let extents: Vec<AttrExtent> = (0..3)
        .map(|i| {
            let mut extent = attr_extent_at(PARTITION, "submitter", &format!("flush-{i}-1"));
            extent.dict = Some(format!(
                "partitions/{PARTITION}/attrs/submitter/extents/flush-{i}-1.dict"
            ));
            sizes.insert(extent.values.clone(), 1 << 20);
            sizes.insert(extent.presence.clone(), 0);
            sizes.insert(extent.dict.clone().expect("a dictionary"), 1 << 20);
            extent
        })
        .collect();
    let policy = CoalescePolicy {
        width: 3,
        floor_bytes: 1 << 20,
        max_input_bytes: 4 << 20,
    };
    let counted = |extent: &AttrExtent| {
        Some(
            sizes[&extent.values]
                + sizes[&extent.presence]
                + extent.dict.as_ref().map_or(0, |d| sizes[d]),
        )
    };
    let values_only =
        |extent: &AttrExtent| Some(sizes[&extent.values] + sizes[&extent.presence]);
    assert_eq!(
        select_window(&extents, policy.width, policy, values_only),
        Some(0..3),
        "three 1 MiB values files fit a 4 MiB cap on their own"
    );
    assert_eq!(
        select_window(&extents, policy.width, policy, counted),
        None,
        "counting the dictionaries, the same window is 6 MiB and must not be selected"
    );

    // Through the planner: the same three extents, digested in the manifest, narrow to the
    // two that fit the cap with their dictionaries counted.
    let (mut manifest, build_files) = manifest_with(0);
    for extent in &extents {
        manifest
            .files
            .insert(extent.values.clone(), digest(1 << 20));
        manifest.files.insert(extent.presence.clone(), digest(0));
        manifest
            .files
            .insert(extent.dict.clone().expect("a dictionary"), digest(1 << 20));
        manifest.attr_extents.push(extent.clone());
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");
    let window = plan
        .attrs
        .iter()
        .find(|w| w.column == "submitter")
        .expect("the keyword column is selected");
    assert_eq!(
        window.extents.len(),
        2,
        "narrowed to the two extents whose values and dictionaries fit the cap"
    );
}

/// **A column whose layers carry their own dictionaries is selected on the same policy as every
/// other**, its window naming every extent's dictionary beside its values. The merge for it
/// renumbers, and the containment is the manifest record's and the composition's (module doc);
/// nothing at selection needs to know the family beyond counting the dictionary toward the cap.
///
/// **Mutation this kills:** restore a `dict.is_some()` skip at the selection and `title` is
/// never selected, so an indexed keyword column gains one extent per flush until the fold.
#[test]
fn a_column_with_per_layer_dictionaries_is_selected_like_any_other() {
    let (mut manifest, build_files) = manifest_with(4);
    for extent in manifest
        .attr_extents
        .iter_mut()
        .filter(|e| e.column == "title")
    {
        let dict = format!("{}.dict", extent.values);
        manifest.files.insert(dict.clone(), digest(64));
        extent.dict = Some(dict);
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let window = plan
        .attrs
        .iter()
        .find(|w| w.column == "title")
        .expect("the keyword column's window is planned");
    assert_eq!(window.extents.len(), 3, "the policy's width");
    assert!(
        window.extents.iter().all(|e| e.dict.is_some()),
        "every extent of the window names the dictionary its ordinals are read against"
    );
    assert!(
        plan.attrs.iter().any(|w| w.column == "department"),
        "the neighbour is selected as before"
    );
}

/// **A coalesced keyword extent replaces its window in both halves of the manifest, dictionaries
/// included** — and a flush of the same column landing between the plan and the rebase leaves
/// the window where it was, with the flush's extent and its own dictionary untouched.
///
/// The consumed dictionaries leave `files` with the values and presence they numbered: a
/// dictionary left digested for a layer no list names is a file the fold's sweep reclaims and
/// the manifest meanwhile misdescribes. The coalesced entry names its merged dictionary, and
/// that file is digested — a keyword entry without one is a layer the reader refuses at open.
///
/// **Mutation:** drop `e.dict` from the `attr_paths` chain and the consumed dictionaries stay
/// digested; drop `dict` from the coalesced entry and `FilterColumns::open` refuses the bundle.
#[test]
fn a_coalesced_keyword_extent_replaces_its_window_and_its_dictionaries_in_both_halves() {
    let (mut manifest, build_files) = manifest_with(4);
    for extent in manifest
        .attr_extents
        .iter_mut()
        .filter(|e| e.column == "title")
    {
        let dict = format!("{}.dict", extent.values);
        manifest.files.insert(dict.clone(), digest(64));
        extent.dict = Some(dict);
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let title = plan
        .attrs
        .iter()
        .find(|w| w.column == "title")
        .expect("the keyword window");
    let consumed: Vec<String> = title
        .extents
        .iter()
        .flat_map(|e| {
            [e.values.clone(), e.presence.clone()]
                .into_iter()
                .chain(e.dict.clone())
        })
        .collect();
    assert_eq!(consumed.len(), 9, "three files per consumed keyword extent");

    // The flush that landed while the pass ran: a fifth `title` extent, with its own
    // dictionary, appended after the window.
    let late = {
        let mut extent = attr_extent_at(PARTITION, "title", "flush-9-1");
        extent.dict = Some(format!("{}.dict", extent.values));
        extent
    };
    manifest.files.insert(late.values.clone(), digest(1024));
    manifest.files.insert(late.presence.clone(), digest(64));
    manifest
        .files
        .insert(late.dict.clone().expect("a dictionary"), digest(64));
    manifest.attr_extents.push(late.clone());

    let out_rel = "partitions/p0/coalesced/coalesce-1-1";
    let mut attrs = completed_attrs(&plan, out_rel);
    let merged_dict_rel = format!("{out_rel}/attrs/title/dict.bin");
    for attr in attrs.iter_mut().filter(|a| a.extent.column == "title") {
        attr.extent.dict = Some(merged_dict_rel.clone());
    }
    let files: BTreeMap<String, FileDigest> = attrs
        .iter()
        .flat_map(|a| {
            [
                (a.extent.values.clone(), digest(3072)),
                (a.extent.presence.clone(), digest(96)),
            ]
            .into_iter()
            .chain(a.extent.dict.clone().map(|d| (d, digest(192))))
        })
        .collect();
    let dir = tempfile::TempDir::new().unwrap();
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: None,
        dict: None,
        attrs,
        record: None,
        texts: Vec::new(),
        terms: None,
        files,
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(
        rebase_into(&mut manifest, &completed),
        "a flush appending the same column's extent does not move the window"
    );

    let listed: Vec<&AttrExtent> = manifest
        .attr_extents
        .iter()
        .filter(|e| e.column == "title")
        .collect();
    assert_eq!(
        listed.len(),
        3,
        "3 became 1, 1 untouched, and the late flush's: {listed:?}"
    );
    assert_eq!(
        listed[0].dict.as_deref(),
        Some(merged_dict_rel.as_str()),
        "the coalesced entry names the merged dictionary beside its values"
    );
    assert!(
        manifest.files.contains_key(&merged_dict_rel),
        "the merged dictionary is digested"
    );
    assert_eq!(
        listed[2].dict, late.dict,
        "the late flush's extent keeps its own dictionary"
    );
    assert!(manifest.files.contains_key(late.dict.as_deref().unwrap()));
    for rel in &consumed {
        assert!(
            !manifest.files.contains_key(rel),
            "a consumed extent file is still digested: {rel}"
        );
    }
}

/// One flush's keyword extent of `title`, written with the flush's own writer into `prefix_dir`
/// and listed in `manifest` with its three files digested. `keys` is one key per entity, in
/// `entities`' order; the extent's dictionary is the sorted distinct set of them, so each
/// extent numbers its keys its own way.
fn write_keyword_flush(
    prefix_dir: &std::path::Path,
    manifest: &mut SegmentsManifest,
    flush: &str,
    entities: &[u32],
    keys: &[&str],
) -> AttrExtent {
    let mut sorted: Vec<&str> = keys.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let codes: Vec<u32> = keys
        .iter()
        .map(|k| sorted.binary_search(k).expect("from these") as u32)
        .collect();
    let mut presence = croaring::Bitmap::new();
    for e in entities {
        presence.add(*e);
    }
    let column_dir = prefix_dir.join(format!("partitions/{PARTITION}/attrs/title"));
    let (values, presence_path, dict) = tessera_filter::write_extent(
        &column_dir,
        flush,
        &tessera_filter::Codes::U32(codes.into()),
        &presence,
        Some(&sorted),
    )
    .expect("the flush's writer writes a keyword extent");
    let rel = |path: &std::path::Path| {
        path.strip_prefix(prefix_dir)
            .expect("under the prefix")
            .to_str()
            .expect("utf-8")
            .to_string()
    };
    let extent = AttrExtent {
        incarnation: None,
        column: "title".to_string(),
        view: None,
        values: rel(&values),
        presence: rel(&presence_path),
        dict: Some(rel(&dict.expect("a keyword extent names its dictionary"))),
        postings: None,
        offsets: None,
    };
    for path in [&extent.values, &extent.presence]
        .into_iter()
        .chain(extent.dict.as_ref())
    {
        manifest.files.insert(
            path.clone(),
            tessera_store::digest_of(&prefix_dir.join(path)).unwrap(),
        );
    }
    manifest.attr_extents.push(extent.clone());
    extent
}

/// **A keyword window executes into one extent whose dictionary numbers its ordinals, and the
/// completed pass carries the pair the manifest entry names** — run over real files with the
/// flush's own writer and the merge the pass runs, rather than the stubbed reader the manifest
/// tests use.
///
/// The three inputs number their keys three different ways (`alpha` is 0 in the first and
/// absent from the others; `gamma` is 1 in the first, 0 in the second, 1 in the third), and the
/// merged dictionary numbers all five keys a fourth way. Every entity then reads its own key
/// through the coalesced pair — through the opened readers the executor installs, and again
/// through the files the rebased manifest names, which is what a restart opens.
///
/// **Mutation this kills:** leave `dict` off the `OpenedExtent` or the `AttrExtent` and the
/// entry names ordinals with nothing to read them against; run the byte-preserving merge on
/// the window and entity 30 reads `alpha` where it carried `gamma`.
#[test]
fn a_keyword_window_executes_into_one_extent_whose_dictionary_numbers_its_ordinals() {
    let dir = tempfile::TempDir::new().unwrap();
    let prefix_dir = dir.path().join("v00000");
    let (mut manifest, build_files) = manifest_with(0);
    let flushes: [(&[u32], &[&str]); 3] = [
        (&[10, 11], &["gamma", "alpha"]),
        (&[20, 21], &["gamma", "delta"]),
        (&[30, 31, 32], &["gamma", "beta", "epsilon"]),
    ];
    let mut expected: BTreeMap<u32, &str> = BTreeMap::new();
    for (i, (entities, keys)) in flushes.iter().enumerate() {
        write_keyword_flush(
            &prefix_dir,
            &mut manifest,
            &format!("flush-{i}-1"),
            entities,
            keys,
        );
        expected.extend(entities.iter().copied().zip(keys.iter().copied()));
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.attrs.len(), 1, "the one keyword window");
    let out_rel = format!("partitions/{PARTITION}/coalesced/coalesce-1-1");
    let completed = execute_coalesce(
        plan,
        CoalesceContext {
            prefix_dir: prefix_dir.clone(),
            prefix: "v00000".to_string(),
            out_rel: out_rel.clone(),
        },
    )
    .expect("the keyword window merges");

    let attr = &completed.attrs[0];
    let dict_rel = attr
        .extent
        .dict
        .as_deref()
        .expect("the coalesced entry names the merged dictionary");
    assert!(dict_rel.starts_with(&out_rel));
    assert!(
        completed.files.contains_key(dict_rel),
        "the merged dictionary is digested with the values it numbers"
    );
    let dict = attr
        .dict
        .as_ref()
        .expect("the completed pass carries the dictionary opened, beside the values");
    assert_eq!(dict.len(), 5, "alpha, beta, delta, epsilon, gamma");
    let mut scratch = Vec::new();
    for (entity, key) in &expected {
        let ordinal = attr
            .values
            .value_of(*entity)
            .expect("every consumed entity is present")
            .raw();
        assert_eq!(
            dict.key_of(ordinal, &mut scratch).expect("in range"),
            *key,
            "entity {entity} reads another key through the merged pair"
        );
    }

    // The manifest edit, and the files it names reopened from disc as a restart would.
    assert!(rebase_into(&mut manifest, &completed));
    let listed: Vec<&AttrExtent> = manifest
        .attr_extents
        .iter()
        .filter(|e| e.column == "title")
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].dict.as_deref(), Some(dict_rel));
    let values = tessera_filter::open_extent(
        &prefix_dir.join(&listed[0].values),
        &prefix_dir.join(&listed[0].presence),
        tessera_filter::Access::Read,
    )
    .expect("the listed values open");
    let dict = tessera_filter::SortedDict::open(
        &prefix_dir.join(dict_rel),
        tessera_filter::Access::Read,
    )
    .expect("the listed dictionary opens");
    for (entity, key) in &expected {
        let ordinal = values.value_of(*entity).expect("present").raw();
        assert_eq!(dict.key_of(ordinal, &mut scratch).expect("in range"), *key);
    }
}

/// **A keyword window the merge refuses installs nothing.** The merge's guards run before any
/// ordinal is written, and the executor's only commit point is the manifest edit, so a refusal
/// leaves the consumed entries standing, their files digested, and the output directory as an
/// orphan the fold reclaims.
///
/// The fault here is an input the merge cannot read consistently: an extent whose ordinals
/// reach past its own dictionary. A wrong *remap* — the merge's own defect — is refused by the
/// same guard family at the merge (`tessera_filter_write::keyword`'s tests inject one), and
/// the pass treats every refusal alike: `Err`, and nothing published.
#[test]
fn a_keyword_window_the_merge_refuses_installs_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let prefix_dir = dir.path().join("v00000");
    let (mut manifest, build_files) = manifest_with(0);
    write_keyword_flush(
        &prefix_dir,
        &mut manifest,
        "flush-0-1",
        &[10, 11],
        &["b", "a"],
    );
    let faulted = write_keyword_flush(
        &prefix_dir,
        &mut manifest,
        "flush-1-1",
        &[20, 21],
        &["d", "c"],
    );
    write_keyword_flush(&prefix_dir, &mut manifest, "flush-2-1", &[30], &["e"]);
    // The second extent's dictionary replaced by one of a single key, so its ordinal 1 names
    // nothing. The manifest still digests the original bytes; the pass reads the file.
    let mut dictionary = Vec::new();
    let mut writer =
        tessera_filter::SortedDictWriter::new(&mut dictionary).expect("a writer opens");
    writer.push("c").unwrap();
    writer.finish().unwrap();
    std::fs::write(
        prefix_dir.join(faulted.dict.as_deref().unwrap()),
        dictionary,
    )
    .unwrap();
    let before = serde_json::to_string(&manifest).unwrap();

    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let out_rel = format!("partitions/{PARTITION}/coalesced/coalesce-1-1");
    let err = match execute_coalesce(
        plan,
        CoalesceContext {
            prefix_dir: prefix_dir.clone(),
            prefix: "v00000".to_string(),
            out_rel: out_rel.clone(),
        },
    ) {
        Ok(_) => panic!("an ordinal past its dictionary must be refused"),
        Err(e) => e,
    };
    assert!(
        err.0.contains("keyword coalesce for 'title'"),
        "the refusal names the merge and the column: {err:?}"
    );
    assert_eq!(
        serde_json::to_string(&manifest).unwrap(),
        before,
        "the pass has no commit point before the manifest edit, and never reached it"
    );
    assert!(
        !prefix_dir
            .join(&out_rel)
            .join("attrs/title")
            .join(tessera_filter::VALUES_FILE)
            .exists(),
        "no ordinal was written under the merged dictionary"
    );
    for extent in &manifest.attr_extents {
        assert!(prefix_dir.join(&extent.values).exists());
        assert!(prefix_dir.join(extent.dict.as_deref().unwrap()).exists());
    }
}

/// **Both obligations land in one manifest edit: the files and the `attr_extents` entries.**
///
/// Doing one without the other yields a bundle that opens cleanly and answers filters missing
/// every entity the consumed window held — a wrong answer with no symptom, and strictly worse
/// than a refusal to open (filter-index §6.2).
///
/// **Mutation:** drop the `attr_paths` chain from the `files` removal and the consumed digests
/// stand; drop the `attr_extents` rebuild and the manifest names the coalesced bytes nowhere.
#[test]
fn a_coalesced_attr_extent_replaces_its_window_in_both_halves_of_the_manifest() {
    let (mut manifest, build_files) = manifest_with(4);
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let consumed: Vec<String> = plan
        .attrs
        .iter()
        .flat_map(|w| w.extents.iter())
        .flat_map(|e| [e.values.clone(), e.presence.clone()])
        .collect();
    let out_rel = "partitions/p0/coalesced/coalesce-1-1";
    let attrs = completed_attrs(&plan, out_rel);
    let files: BTreeMap<String, FileDigest> = attrs
        .iter()
        .flat_map(|a| {
            [
                (a.extent.values.clone(), digest(3072)),
                (a.extent.presence.clone(), digest(96)),
            ]
        })
        .collect();
    let dir = tempfile::TempDir::new().unwrap();
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: None,
        dict: None,
        attrs,
        record: None,
        texts: Vec::new(),
        terms: None,
        files,
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(rebase_into(&mut manifest, &completed));

    for column in COLUMNS {
        let listed: Vec<&AttrExtent> = manifest
            .attr_extents
            .iter()
            .filter(|e| e.column == column)
            .collect();
        assert_eq!(
            listed.len(),
            2,
            "3 extents became 1, 1 untouched: {listed:?}"
        );
        assert_eq!(
            listed[0].values,
            format!("{out_rel}/attrs/{column}/values.arrow"),
            "the coalesced extent takes the window's position in its column's subsequence"
        );
    }
    for rel in &consumed {
        assert!(
            !manifest.files.contains_key(rel),
            "a consumed extent file is still digested: {rel}"
        );
    }
    for attr in &completed.attrs {
        for rel in [&attr.extent.values, &attr.extent.presence] {
            assert!(
                manifest.files.contains_key(rel),
                "the coalesced extent's bytes are named in `attr_extents` but not digested: \
                 {rel}"
            );
        }
    }
}

/// A window a flush has since moved out from under no longer rebases — and a flush that
/// *appends* another column's extent mid-list does not disturb it, because the contiguity that
/// matters is contiguity in the column's own subsequence.
#[test]
fn an_attr_window_rebases_through_another_columns_flush_but_not_through_its_own() {
    let (mut manifest, build_files) = manifest_with(4);
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let dir = tempfile::TempDir::new().unwrap();
    let attrs = completed_attrs(&plan, "c");
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: None,
        dict: None,
        attrs,
        record: None,
        texts: Vec::new(),
        terms: None,
        files: BTreeMap::new(),
        plan,
        prefix: "v00000".to_string(),
    };

    // Another column's extent, inserted between two of `title`'s — which is precisely what a
    // flush publishing both columns produces, and must not discard the pass.
    let mut interleaved = manifest.clone();
    interleaved
        .attr_extents
        .insert(1, attr_extent_at(PARTITION, "elsewhere", "flush-9-1"));
    assert!(rebase_into(&mut interleaved, &completed));

    // Its own extent gone, however, is the state the plan was made against being gone.
    let consumed = completed.plan.attrs[0].extents[1].values.clone();
    manifest.attr_extents.retain(|e| e.values != consumed);
    assert!(!rebase_into(&mut manifest, &completed));
}

/// **A group-scoped column's window rebases within its own view's subsequence**, which is the
/// `(column, view, incarnation)` the planner grouped it by. Two views of one family interleave
/// their extents under one column name, so a subsequence taken on the name alone holds neither
/// window contiguously and every finished coalesce of a scoped family is discarded.
#[test]
fn a_scoped_columns_window_rebases_within_its_own_views_extents() {
    let (mut manifest, build_files) = manifest_with(0);
    let views = ["quarter:2026-Q1", "quarter:2026-Q3"];
    for i in 0..4 {
        for view in views {
            let extent = scoped_extent_at(PARTITION, "mood", view, &format!("flush-{i}-1"));
            manifest.files.insert(extent.values.clone(), digest(1024));
            manifest.files.insert(extent.presence.clone(), digest(64));
            manifest.attr_extents.push(extent);
        }
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.attrs.len(), 2, "one window per view");
    let consumed: Vec<String> = plan
        .attrs
        .iter()
        .flat_map(|w| w.extents.iter())
        .flat_map(|e| [e.values.clone(), e.presence.clone()])
        .collect();
    let untouched: Vec<String> = manifest
        .attr_extents
        .iter()
        .filter(|e| !consumed.contains(&e.values))
        .map(|e| e.values.clone())
        .collect();

    let out_rel = "partitions/p0/coalesced/coalesce-1-1";
    let attrs = completed_attrs(&plan, out_rel);
    let files: BTreeMap<String, FileDigest> = attrs
        .iter()
        .flat_map(|a| {
            [
                (a.extent.values.clone(), digest(3072)),
                (a.extent.presence.clone(), digest(96)),
            ]
        })
        .collect();
    let coalesced: Vec<String> = attrs.iter().map(|a| a.extent.values.clone()).collect();
    let completed = CompletedCoalesce {
        tier: None,
        run: None,
        dict: None,
        attrs,
        record: None,
        texts: Vec::new(),
        terms: None,
        files,
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(rebase_into(&mut manifest, &completed));

    let listed: Vec<&str> = manifest
        .attr_extents
        .iter()
        .map(|e| e.values.as_str())
        .collect();
    let expected: Vec<&str> = coalesced
        .iter()
        .chain(&untouched)
        .map(String::as_str)
        .collect();
    assert_eq!(
        listed, expected,
        "each view's coalesced extent lands where that view's window began, once, and the \
         later flush's extents keep their order"
    );
    for rel in &consumed {
        assert!(
            !manifest.files.contains_key(rel),
            "a consumed extent file is still digested: {rel}"
        );
    }
    for extent in &manifest.attr_extents {
        assert!(
            manifest.files.contains_key(&extent.values),
            "a listed extent's bytes are not digested: {}",
            extent.values
        );
    }
}

/// **A group-scoped text column's window rebases within its own view's subsequence** — the
/// attribute axis's rule over `text_extents`, for its reason.
#[test]
fn a_scoped_text_columns_window_rebases_within_its_own_views_extents() {
    let (mut manifest, build_files) = manifest_with(0);
    let views = ["quarter:2026-Q1", "quarter:2026-Q3"];
    for i in 0..4 {
        for view in views {
            let (group, key) = view.split_once(':').expect("a view of a group");
            let dir = format!("partitions/{PARTITION}/text/notes/{group}/{key}/extents");
            let extent = TextExtent {
                column: "notes".to_string(),
                view: Some(view.to_string()),
                incarnation: Some(tessera_store::manifest::DECLARED_INCARNATION),
                dict: format!("{dir}/flush-{i}-1.dict"),
                postings: format!("{dir}/flush-{i}-1.postings"),
                presence: format!("{dir}/flush-{i}-1.roaring"),
            };
            manifest.files.insert(extent.dict.clone(), digest(1024));
            manifest.files.insert(extent.postings.clone(), digest(1024));
            manifest.files.insert(extent.presence.clone(), digest(64));
            manifest.text_extents.push(extent);
        }
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.texts.len(), 2, "one window per view");
    let consumed: Vec<String> = plan
        .texts
        .iter()
        .flat_map(|w| w.extents.iter())
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let untouched: Vec<String> = manifest
        .text_extents
        .iter()
        .filter(|e| !consumed.contains(&e.dict))
        .map(|e| e.dict.clone())
        .collect();

    let out_rel = "partitions/p0/coalesced/coalesce-1-1";
    let texts: Vec<TextExtent> = plan
        .texts
        .iter()
        .map(|window| {
            let column_rel =
                coalesced_column_rel(out_rel, &window.column, window.view.as_deref());
            TextExtent {
                column: window.column.clone(),
                view: window.view.clone(),
                incarnation: window.incarnation,
                dict: format!("{column_rel}/text.dict"),
                postings: format!("{column_rel}/text.postings"),
                presence: format!("{column_rel}/text.roaring"),
            }
        })
        .collect();
    let files: BTreeMap<String, FileDigest> = texts
        .iter()
        .flat_map(|e| {
            [
                (e.dict.clone(), digest(3072)),
                (e.postings.clone(), digest(3072)),
                (e.presence.clone(), digest(96)),
            ]
        })
        .collect();
    let coalesced: Vec<String> = texts.iter().map(|e| e.dict.clone()).collect();
    let completed = CompletedCoalesce {
        tier: None,
        run: None,
        dict: None,
        attrs: Vec::new(),
        record: None,
        texts,
        terms: None,
        files,
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(rebase_into(&mut manifest, &completed));

    let listed: Vec<&str> = manifest
        .text_extents
        .iter()
        .map(|e| e.dict.as_str())
        .collect();
    let expected: Vec<&str> = coalesced
        .iter()
        .chain(&untouched)
        .map(String::as_str)
        .collect();
    assert_eq!(
        listed, expected,
        "each view's coalesced extent lands where that view's window began, once, and the \
         later flush's extents keep their order"
    );
    for rel in &consumed {
        assert!(
            !manifest.files.contains_key(rel),
            "a consumed extent file is still digested: {rel}"
        );
    }
    for extent in &manifest.text_extents {
        assert!(
            manifest.files.contains_key(&extent.dict),
            "a listed extent's bytes are not digested: {}",
            extent.dict
        );
    }
}

/// **The record axis selects a window of `record_extents` and replaces it in place, in both
/// halves of the manifest** — the entry list and the files map. The same silent-failure shape
/// as the attribute axis: a bundle that lost the window's entry while keeping its bytes (or
/// the reverse) opens cleanly and answers drill-downs short, with no symptom.
#[test]
fn the_record_axis_selects_a_window_and_replaces_it_in_both_manifest_halves() {
    let (mut manifest, build_files) = manifest_with(4);
    for i in 0..4 {
        let dir = "partitions/p0/attrs/record/extents";
        let extent = RecordExtent {
            blocks: format!("{dir}/flush-{i}-1.blocks.bin"),
            hasrow: format!("{dir}/flush-{i}-1.hasrow.roaring"),
            directory: format!("{dir}/flush-{i}-1.directory.arrow"),
        };
        manifest.files.insert(extent.blocks.clone(), digest(1024));
        manifest.files.insert(extent.hasrow.clone(), digest(64));
        manifest.files.insert(extent.directory.clone(), digest(128));
        manifest.record_extents.push(extent);
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.records.len(), 3, "the policy's width");
    let consumed: Vec<String> = plan
        .records
        .iter()
        .flat_map(|e| e.files().map(String::from))
        .collect();

    let coalesced = RecordExtent {
        blocks: "c/attrs/record/blocks.bin".to_string(),
        hasrow: "c/attrs/record/hasrow.roaring".to_string(),
        directory: "c/attrs/record/directory.arrow".to_string(),
    };
    let dir = tempfile::TempDir::new().unwrap();
    let attrs = completed_attrs(&plan, "c");
    let files: BTreeMap<String, FileDigest> = [
        (coalesced.blocks.clone(), digest(3072)),
        (coalesced.hasrow.clone(), digest(96)),
        (coalesced.directory.clone(), digest(256)),
    ]
    .into_iter()
    .collect();
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: None,
        dict: None,
        attrs,
        record: Some(coalesced.clone()),
        texts: Vec::new(),
        terms: None,
        files,
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(rebase_into(&mut manifest, &completed));

    assert_eq!(
        manifest.record_extents.len(),
        2,
        "3 extents became 1, 1 untouched: {:?}",
        manifest.record_extents
    );
    assert_eq!(
        manifest.record_extents[0].blocks, coalesced.blocks,
        "the coalesced extent takes the window's position"
    );
    for rel in &consumed {
        assert!(
            !manifest.files.contains_key(rel),
            "a consumed extent file is still digested: {rel}"
        );
    }
    for rel in [&coalesced.blocks, &coalesced.hasrow, &coalesced.directory] {
        assert!(
            manifest.files.contains_key(rel),
            "the coalesced extent's bytes are named in `record_extents` but not digested: {rel}"
        );
    }

    // And a window a fold (or another pass) has since consumed no longer rebases.
    let gone = completed.plan.records[1].blocks.clone();
    manifest.record_extents.retain(|e| e.blocks != gone);
    assert!(!rebase_into(&mut manifest, &completed));
}

/// **The entity→term axis selects a window of `entity_terms_extents` and replaces it in place,
/// in both halves of the manifest** — the record axis's claim over the record axis's shape.
/// The silent failure it guards is the sharper one of the two: a bundle that lost the window's
/// entry while keeping its bytes answers *unknown* for those entities' labels, which on the
/// write path is the join rule's `409` failing to fire.
#[test]
fn the_entity_terms_axis_selects_a_window_and_replaces_it_in_both_manifest_halves() {
    let (mut manifest, build_files) = manifest_with(4);
    for i in 0..4 {
        let dir = "partitions/p0/entities/terms/extents";
        let extent = EntityTermsExtent {
            hasrow: format!("{dir}/flush-{i}-1.hasrow.roaring"),
            offsets: format!("{dir}/flush-{i}-1.offsets.u32"),
            terms: format!("{dir}/flush-{i}-1.terms.u32"),
            bases: format!("{dir}/flush-{i}-1.bases.u64"),
        };
        manifest.files.insert(extent.hasrow.clone(), digest(64));
        manifest.files.insert(extent.offsets.clone(), digest(128));
        manifest.files.insert(extent.terms.clone(), digest(1024));
        manifest.files.insert(extent.bases.clone(), digest(8));
        manifest.entity_terms_extents.push(extent);
    }
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    assert_eq!(plan.terms.len(), 3, "the policy's width");
    let consumed: Vec<String> = plan
        .terms
        .iter()
        .flat_map(|e| {
            [
                e.hasrow.clone(),
                e.offsets.clone(),
                e.terms.clone(),
                e.bases.clone(),
            ]
        })
        .collect();

    let coalesced = EntityTermsExtent {
        hasrow: "c/entities/terms/hasrow.roaring".to_string(),
        offsets: "c/entities/terms/offsets.u32".to_string(),
        terms: "c/entities/terms/terms.u32".to_string(),
        bases: "c/entities/terms/bases.u64".to_string(),
    };
    let dir = tempfile::TempDir::new().unwrap();
    let attrs = completed_attrs(&plan, "c");
    let files: BTreeMap<String, FileDigest> = [
        (coalesced.hasrow.clone(), digest(96)),
        (coalesced.offsets.clone(), digest(384)),
        (coalesced.terms.clone(), digest(3072)),
    ]
    .into_iter()
    .collect();
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: None,
        dict: None,
        attrs,
        record: None,
        texts: Vec::new(),
        terms: Some(coalesced.clone()),
        files,
        plan,
        prefix: "v00000".to_string(),
    };
    assert!(rebase_into(&mut manifest, &completed));

    assert_eq!(
        manifest.entity_terms_extents.len(),
        2,
        "3 extents became 1, 1 untouched: {:?}",
        manifest.entity_terms_extents
    );
    assert_eq!(
        manifest.entity_terms_extents[0].terms, coalesced.terms,
        "the coalesced extent takes the window's position"
    );
    for rel in &consumed {
        assert!(
            !manifest.files.contains_key(rel),
            "a consumed extent file is still digested: {rel}"
        );
    }
    for rel in [&coalesced.hasrow, &coalesced.offsets, &coalesced.terms] {
        assert!(
            manifest.files.contains_key(rel),
            "the coalesced extent's bytes are listed but not digested: {rel}"
        );
    }

    // And a window a fold (or another pass) has since consumed no longer rebases.
    let gone = completed.plan.terms[1].terms.clone();
    manifest.entity_terms_extents.retain(|e| e.terms != gone);
    assert!(!rebase_into(&mut manifest, &completed));
}

/// A plan whose window is gone no longer rebases, and the publication is discarded rather than
/// forced — its files orphans nothing references, every consumed entry still standing.
#[test]
fn a_plan_whose_window_moved_does_not_rebase() {
    let (mut manifest, build_files) = manifest_with(3);
    let plan =
        plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
    let dir = tempfile::TempDir::new().unwrap();
    let attrs = completed_attrs(&plan, "c");
    let completed = CompletedCoalesce {
        tier: Some(tier_at(dir.path())),
        run: None,
        dict: None,
        attrs,
        record: None,
        texts: Vec::new(),
        terms: None,
        files: BTreeMap::new(),
        plan,
        prefix: "v00000".to_string(),
    };
    manifest.deltas.remove(1);
    assert!(!rebase_into(&mut manifest, &completed));
}
