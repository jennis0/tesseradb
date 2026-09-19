use super::*;

/// The bundle's declared scalar tail: render columns only.
pub(crate) fn scalar_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Vec<(String, ScalarType)> {
    // A filter-only column has no row slot; including one here makes `gather_scalars` refuse the
    // build's own segment for correctly omitting it.
    manifest
        .render_scalars()
        .map(|d| (d.name.clone(), d.arrow_type))
        .collect()
}

/// Every view's group-scoped attribute families, keyed by view id.
///
/// A family belongs to the group that owns the keys; a `members` group's own list is always
/// empty. The ingest boundary, the commit window and the flush all take this one derivation, so a
/// row's positional tail is built and read against the same list.
pub(crate) fn scoped_families_by_view(
    manifest: &tessera_store::manifest::Manifest,
) -> FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> {
    let mut out: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
        FxHashMap::default();
    for group in &manifest.groups {
        for view in &group.views {
            let id = format!(
                "{}{}{}",
                group.name,
                tessera_store::GROUP_SEPARATOR,
                view.key
            );
            let families = scoped_families_of_view(manifest, &id);
            if families.is_empty() {
                continue;
            }
            out.insert(id, families.to_vec());
        }
    }
    out
}

/// One view's group-scoped families: [`scoped_families_by_view`]'s rule, for a single view.
///
/// In the owning group's manifest order, the order a row's `scoped` list is positional against.
/// Empty for a plain view, for a view whose key the owner does not carry, or a group with none.
pub(crate) fn scoped_families_of_view<'a>(
    manifest: &'a tessera_store::manifest::Manifest,
    view: &str,
) -> &'a [tessera_store::manifest::ScopedScalar] {
    pub(in crate::write) const NONE: &[tessera_store::manifest::ScopedScalar] = &[];
    let owner = scoped_owner_view_of(manifest, view);
    let Some((owner_group, key)) = owner.split_once(tessera_store::GROUP_SEPARATOR) else {
        return NONE;
    };
    let Some(group) = manifest.groups.iter().find(|g| g.name == owner_group) else {
        return NONE;
    };
    // The key is matched, not assumed: one the owning group does not carry addresses no cell.
    if !group.views.iter().any(|v| v.key == key) {
        return NONE;
    }
    &group.scoped_scalars
}

/// The view id a scoped value written through `view` is addressed by: the owning group's view of
/// the same key.
///
/// A sharing group's view resolves to the owner's, so a row written through either lands under the
/// same column name.
pub(crate) fn scoped_owner_view_of(
    manifest: &tessera_store::manifest::Manifest,
    view: &str,
) -> String {
    let Some((group, key)) = view.split_once(tessera_store::GROUP_SEPARATOR) else {
        return view.to_string();
    };
    let owner = manifest
        .groups
        .iter()
        .find(|g| g.name == group)
        .and_then(|g| g.members_of.as_deref())
        .unwrap_or(group);
    format!("{owner}{}{key}", tessera_store::GROUP_SEPARATOR)
}

/// One view's writer schema: the bundle-wide render tail, then the group-scoped render lanes.
///
/// Every producer of a segment must take this rather than `scalar_schema_of` alone, or a rewrite
/// drops the per-family lanes and a served value reads back as the type's zero. No gate here: a
/// writer has no principal.
pub(crate) fn view_scalar_schema_of(
    manifest: &tessera_store::manifest::Manifest,
    view: &str,
) -> Vec<(String, ScalarType)> {
    let mut schema = scalar_schema_of(manifest);
    schema.extend(
        crate::viewport::scoped_render_families(manifest, view)
            .into_iter()
            .map(|f| (f.name.clone(), f.arrow_type)),
    );
    schema
}

/// The columns of one view's writer schema an input segment may lawfully lack, for a merge or a
/// fold of segments written before them (`tessera_store::segment_cursor::gather_scalars`): the
/// group-scoped render lanes, which begin at `entity_scoped` in the schema, and the entity-scoped
/// columns declared at a running service and not yet folded. Every other column of the schema is
/// one every input holds, and one missing is a torn segment.
pub(crate) fn lawful_absences(
    schema: &[(String, ScalarType)],
    entity_scoped: usize,
    runtime: &[String],
) -> Vec<String> {
    schema
        .iter()
        .enumerate()
        .filter(|(position, (name, _))| {
            *position >= entity_scoped || runtime.iter().any(|held| held == name)
        })
        .map(|(_, (name, _))| name.clone())
        .collect()
}

/// The filterable columns, with the position each occupies in a buffered row's scalar list.
///
/// Positional against the full `declared_scalars`, not the render tail: `scalar_schema_of`
/// narrows to render columns, so building this from that would misalign every filter column after
/// the first difference. A `visibility = "derived"` category is included whether or not it is
/// declared filterable, so `/v1/categories` can still offer values from it.
pub(crate) fn filter_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Vec<crate::flush::FilterColumnSpec> {
    manifest
        .declared_scalars
        .iter()
        .enumerate()
        // Text is not here: `owes_value_column` says it owes no value column. Its flush track is
        // `text_schema_of`.
        .filter(|(_, d)| crate::filter::owes_value_column(d, &manifest.vocabularies))
        .map(|(index, d)| crate::flush::FilterColumnSpec {
            index,
            name: d.name.clone(),
            ty: d.arrow_type,
            category: d.vocabulary.is_some(),
        })
        .collect()
}

/// The indexed `text` columns, each with the analyser its declaration named.
///
/// Refuses rather than defaults when the binary does not carry the recorded analyser: a match
/// query would otherwise answer from whichever layer holds the entity, with no error.
pub(crate) fn text_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Result<Vec<crate::flush::TextColumnSpec>, crate::flush::MaintenanceFailed> {
    let mut out = Vec::new();
    for (index, d) in manifest.declared_scalars.iter().enumerate() {
        if d.arrow_type != tessera_spatial::tiler::ScalarType::Text || !d.index {
            continue;
        }
        let identity = d.analyser.as_deref().ok_or_else(|| {
            crate::flush::MaintenanceFailed(format!(
                "column '{}' is text but the manifest records no analyser identity",
                d.name
            ))
        })?;
        let analyser = tessera_analyse::analyser(identity.split('/').next().unwrap_or_default())
            .filter(|a| a.identity() == identity)
            .ok_or_else(|| {
                crate::flush::MaintenanceFailed(format!(
                    "column '{}' was indexed by analyser '{identity}', which this binary does not \
                     carry. A flush cannot extend an index whose terms it cannot reproduce.",
                    d.name
                ))
            })?;
        out.push(crate::flush::TextColumnSpec {
            index,
            name: d.name.clone(),
            analyser: std::sync::Arc::new(analyser),
        });
    }
    Ok(out)
}

/// The analyser a group-scoped `text` family's terms were produced by
/// ([`text_schema_of`]'s resolution, over a family's declaration).
pub(in crate::write) fn analyser_of(
    family: &tessera_store::manifest::ScopedScalar,
) -> Result<tessera_analyse::Analyser, crate::flush::MaintenanceFailed> {
    let identity = family.analyser.as_deref().ok_or_else(|| {
        crate::flush::MaintenanceFailed(format!(
            "the scoped column family '{}' is text but the manifest records no analyser identity",
            family.name
        ))
    })?;
    tessera_analyse::analyser(identity.split('/').next().unwrap_or_default())
        .filter(|a| a.identity() == identity)
        .ok_or_else(|| {
            crate::flush::MaintenanceFailed(format!(
                "the scoped column family '{}' was indexed by analyser '{identity}', which this \
                 binary does not carry. A flush cannot extend an index whose terms it cannot \
                 reproduce.",
                family.name
            ))
        })
}

/// The blob-resident columns, with each one's position in a buffered row's scalar list.
///
/// The predicate must be the build's, [`crate::filter::blob_resident`]: this, the render indices
/// and the filter schema must partition `declared_scalars` identically, or a column none of them
/// claims is acknowledged and then lost.
pub(crate) fn record_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Vec<crate::flush::RecordColumnSpec> {
    manifest
        .declared_scalars
        .iter()
        .enumerate()
        .filter(|(_, d)| crate::filter::blob_resident(d, &manifest.vocabularies))
        .map(|(index, d)| crate::flush::RecordColumnSpec {
            index,
            name: d.name.clone(),
            ty: d.arrow_type,
        })
        .collect()
}

/// The category-width code a row's scalar carries, or `None` where it carries none.
///
/// Only the three category widths carry a code; a wider or non-integer column never names a
/// predicate layer, so reaching one here is a schema that never validated.
pub(in crate::write) fn scalar_code(scalar: &WalScalar) -> Option<u32> {
    match scalar {
        WalScalar::U8(v) => Some(u32::from(*v)),
        WalScalar::U16(v) => Some(u32::from(*v)),
        WalScalar::U32(v) => Some(*v),
        _ => None,
    }
}

/// A vocabulary code, at its column's declared width.
///
/// `is_category_width` admits `u8`/`u16`/`u32` only, so the fallthrough is `u32`, the widest,
/// which cannot truncate a code the other two could hold.
pub(in crate::write) fn code_at_declared_width(width: ScalarType, code: u32) -> WalScalar {
    match width {
        ScalarType::U8 => WalScalar::U8(code as u8),
        ScalarType::U16 => WalScalar::U16(code as u16),
        _ => WalScalar::U32(code),
    }
}

#[cfg(test)]
mod segment_schema_tests {
    use super::*;
    use tessera_plugin::Plugin;
    use tessera_store::manifest::DeclaredScalar;

    /// A segment's writer schema is the render columns; this guards the line that makes it so.
    ///
    /// Calls the production function rather than re-deriving its filter, because a re-implemented
    /// predicate would pass even if the production filter were wrong.
    #[test]
    pub(in crate::write) fn a_segments_writer_schema_omits_filter_only_columns() {
        let manifest = tessera_store::manifest::Manifest {
            bundle_format: 3,
            created_at: String::new(),
            data_plugin_hash: tessera_plugin::Passthrough::new().data_plugin_hash(),
            declared_bounds: serde_json::json!({}),
            vocabularies: vec![],
            small_term_threshold: 32,
            entity_id_high_water: 0,
            identity: tessera_store::manifest::IdentityDescriptor {
                construction: "siphash-2-4".to_string(),
                rounds: 1,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            groups: Vec::new(),
            views: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: std::collections::BTreeMap::new(),
            declared_scalars: vec![
                DeclaredScalar {
                    name: "department".to_string(),
                    arrow_type: ScalarType::U16,
                    vocabulary: Some("departments".to_string()),
                    analyser: None,
                    index: true,
                    render: true,
                },
                DeclaredScalar {
                    name: "title".to_string(),
                    arrow_type: ScalarType::Utf8,
                    vocabulary: None,
                    analyser: None,
                    index: true,
                    render: false,
                },
            ],
        };

        let schema = scalar_schema_of(&manifest);
        assert_eq!(
            schema,
            vec![("department".to_string(), ScalarType::U16)],
            "a filter-only column must not reach the segment writer"
        );

        // The full list is untouched: the ingest plane supplies values for every declared column,
        // filterable ones included.
        assert_eq!(manifest.declared_scalars.len(), 2);
    }
}
