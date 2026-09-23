use super::*;

/// When the growth a set of artifact records carries has to reach a side-manifest.
pub(super) enum Publish {
    /// As soon as the deny lane is empty: one manifest per request.
    Promptly,
    /// At the next tick, or a manifest write per batch would hold the write thread.
    AtTick,
}

/// The `(layer, level)` a record changes the artifacts of, and `None` for every other record.
pub(super) fn artifact_level_of(record: &WalRecord) -> Option<(&str, u32)> {
    match record {
        WalRecord::ArtifactPublish { layer, level, .. }
        | WalRecord::ArtifactGrow { layer, level, .. }
        | WalRecord::ArtifactFill { layer, level, .. } => Some((layer.as_str(), *level)),
        _ => None,
    }
}

/// What a joining set is subtracted against. `restating` is `None` where any row could be.
pub(super) struct HeldMembers<'a> {
    pub(super) store: &'a tessera_lifecycle::ArtifactStore,
    pub(super) restating: Option<&'a croaring::Bitmap>,
}

/// The entities of a window's join rows: the only rows that can restate a membership.
pub(super) fn joining_entities<W>(closed: &[tessera_lifecycle::ClosedEntry<W>]) -> croaring::Bitmap {
    let mut restating = croaring::Bitmap::new();
    for entry in closed {
        for row in entry.rows().iter().filter(|row| row.join) {
            // Entity space is `u32`-wide, so the narrowing is total.
            restating.add(row.entity_id.raw() as u32);
        }
    }
    restating
}

/// The entity one membership's row index names, and `None` for a row the source does not carry.
pub(super) type EntityOfRow<'a> = &'a dyn Fn(usize, u32) -> Option<EntityId>;

fn row_not_carried(layer: &str, row: u32) -> String {
    format!("column '{layer}' names row {row}, which this batch does not carry")
}

/// The joins both write doors' resolved memberships declared, as one bitmap per `(layer, level,
/// ordinal)` less what the artifact already holds. A key with no ordinal was minted at this commit.
pub(super) fn grouped_growth<'a>(
    sources: impl Iterator<Item = &'a [tessera_lifecycle::ResolvedMembership]>,
    entity_of: EntityOfRow<'_>,
    held: Option<HeldMembers<'_>>,
) -> Result<Vec<(WalRecord, usize)>, String> {
    use std::collections::BTreeMap;
    /// One `(layer, level)`'s joins: the source to blame for the append, and a bitmap per ordinal.
    type Level = (usize, BTreeMap<u32, croaring::Bitmap>);
    // Ordered, so replay does not depend on hash iteration.
    let mut by_level: BTreeMap<(&str, u32), Level> = BTreeMap::new();
    for (index, memberships) in sources.enumerate() {
        for join in memberships {
            let Some(ordinal) = join.ordinal else {
                continue;
            };
            let (_, ordinals) = by_level
                .entry((join.layer.as_str(), join.level))
                .or_insert_with(|| (index, BTreeMap::new()));
            let joining = ordinals.entry(ordinal).or_default();
            for row in &join.rows {
                let entity =
                    entity_of(index, *row).ok_or_else(|| row_not_carried(&join.layer, *row))?;
                // Entity space is `u32`-wide, so the narrowing is total.
                joining.add(entity.raw() as u32);
            }
        }
    }
    // What the artifact already holds is not a join, or a restating row would append a record
    // that changes nothing and pins the log. Read against the store before this commit applies.
    if let Some(held) = held {
        for ((layer, level), (_, ordinals)) in &mut by_level {
            for (ordinal, joining) in ordinals.iter_mut() {
                if held.restating.is_some_and(|rows| !joining.intersect(rows)) {
                    continue;
                }
                if let Some(record) = held.store.get(layer, *level, *ordinal) {
                    joining.andnot_inplace(&record.members);
                }
            }
        }
    }
    Ok(by_level
        .into_iter()
        .filter_map(|((layer, level), (index, ordinals))| {
            let joins = ordinals
                .iter()
                .map(|(ordinal, joining)| (*ordinal, joining));
            tessera_lifecycle::membership::growth_record(layer, level, joins)
                .map(|record| (record, index))
        })
        .collect())
}

/// The artifacts both write doors' resolved memberships named and no artifact holds, one per
/// `(layer, level, key)`. Two sources naming one unknown key mint once and both join it.
pub(super) fn grouped_mints<'a>(
    sources: impl Iterator<Item = &'a [tessera_lifecycle::ResolvedMembership]>,
    entity_of: EntityOfRow<'_>,
) -> Result<MintPlan, String> {
    let mut wanted: MintPlan = std::collections::BTreeMap::new();
    for (index, memberships) in sources.enumerate() {
        for join in memberships {
            if join.ordinal.is_some() {
                continue;
            }
            let (_, members) = wanted
                .entry((
                    join.layer.clone(),
                    join.level,
                    join.view.clone(),
                    join.key.clone(),
                ))
                // The first source that named the key owns the mint.
                .or_insert_with(|| (index, croaring::Bitmap::new()));
            for row in &join.rows {
                let entity =
                    entity_of(index, *row).ok_or_else(|| row_not_carried(&join.layer, *row))?;
                // Entity space is `u32`-wide, so the narrowing is total.
                members.add(entity.raw() as u32);
            }
        }
    }
    Ok(wanted)
}

/// The growth records one closed window owes, in the order to append them. The entities are
/// `entity_ids[row]`, the assignment this window just made.
pub(super) fn growth_records<W>(
    closed: &[tessera_lifecycle::ClosedEntry<W>],
    held: Option<HeldMembers<'_>>,
) -> Result<Vec<(WalRecord, usize)>, String> {
    grouped_growth(
        closed.iter().map(|entry| entry.memberships.as_slice()),
        &|entry, row| closed[entry].entity_ids.get(row as usize).copied(),
        held,
    )
}

/// What a closed window is about to mint, and the edges its close has to settle. `None` where it
/// has neither. Minting happens at the close, not admission, so [`Executor::prepare_mints`]
/// re-resolves the plan against the store in case a publication landed since.
pub(super) fn mint_plan<W>(
    closed: &[tessera_lifecycle::ClosedEntry<W>],
) -> Result<Option<(MintPlan, Vec<tessera_lifecycle::BatchEdge>)>, String> {
    let wanted = grouped_mints(
        closed.iter().map(|entry| entry.memberships.as_slice()),
        &|entry, row| closed[entry].entity_ids.get(row as usize).copied(),
    )?;
    let edges: Vec<tessera_lifecycle::BatchEdge> =
        closed.iter().flat_map(|e| e.edges.iter().cloned()).collect();
    if wanted.is_empty() && edges.is_empty() {
        return Ok(None);
    }
    Ok(Some((wanted, edges)))
}

/// An artifact a key names: `(layer, level, view, key)`, the view `None` on an entity-scoped layer.
pub(super) type MintKey = (String, u32, Option<String>, String);

/// What one commit is about to mint: each key to the source that first named it, and the entities
/// joining it.
pub(super) type MintPlan = std::collections::BTreeMap<MintKey, (usize, croaring::Bitmap)>;

/// A key that acquired an artifact between its resolution and its preparation grows into it
/// rather than minting a second one: writes the ordinals found back onto the memberships, so
/// `growth_records` carries them as ordinary joins.
pub(super) fn settle_resolved_ordinals(
    memberships: &mut [tessera_lifecycle::ResolvedMembership],
    resolved: &std::collections::BTreeMap<MintKey, u32>,
) {
    if resolved.is_empty() {
        return;
    }
    for join in memberships.iter_mut() {
        if join.ordinal.is_some() {
            continue;
        }
        let at = (
            join.layer.clone(),
            join.level,
            join.view.clone(),
            join.key.clone(),
        );
        join.ordinal = resolved.get(&at).copied();
    }
}

/// What [`Executor::prepare_mints`] answers.
pub(super) struct PreparedMints {
    /// The publication records to append, in that order.
    pub(super) records: Vec<WalRecord>,
    /// Keys that turned out already held, and the ordinal each resolved to; the caller grows into
    /// those instead of minting.
    pub(super) resolved: std::collections::BTreeMap<MintKey, u32>,
    /// The keys this run created.
    pub(super) minted: std::collections::BTreeSet<MintKey>,
}

/// A borrowed [`MintKey`].
type MintAt<'a> = (&'a str, u32, Option<&'a str>, &'a str);

/// The parent each child in these edges is named under, refusing a child named under two. Keyed by
/// the child's own level, since one key can legitimately sit at two levels with a different parent
/// at each.
pub(super) fn parent_of_each_child(
    edges: &[tessera_lifecycle::BatchEdge],
) -> Result<std::collections::BTreeMap<MintAt<'_>, &str>, String> {
    let mut claimed: std::collections::BTreeMap<MintAt<'_>, &str> = Default::default();
    for edge in edges {
        let at = (
            edge.layer.as_str(),
            edge.level,
            edge.view.as_deref(),
            edge.child.as_str(),
        );
        if let Some(first) = claimed.insert(at, edge.parent.as_str()) {
            if first != edge.parent {
                return Err(format!(
                    "{} in level {} of {} is named as a child of both {first} and {}; a list \
                     column declares the edges, so name one parent for the child or publish the \
                     two hierarchies as separate layers",
                    edge.child, edge.level, edge.layer, edge.parent
                ));
            }
        }
    }
    Ok(claimed)
}

impl Executor {
    /// Resolve one batch's membership keys, and decide the edges its adjacency declared. `Err`
    /// refuses the whole batch without effect.
    pub(super) fn resolve_memberships(
        &self,
        artifacts: &tessera_lifecycle::BatchArtifacts,
    ) -> Result<
        (
            Vec<tessera_lifecycle::ResolvedMembership>,
            Vec<tessera_lifecycle::BatchEdge>,
        ),
        String,
    > {
        if artifacts.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        self.live.with_publication_state(|registry, store, _| {
            let memberships: Vec<tessera_lifecycle::ResolvedMembership> = artifacts
                .memberships
                .iter()
                .map(|join| {
                    registry
                        .resolve_or_mint(
                            &join.layer,
                            join.level,
                            join.view.as_deref(),
                            &join.key,
                            store,
                        )
                        .map(|ordinal| tessera_lifecycle::ResolvedMembership {
                            layer: join.layer.clone(),
                            level: join.level,
                            view: join.view.clone(),
                            key: join.key.clone(),
                            ordinal,
                            rows: join.rows.clone(),
                        })
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<_, String>>()?;
            // A child's level is asked precisely; a parent's is asked of the whole layer, since one
            // key can legitimately sit at two levels.
            let minting: std::collections::BTreeSet<MintAt<'_>> = memberships
                .iter()
                .filter(|m| m.ordinal.is_none())
                .map(|m| (m.layer.as_str(), m.level, m.view.as_deref(), m.key.as_str()))
                .collect();
            let anywhere: std::collections::BTreeSet<(&str, Option<&str>, &str)> = minting
                .iter()
                .map(|(layer, _, view, key)| (*layer, *view, *key))
                .collect();

            parent_of_each_child(&artifacts.edges)?;

            let mut settling = Vec::new();
            for edge in &artifacts.edges {
                let (layer, view) = (edge.layer.as_str(), edge.view.as_deref());
                match registry.check_edge(
                    edge,
                    store,
                    minting.contains(&(layer, edge.level, view, edge.child.as_str())),
                    &|key| anywhere.contains(&(layer, view, key)),
                ) {
                    Ok(tessera_lifecycle::EdgeCheck::Agrees) => {}
                    // Carried to the close: on the publication that creates the child, or as a
                    // fill on a child that exists without a parent.
                    Ok(
                        tessera_lifecycle::EdgeCheck::Mints
                        | tessera_lifecycle::EdgeCheck::Records,
                    ) => settling.push(edge.clone()),
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok((memberships, settling))
        })
    }

    /// The artifacts these rows' values named and nothing holds: one per
    /// `membership = { attribute = f }` layer whose column carried a value the level has no
    /// artifact for. Each row is a cell list positional against the declared scalars. Runs after
    /// the vocabulary mint, since a novel category key is a string in the row until that pass
    /// draws it a code.
    pub(super) fn derive_records<'a>(
        &mut self,
        rows: impl Iterator<Item = &'a [WalScalar]> + Clone,
        vocabularies: &Vocabularies,
    ) -> Result<Vec<WalRecord>, String> {
        use tessera_types::layer::attribute_value_key;

        let generation = self.generation.load();
        let declared = &generation.bundle.manifest.declared_scalars;
        let predicates: Vec<(String, usize, Option<String>)> =
            self.live.predicate_columns(|field| {
                let index = declared.iter().position(|scalar| scalar.name == field)?;
                Some((index, declared[index].vocabulary.clone()))
            });
        if predicates.is_empty() {
            return Ok(Vec::new());
        }

        let mut records = Vec::new();
        for (layer, index, vocabulary) in predicates {
            // Resolved one code at a time against the minter's reverse map, not built as a whole
            // `code → key` pass over the vocabulary.
            let minter = vocabulary
                .as_deref()
                .and_then(|name| vocabularies.get(name));
            let mut wanted: std::collections::BTreeSet<String> = Default::default();
            for row in rows.clone() {
                let Some(code) = row.get(index).and_then(scalar_code) else {
                    continue;
                };
                // Code 0 is category code space's reserved absent sentinel, unlike a plain
                // integer column.
                if vocabulary.is_some() && code == tessera_store::vocabulary::ABSENT_CODE {
                    continue;
                }
                wanted.insert(attribute_value_key(
                    code,
                    minter.and_then(|minter| minter.key_of(code)),
                ));
            }
            if wanted.is_empty() {
                continue;
            }
            let prepared = self.live.with_publication_state(|registry, store, alloc| {
                let fresh: Vec<String> = wanted
                    .iter()
                    .filter(|key| store.ordinal_of_key(&layer, 0, None, key).is_none())
                    .cloned()
                    .collect();
                if fresh.is_empty() {
                    return Ok(None);
                }
                registry
                    .prepare_derive(&layer, 0, &fresh, store, alloc)
                    .map(Some)
                    .map_err(|e| e.to_string())
            })?;
            if let Some(record) = prepared {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Prepare the publications that create every artifact this window's rows named and nothing
    /// holds. Patches the memberships whose key resolved since admission to grow rather than mint;
    /// leaves a minted key's ordinal `None`, which tells [`growth_records`] the publication
    /// carried the join.
    pub(super) fn mint_records(
        &mut self,
        closed: &mut [tessera_lifecycle::ClosedEntry<Reply<Ingested>>],
    ) -> Result<(Vec<WalRecord>, Vec<u64>), String> {
        let mut minted_per_entry = vec![0u64; closed.len()];
        let Some((wanted, edges)) = mint_plan(closed)? else {
            return Ok((Vec::new(), minted_per_entry));
        };
        let PreparedMints {
            records,
            resolved,
            minted,
        } = self.prepare_mints(&wanted, &edges)?;

        for entry in closed.iter_mut() {
            settle_resolved_ordinals(&mut entry.memberships, &resolved);
        }
        for (at, (index, _)) in &wanted {
            if minted.contains(at) {
                minted_per_entry[*index] += 1;
            }
        }
        Ok((records, minted_per_entry))
    }

    /// Prepare one set of mints: the publications that create the artifacts a caller's keys named
    /// and no artifact holds, one implementation shared by both write doors. The records come back
    /// in the order to append: ascending level, coarse first.
    pub(super) fn prepare_mints(
        &self,
        wanted: &MintPlan,
        edges: &[tessera_lifecycle::BatchEdge],
    ) -> Result<PreparedMints, String> {
        use std::collections::BTreeMap;
        self.live.with_publication_state(|registry, store, alloc| {
            let parents = parent_of_each_child(edges)?;
            // Re-resolved here, not trusted from admission, since a publication may land between.
            let mut resolved: BTreeMap<MintKey, u32> = BTreeMap::new();
            // One publication per view, since a parent is resolved inside its child's view.
            type Minting<'a> = Vec<(&'a str, &'a croaring::Bitmap)>;
            let mut to_mint: BTreeMap<(&str, u32, Option<&str>), Minting<'_>> = BTreeMap::new();
            for (at, (_, members)) in wanted {
                let (layer, level, view, key) = at;
                match store.ordinal_of_key(layer, *level, view.as_deref(), key) {
                    Some(ordinal) => {
                        resolved.insert(at.clone(), ordinal);
                    }
                    None => to_mint
                        .entry((layer.as_str(), *level, view.as_deref()))
                        .or_default()
                        .push((key.as_str(), members)),
                }
            }

            // Ascending level, coarse first: a tiered chain's parent is fixed by the prior record.
            let mut assigned: BTreeMap<MintAt<'_>, u32> = BTreeMap::new();
            let mut records = Vec::new();
            for ((layer, level, view), keys) in &to_mint {
                let incoming: Vec<tessera_lifecycle::IncomingArtifact> = keys
                    .iter()
                    .map(|(key, members)| tessera_lifecycle::IncomingArtifact {
                        key: Some((*key).to_string()),
                        view: view.map(str::to_string),
                        members: (*members).clone(),
                        excluding: None,
                        contents: Vec::new(),
                        attached_to: None,
                        parent_keys: parents
                            .get(&(*layer, *level, *view, *key))
                            .map(|parent| vec![(*parent).to_string()])
                            .unwrap_or_default(),
                        shape: None,
                        // A key column names no label, so a minted artifact takes the default.
                        access: Vec::new(),
                    })
                    .collect();
                let pending = |key: &str| {
                    let coarser = level.checked_sub(1)?;
                    assigned.get(&(*layer, coarser, *view, key)).map(|ordinal| {
                        tessera_lifecycle::wal::ParentRef {
                            level: coarser,
                            ordinal: *ordinal,
                        }
                    })
                };
                let record = registry
                    .prepare_publish(layer, *level, &incoming, store, alloc, &pending)
                    .map_err(|e| e.to_string())?;
                let WalRecord::ArtifactPublish { artifacts, .. } = &record else {
                    unreachable!("prepare_publish returns an ArtifactPublish");
                };
                // Read back off the record, not recomputed.
                for ((key, _), artifact) in keys.iter().zip(artifacts) {
                    debug_assert_eq!(artifact.key.as_deref(), Some(*key));
                    assigned.insert((*layer, *level, *view, key), artifact.ordinal);
                }
                records.push(record);
            }

            // Seeded with the edges the publications above are about to create, since the cycle
            // check reads the layer's held edges and this window's pending ones as one graph:
            // nothing prepared here is in the store yet.
            let mut window_edges: BTreeMap<
                &str,
                BTreeMap<tessera_lifecycle::wal::ParentRef, Vec<tessera_lifecycle::wal::ParentRef>>,
            > = BTreeMap::new();
            for record in &records {
                let WalRecord::ArtifactPublish {
                    layer,
                    level,
                    artifacts,
                    ..
                } = record
                else {
                    continue;
                };
                let held = window_edges.entry(layer.as_str()).or_default();
                for artifact in artifacts.iter().filter(|a| !a.parents.is_empty()) {
                    held.insert(
                        tessera_lifecycle::wal::ParentRef {
                            level: *level,
                            ordinal: artifact.ordinal,
                        },
                        artifact.parents.clone(),
                    );
                }
            }
            let mut fills = Vec::new();
            for edge in edges {
                let view = edge.view.as_deref();
                if assigned.contains_key(&(edge.layer.as_str(), edge.level, view, edge.child.as_str()))
                {
                    continue;
                }
                let pending = |key: &str| {
                    assigned
                        .iter()
                        .find(|((layer, _, in_view, held), _)| {
                            *layer == edge.layer && *in_view == view && *held == key
                        })
                        .map(|((_, level, _, _), ordinal)| tessera_lifecycle::wal::ParentRef {
                            level: *level,
                            ordinal: *ordinal,
                        })
                };
                if let Some(record) = registry
                    .prepare_parent_fill(
                        edge,
                        store,
                        &pending,
                        window_edges.entry(edge.layer.as_str()).or_default(),
                    )
                    .map_err(|e| e.to_string())?
                {
                    fills.push(record);
                }
            }
            records.extend(fills);

            let minted = assigned
                .keys()
                .map(|(layer, level, view, key)| {
                    (
                        (*layer).to_string(),
                        *level,
                        view.map(str::to_string),
                        (*key).to_string(),
                    )
                })
                .collect();
            Ok(PreparedMints {
                records,
                resolved,
                minted,
            })
        })
    }

    /// Hold one accepted write's delta until the tick, when the level's row forms take the run of
    /// them. `refused` names the growth entries the store did not take; a record whose every entry
    /// was refused is still held, empty, since a level's delta versions must stay consecutive.
    pub(super) fn hold_delta(&mut self, record: &WalRecord, before: u64, refused: &[usize]) {
        let (layer, level, kind) = match record {
            WalRecord::ArtifactPublish {
                layer,
                level,
                artifacts,
                ..
            } => (
                layer,
                *level,
                crate::artifacts::DeltaKind::Published(
                    artifacts.iter().map(|a| a.ordinal).collect(),
                ),
            ),
            WalRecord::ArtifactGrow {
                layer,
                level,
                growth,
            } => {
                let mut joins = Vec::new();
                let mut pages = Vec::new();
                for (index, grown) in growth.iter().enumerate() {
                    if refused.contains(&index) {
                        continue;
                    }
                    let Some(joining) =
                        tessera_lifecycle::membership::deserialise_members(&grown.joining)
                    else {
                        continue;
                    };
                    match grown.set {
                        tessera_lifecycle::wal::GrownSet::Membership => {
                            joins.push((grown.ordinal, joining))
                        }
                        tessera_lifecycle::wal::GrownSet::GeneratingSet { rank, cardinality } => {
                            // A leave re-derives the operator whole: a union cannot express one.
                            let leaves =
                                tessera_lifecycle::membership::deserialise_leaving(&grown.leaving)
                                    .is_none_or(|leaving| !leaving.is_empty());
                            pages.push(crate::artifacts::SetPage {
                                ordinal: grown.ordinal,
                                rank,
                                joining,
                                whole: leaves || cardinality == 0,
                            });
                        }
                    }
                }
                (
                    layer,
                    *level,
                    crate::artifacts::DeltaKind::Grown { joins, pages },
                )
            }
            WalRecord::ArtifactFill {
                layer,
                level,
                ordinal,
                ..
            } => (
                layer,
                *level,
                crate::artifacts::DeltaKind::Filled(vec![*ordinal]),
            ),
            _ => return,
        };
        self.pending_forms
            .entry((layer.clone(), level))
            .or_default()
            .push(crate::artifacts::LevelDelta { before, kind });
    }

    /// Applies durable artifact records to the registry and the store, and holds the delta each
    /// made. Each record moves its level's version by one, so a delta's starting version walks
    /// with the records. `publish` says only when the growth reaches a side-manifest.
    pub(super) fn apply_artifact_records(
        &mut self,
        records: &[&WalRecord],
        positions: &[u64],
        publish: Publish,
    ) {
        if records.is_empty() {
            return;
        }
        let befores: Vec<u64> = self.live.with_artifacts(|store| {
            let mut seen: std::collections::BTreeMap<(&str, u32), u64> = Default::default();
            records
                .iter()
                .map(|record| {
                    let Some((layer, level)) = artifact_level_of(record) else {
                        return 0;
                    };
                    let at = seen
                        .entry((layer, level))
                        .or_insert_with(|| store.level_version(layer, level));
                    let version = *at;
                    *at += 1;
                    version
                })
                .collect()
        });
        let mut refused_per_record: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
        let undecodable = self.live.with_publication_state(|registry, store, _| {
            records
                .iter()
                .zip(positions)
                .zip(refused_per_record.iter_mut())
                .map(|((record, position), refused)| {
                    registry.apply(record);
                    store.apply_reporting(record, *position, refused)
                })
                .sum::<usize>()
        });
        if undecodable > 0 {
            tracing::error!(
                count = undecodable,
                "ALARM: artifact records did not survive their own round trip; those artifacts are absent"
            );
        }
        for ((record, before), refused) in records.iter().zip(befores).zip(&refused_per_record) {
            self.hold_delta(record, before, refused);
        }
        // Durable in the log and not yet in a manifest.
        match publish {
            Publish::Promptly => self.side_manifests.behind_live = true,
            Publish::AtTick => self.side_manifests.growth_unpublished = true,
        }
    }
}
