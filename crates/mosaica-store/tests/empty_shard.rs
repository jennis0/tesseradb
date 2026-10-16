//! An empty shard's base holds every structure its declarations name and no item, and each opens
//! with the reader that serves it.

use std::collections::BTreeSet;
use std::path::Path;

use mosaica_filter::{Access, ColumnPostings, RecordBlob, SortedDict, ValueColumn};
use mosaica_spatial::tiler::ScalarType;
use mosaica_store::empty_shard::{write_empty_shard, EmptyShard, EmptyView};
use mosaica_store::manifest::{
    DeclaredScalar, ManifestVocabulary, ScopedScalar, Visibility, VocabularyKind,
};
use mosaica_store::ShardLayout;
use mosaica_types::ShardId;

fn scalar(name: &str, arrow_type: ScalarType, index: bool, render: bool) -> DeclaredScalar {
    DeclaredScalar {
        name: name.to_string(),
        arrow_type,
        vocabulary: None,
        analyser: None,
        index,
        render,
        unique: false,
    }
}

fn category(name: &str, vocabulary: &str, index: bool, render: bool) -> DeclaredScalar {
    DeclaredScalar {
        vocabulary: Some(vocabulary.to_string()),
        ..scalar(name, ScalarType::U8, index, render)
    }
}

fn vocabulary(name: &str, visibility: Visibility) -> ManifestVocabulary {
    ManifestVocabulary {
        name: name.to_string(),
        kind: VocabularyKind::Discovered,
        visibility,
        width: ScalarType::U8,
        values: Vec::new(),
        reserved: Vec::new(),
    }
}

fn family(name: &str, arrow_type: ScalarType, views: &[&str]) -> ScopedScalar {
    ScopedScalar {
        name: name.to_string(),
        group: "g".to_string(),
        arrow_type,
        vocabulary: None,
        analyser: None,
        index: true,
        render: arrow_type != ScalarType::Text,
        views: views.iter().map(|v| v.to_string()).collect(),
    }
}

/// Every file under `dir`, relative to `prefix`.
fn files_under(prefix: &Path, dir: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(prefix).unwrap();
                found.insert(rel.to_str().unwrap().to_string());
            }
        }
    }
    found
}

#[test]
fn an_empty_shard_holds_every_declared_structure_and_no_item() {
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix = tmp.path();
    let layout = ShardLayout::new("default", ShardId::new(3).unwrap());
    let declared = vec![
        scalar("kw", ScalarType::Keyword, true, false),
        category("kind", "public", true, true),
        category("source", "derived", false, false),
        category("plain", "public", false, false),
        scalar("num", ScalarType::F64, false, true),
        scalar("note", ScalarType::Text, true, false),
        DeclaredScalar {
            unique: true,
            ..scalar("id", ScalarType::U64, false, false)
        },
    ];
    let vocabularies = vec![
        vocabulary("public", Visibility::Public),
        vocabulary("derived", Visibility::Derived),
    ];
    let scoped = vec![
        family("score", ScalarType::F32, &["g:a", "g:b"]),
        family("memo", ScalarType::Text, &["g:a"]),
    ];
    let tail = |extra: &[(&str, ScalarType)]| -> Vec<(String, ScalarType)> {
        [("kind", ScalarType::U8), ("num", ScalarType::F64)]
            .iter()
            .chain(extra)
            .map(|(name, ty)| (name.to_string(), *ty))
            .collect()
    };
    let views = vec![
        EmptyView {
            id: "world",
            incarnation: 0,
            render: tail(&[]),
        },
        EmptyView {
            id: "g:a",
            incarnation: 2,
            render: tail(&[("score", ScalarType::F32)]),
        },
    ];
    let written = write_empty_shard(&EmptyShard {
        prefix_dir: prefix,
        layout,
        terms: 3,
        small_term_threshold: 32,
        pairs: true,
        declared: &declared,
        vocabularies: &vocabularies,
        scoped: &scoped,
        views: &views,
        seg_id: "seg-4",
    })
    .unwrap();

    let listed: BTreeSet<String> = written.files.iter().map(|(rel, _)| rel.clone()).collect();
    assert_eq!(
        listed.len(),
        written.files.len(),
        "each file is listed once"
    );
    assert_eq!(
        listed,
        files_under(prefix, &prefix.join(layout.rel())),
        "every file written is listed, and nothing else is left behind"
    );
    for (rel, path) in &written.files {
        assert_eq!(&prefix.join(rel), path);
    }

    let postings =
        mosaica_authz::PostingsReader::open(&prefix.join(layout.postings_rel()), true).unwrap();
    assert_eq!(postings.term_count(), 3, "one record for each term");
    for term in 0..3 {
        assert!(postings
            .posting_at(term)
            .unwrap()
            .is_none_or(|posting| match posting {
                mosaica_authz::PostingRef::Array(bytes) => bytes.is_empty(),
                mosaica_authz::PostingRef::Roaring(set) => set.is_empty(),
            }));
    }
    let transpose =
        mosaica_store::EntityTerms::open_dir(&prefix.join(layout.entity_terms_rel())).unwrap();
    assert_eq!(transpose.len(), 0);

    let column = |rel: String| prefix.join(rel);
    for name in ["kw", "kind", "source"] {
        let values = ValueColumn::open_dir(&column(layout.column_rel(name)), Access::Read).unwrap();
        assert!(values.present().is_empty(), "'{name}' holds no value");
    }
    let dict = SortedDict::open_dir(&column(layout.column_rel("kw")), Access::Read).unwrap();
    assert_eq!(dict.len(), 0);
    for name in ["kind", "source"] {
        ColumnPostings::open_keyed(&column(layout.column_rel(name)).join("postings.arrow"))
            .unwrap();
    }
    for name in ["plain", "num"] {
        assert!(
            !column(layout.column_rel(name)).exists(),
            "'{name}' owes no value column"
        );
    }
    let note = column(layout.column_rel("note"));
    assert_eq!(SortedDict::open_dir(&note, Access::Read).unwrap().len(), 0);
    ColumnPostings::open(&note.join("postings.arrow"), false).unwrap();
    let blob = RecordBlob::open_dir(&column(layout.record_rel()), Access::Read).unwrap();
    assert_eq!(blob.rows(), 0);
    assert!(column(layout.unique_index_rel("id")).is_dir());

    let score = column(layout.scoped_column_rel("score", "g:a", 2));
    assert!(ValueColumn::open_dir(&score, Access::Read)
        .unwrap()
        .present()
        .is_empty());
    let memo = column(layout.scoped_column_rel("memo", "g:a", 2));
    assert_eq!(SortedDict::open_dir(&memo, Access::Read).unwrap().len(), 0);
    let prose = RecordBlob::open_dir(
        &memo.join(mosaica_store::manifest::SCOPED_PROSE_DIR),
        Access::Read,
    )
    .unwrap();
    assert_eq!(prose.rows(), 0);
    assert!(
        !listed.iter().any(|rel| rel.contains("/g/b")),
        "a family's view the shard does not hold gets no column"
    );

    assert_eq!(
        written
            .segments
            .iter()
            .map(|s| (
                s.view.as_str(),
                s.incarnation,
                s.seg_id.as_str(),
                s.row_count
            ))
            .collect::<Vec<_>>(),
        vec![("world", 0, "seg-4", 0), ("g:a", 2, "seg-4", 0)]
    );
    for view in &views {
        let segment = prefix.join(layout.segment_rel(view.id, "seg-4"));
        let columns =
            mosaica_store::read::ColumnsRef::load(&segment.join("columns.arrow")).unwrap();
        assert!(columns.residual().is_empty());
        assert_eq!(
            columns.scalar_names().collect::<Vec<_>>(),
            view.render
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
        );
        let permutation =
            mosaica_store::Permutation::load(&prefix.join(layout.permutation_rel(view.id)))
                .unwrap();
        assert_eq!(permutation.bound(), 0);
        let view_dir = prefix.join(layout.view_rel(view.id));
        assert_eq!(
            std::fs::metadata(view_dir.join(mosaica_store::ROW_ENTITY_FILE))
                .unwrap()
                .len(),
            0
        );
        assert!(view_dir
            .join(mosaica_store::field_tallies::FIELD_TALLIES_FILE)
            .is_file());
    }
}

/// A shard opened after a record pack was written holds an empty member pack over its ordinals,
/// named for the publication that wrote the record pack, which pairs with it and lists nothing.
#[test]
fn an_empty_member_pack_pairs_with_its_record_pack_and_lists_nothing() {
    use mosaica_store::membership::{empty_member_pack, LevelPack, PackKind, RecordPackWriter};
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix = tmp.path();
    let space = mosaica_types::LayerSpace::new(2).unwrap();
    let mut records = RecordPackWriter::create(
        prefix,
        mosaica_store::LayerSpaceLayout::new("default", space),
        12,
        1,
        40,
        3,
    )
    .unwrap();
    for _ in 0..3 {
        records.push(&[]).unwrap();
    }
    let records = records.finish().unwrap();
    let shard = ShardLayout::new("default", ShardId::new(4).unwrap());

    let members = empty_member_pack(prefix, shard, space, &records).unwrap();
    assert!(members.pairs(space, &records));
    assert!(members.path.starts_with(&shard.members_rel()));
    assert_eq!(
        empty_member_pack(prefix, shard, space, &records)
            .unwrap()
            .path,
        members.path,
        "written again under the same name"
    );
    let pack = LevelPack::open(&prefix.join(&members.path), PackKind::Members).unwrap();
    assert_eq!((pack.ordinal_lo(), pack.count()), (40, 3));
    assert!(pack.iter().all(|(_, blob)| blob.is_empty()));
}
