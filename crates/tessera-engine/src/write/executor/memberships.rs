use super::*;

/// The `(layer, level)` a record changes the artifacts of, and `None` for every other record:
/// what a caller needs to read that level's version before the record moves it.
pub(super) fn artifact_level_of(record: &WalRecord) -> Option<(&str, u32)> {
    match record {
        WalRecord::ArtifactPublish { layer, level, .. }
        | WalRecord::ArtifactGrow { layer, level, .. }
        | WalRecord::ArtifactFill { layer, level, .. } => Some((layer.as_str(), *level)),
        _ => None,
    }
}

/// What a joining set is subtracted against: the artifact store, and the entities that could be
/// restating a membership it already holds. `restating` is `None` where any row could be.
pub(super) struct HeldMembers<'a> {
    pub(super) store: &'a tessera_lifecycle::ArtifactStore,
    pub(super) restating: Option<&'a croaring::Bitmap>,
}

/// The entities of a window's join rows: the only rows that can restate a membership, since an
/// entity this window allocated is a member of nothing yet.
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

/// The entity one membership's row index names: `(source, row)`, and `None` for a row index the
/// source does not carry.
///
/// The doors index different things: an ingest window's sources are its closed entries and the
/// entities are the ids the close has just assigned, a values batch's one source is the request and
/// the entities are the ones the caller named.
pub(super) type EntityOfRow<'a> = &'a dyn Fn(usize, u32) -> Option<EntityId>;

fn row_not_carried(layer: &str, row: u32) -> String {
    format!("column '{layer}' names row {row}, which this batch does not carry")
}

/// The growth grouping both write doors share: the joins their resolved memberships declared, as
/// one bitmap per `(layer, level, ordinal)` less what the artifact already holds, with the index of
/// the source that first named each level.
///
/// One record per `(layer, level)`, not one per source: several sources naming one cluster merge
/// into a union. A key with no ordinal is skipped: it was minted at this commit, and a minted
/// artifact is published carrying these rows, so there is one record instead of a publication and a
/// growth against it.
pub(super) fn grouped_growth<'a>(
    sources: impl Iterator<Item = &'a [tessera_lifecycle::ResolvedMembership]>,
    entity_of: EntityOfRow<'_>,
    held: Option<HeldMembers<'_>>,
) -> Result<Vec<(WalRecord, usize)>, String> {
    use std::collections::BTreeMap;
    /// One `(layer, level)`'s joins: the source to blame for the append, and a bitmap per ordinal.
    type Level = (usize, BTreeMap<u32, croaring::Bitmap>);
    // Ordered, so replay order does not depend on hash iteration: two nodes replaying one log must
    // read the same sequence.
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
    // What the artifact already holds is not a join: a row restating a membership the store
    // carries would otherwise append a record that changes nothing and pins the log at it, since
    // `growth_record` drops an empty set.
    //
    // Read against the store before the commit's artifact records are applied, the only moment the
    // difference exists.
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

/// The mint grouping both write doors share: the artifacts their resolved memberships named and no
/// artifact holds, one per `(layer, level, key)`, each carrying the entities that named it and the
/// index of the source that named it first.
///
/// Two sources naming one unknown key mint once and both join it.
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
                .entry((join.layer.clone(), join.level, join.key.clone()))
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

/// The growth records one closed window owes, in the order they are to be appended.
///
/// What this door adds to [`grouped_growth`]: the entities are `entity_ids[row]`, the assignment
/// this window has just made, and the index each record carries is the entry to blame if its append
/// fails.
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
/// has neither, which is every window naming no layer.
///
/// Minting happens here, at the close, not at admission: an ordinal is claimed from the level's own
/// cursor and is durable only in the record that claims it. The plan is re-resolved against
/// `ArtifactStore::ordinal_of_key` in [`Executor::prepare_mints`] in case a publication landed
/// since admission, in which case it grows instead of minting.
///
/// What this door adds to [`grouped_mints`]: the entities are `entity_ids[row]`, the index is the
/// entry that first named the key, and the edges travel with the plan, because the close settles
/// both. An edge whose child this window mints travels on the publication that creates it, and an
/// edge whose child exists without a parent is filled onto it, so a window with edges and nothing
/// to mint still has work here.
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

/// What one commit is about to mint: `(layer, level, key)` → the source that first named it, and
/// the entities joining it.
pub(super) type MintPlan = std::collections::BTreeMap<(String, u32, String), (usize, croaring::Bitmap)>;

/// A key that acquired an artifact between its resolution and its preparation grows into it rather
/// than minting a second one.
///
/// [`Executor::prepare_mints`] re-resolves every key against the store and answers the ones that
/// turned out held; this writes those ordinals back onto the memberships, so `growth_records`
/// carries them as ordinary joins. A membership left with no ordinal is one the preparation is
/// about to mint. Only the ingest door can find something here, since a window can stay open across
/// a `PublishArtifacts` command between admission and close; at the values door `resolved` is
/// always empty.
pub(super) fn settle_resolved_ordinals(
    memberships: &mut [tessera_lifecycle::ResolvedMembership],
    resolved: &std::collections::BTreeMap<(String, u32, String), u32>,
) {
    if resolved.is_empty() {
        return;
    }
    for join in memberships.iter_mut() {
        if join.ordinal.is_some() {
            continue;
        }
        let at = (join.layer.clone(), join.level, join.key.clone());
        join.ordinal = resolved.get(&at).copied();
    }
}

/// What [`Executor::prepare_mints`] answers.
pub(super) struct PreparedMints {
    /// The publication records to append, in that order.
    pub(super) records: Vec<WalRecord>,
    /// The keys that turned out to be held after all, and the ordinal each resolved to; their
    /// caller grows into those rather than minting.
    pub(super) resolved: std::collections::BTreeMap<(String, u32, String), u32>,
    /// The keys this run created.
    pub(super) minted: std::collections::BTreeSet<(String, u32, String)>,
}

/// The parent each child in these edges is named under, refusing a child named under two.
///
/// The child is keyed by its own level, which a levelled taxonomy needs: one key legitimately sits
/// at two levels and carries a different parent at each.
pub(super) fn parent_of_each_child(
    edges: &[tessera_lifecycle::BatchEdge],
) -> Result<std::collections::BTreeMap<(&str, u32, &str), &str>, String> {
    let mut claimed: std::collections::BTreeMap<(&str, u32, &str), &str> = Default::default();
    for edge in edges {
        let at = (edge.layer.as_str(), edge.level, edge.child.as_str());
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
    /// Resolve one batch's membership keys, and decide the edges its adjacency declared.
    ///
    /// Returns the memberships, each carrying the ordinal it resolved to or `None` where an open
    /// layer will mint it at the close, and the edges the close has to settle: the ones whose child
    /// it is about to mint, and the ones whose child exists and holds no parent. `Err` is the refusal
    /// text the caller is answered with, whole batch without effect.
    ///
    /// The memberships resolve first: neither a child nor a parent can be minted without appearing
    /// in the resolved set the edge checks read.
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
                        .resolve_or_mint(&join.layer, join.level, &join.key, store)
                        .map(|ordinal| tessera_lifecycle::ResolvedMembership {
                            layer: join.layer.clone(),
                            level: join.level,
                            key: join.key.clone(),
                            ordinal,
                            rows: join.rows.clone(),
                        })
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<_, String>>()?;
            // Two indexes of one set: a child's level is the edge's own, asked precisely; a
            // parent's is asked of the layer. A levelled taxonomy legitimately carries one key at
            // two levels, and one index would treat minting at one as minting at both.
            let minting: std::collections::BTreeSet<(&str, u32, &str)> = memberships
                .iter()
                .filter(|m| m.ordinal.is_none())
                .map(|m| (m.layer.as_str(), m.level, m.key.as_str()))
                .collect();
            let anywhere: std::collections::BTreeSet<(&str, &str)> = minting
                .iter()
                .map(|(layer, _, key)| (*layer, *key))
                .collect();

            // Only a `nested` or `tiered` list column declares edges; a `dag` layer's several
            // parents arrive on its artifact rows' `parent` list by the publish route, never here.
            parent_of_each_child(&artifacts.edges)?;

            let mut settling = Vec::new();
            for edge in &artifacts.edges {
                let layer = edge.layer.as_str();
                match registry.check_edge(
                    edge,
                    store,
                    minting.contains(&(layer, edge.level, edge.child.as_str())),
                    &|key| anywhere.contains(&(layer, key)),
                ) {
                    // The layer already holds this edge, so there is nothing for the close to do.
                    Ok(tessera_lifecycle::EdgeCheck::Agrees) => {}
                    // Carried to the close, where the ordinals are claimed: on the publication that
                    // creates the child, or as a fill on the child that exists without a parent.
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

    /// The artifacts this window's values named and nothing holds: one per
    /// `membership = { attribute = f }` layer whose column carried a value the level has no
    /// artifact for. Uses the same key rule as a build's mint
    /// (`tessera_types::layer::attribute_value_key`). Runs after the vocabulary mint: a novel
    /// category key is a string in the row until that pass draws it a code.
    ///
    /// A suppressed value's key still resolves and mints nothing, since
    /// [`ArtifactStore::ordinal_of_key`] loses a key only when the fold retires the artifact's own
    /// entity; a deleted one does mint again, since the new artifact is a new entity.
    pub(super) fn derive_records(
        &mut self,
        closed: &[tessera_lifecycle::ClosedEntry<Reply<Ingested>>],
        vocabularies: &Vocabularies,
    ) -> Result<Vec<WalRecord>, String> {
        use tessera_types::layer::attribute_value_key;

        // Which declared scalar each predicate layer reads, resolved once.
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
            // The codes this window's rows carry are resolved one at a time against the minter's
            // own reverse map; building `code → key` over the whole vocabulary would be a pass
            // over every binding the layer's column could name, per window.
            let minter = vocabulary
                .as_deref()
                .and_then(|name| vocabularies.get(name));
            let mut wanted: std::collections::BTreeSet<String> = Default::default();
            for entry in closed {
                for row in entry.rows() {
                    let Some(code) = row.scalars.get(index).and_then(scalar_code) else {
                        continue;
                    };
                    // Code 0 is a category code space's reserved *absent* sentinel and names no
                    // value; a plain integer column has no such reservation.
                    if vocabulary.is_some() && code == tessera_store::vocabulary::ABSENT_CODE {
                        continue;
                    }
                    wanted.insert(attribute_value_key(
                        code,
                        minter.and_then(|minter| minter.key_of(code)),
                    ));
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let prepared = self.live.with_publication_state(|registry, store, alloc| {
                let fresh: Vec<String> = wanted
                    .iter()
                    // A predicate layer is entity-scoped, so the key sits in the one set.
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
    /// holds. See [`mint_plan`] for what is minted and why it is minted here.
    ///
    /// Patches the memberships whose key resolved since admission to the ordinal it resolved to, so
    /// they grow rather than mint; leaves a minted key's ordinal `None`, which is what tells
    /// [`growth_records`] the publication carried the join. `Err` is the refusal text every waiter
    /// is answered with: everything a single batch can be refused for alone was refused at admission.
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

        // A key that acquired an artifact between its batch's admission and this close is an
        // ordinary growth. See [`settle_resolved_ordinals`].
        for entry in closed.iter_mut() {
            settle_resolved_ordinals(&mut entry.memberships, &resolved);
        }
        for ((layer, level, key), (index, _)) in &wanted {
            if minted.contains(&(layer.clone(), *level, key.clone())) {
                minted_per_entry[*index] += 1;
            }
        }
        Ok((records, minted_per_entry))
    }

    /// Prepare one set of mints: the publications that create the artifacts a caller's keys named
    /// and no artifact holds.
    ///
    /// One implementation across the doors: `/control/ingest` reaches it through
    /// [`Executor::mint_records`] and `POST /control/values` through `values_mint_plan`, so a key
    /// arriving at either door creates the same artifact, with the same lineage and refusals.
    ///
    /// The records come back in the order they are appended: ascending level, coarse first. `Err`
    /// is the refusal text the caller's waiters are answered with.
    pub(super) fn prepare_mints(
        &self,
        wanted: &MintPlan,
        edges: &[tessera_lifecycle::BatchEdge],
    ) -> Result<PreparedMints, String> {
        use std::collections::BTreeMap;
        self.live.with_publication_state(|registry, store, alloc| {
            // Checked before anything is prepared, so a refusal spends nothing.
            let parents = parent_of_each_child(edges)?;
            // Re-resolved here, not trusted from admission: a publication may execute between an
            // admission and this close.
            let mut resolved: BTreeMap<(String, u32, String), u32> = BTreeMap::new();
            let mut to_mint: BTreeMap<(&str, u32), Vec<(&str, &croaring::Bitmap)>> =
                BTreeMap::new();
            for ((layer, level, key), (_, members)) in wanted {
                // The ingest route carries no artifact view, so the key sits in the one set.
                match store.ordinal_of_key(layer, *level, None, key) {
                    Some(ordinal) => {
                        resolved.insert((layer.clone(), *level, key.clone()), ordinal);
                    }
                    None => to_mint
                        .entry((layer.as_str(), *level))
                        .or_default()
                        .push((key.as_str(), members)),
                }
            }

            // Ascending level, one record each, coarse first: a tiered chain's parent sits one
            // level up and is fixed by the record before this one.
            let mut assigned: BTreeMap<(&str, u32, &str), u32> = BTreeMap::new();
            let mut records = Vec::new();
            for ((layer, level), keys) in &to_mint {
                let incoming: Vec<tessera_lifecycle::IncomingArtifact> = keys
                    .iter()
                    .map(|(key, members)| tessera_lifecycle::IncomingArtifact {
                        key: Some((*key).to_string()),
                        view: None,
                        members: (*members).clone(),
                        excluding: None,
                        contents: Vec::new(),
                        attached_to: None,
                        parent_keys: parents
                            .get(&(*layer, *level, *key))
                            .map(|parent| vec![(*parent).to_string()])
                            .unwrap_or_default(),
                        shape: None,
                    })
                    .collect();
                // One level up and no further: entry k of a list is the parent of entry k+1.
                let pending = |key: &str| {
                    let coarser = level.checked_sub(1)?;
                    assigned.get(&(*layer, coarser, key)).map(|ordinal| {
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
                // Read back off the record, not recomputed: it is what this record actually claimed.
                for ((key, _), artifact) in keys.iter().zip(artifacts) {
                    debug_assert_eq!(artifact.key.as_deref(), Some(*key));
                    assigned.insert((*layer, *level, key), artifact.ordinal);
                }
                records.push(record);
            }

            // The edges whose child was not minted above: it exists and holds no parent, so the
            // edge is a fill on it. Behind the publications, so a parent this window minted has an
            // ordinal by the time the fill resolves it.
            //
            // The cycle walk reads the layer's held edges and `window_edges` as one graph, so
            // `window_edges` is seeded with the edges the publications above are about to create:
            // nothing prepared here is in the store yet, and a mint under an existing artifact plus
            // a fill on that artifact naming the mint is a cycle neither half sees alone.
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
                if assigned.contains_key(&(edge.layer.as_str(), edge.level, edge.child.as_str())) {
                    continue;
                }
                let pending = |key: &str| {
                    assigned
                        .iter()
                        .find(|((layer, _, held), _)| *layer == edge.layer && *held == key)
                        .map(|((_, level, _), ordinal)| tessera_lifecycle::wal::ParentRef {
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
                .map(|(layer, level, key)| ((*layer).to_string(), *level, (*key).to_string()))
                .collect();
            Ok(PreparedMints {
                records,
                resolved,
                minted,
            })
        })
    }

    /// Hold one accepted write's delta until the tick. Every route that changes a level's records
    /// arrives here with the level version it followed, and the level's row forms take the run of
    /// them at the next tick.
    ///
    /// `refused` names the growth entries the store did not take; they are held for nothing. A
    /// record whose every entry was refused is still held, empty, since the versions of an
    /// interval's deltas must stay consecutive.
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
                            // A leave, or a withdrawal, re-derives the operator whole: a union
                            // cannot express a leave.
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
    /// made for the tick that brings the level's row forms forward. Each record moves its level's
    /// version by one, so the version a delta starts from walks with the records.
    pub(super) fn apply_artifact_records(&mut self, records: &[&WalRecord], positions: &[u64]) {
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
        self.side_manifests.behind_live = true;
    }
}
