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
    pub(super) store: &'a mosaica_lifecycle::ArtifactStore,
    pub(super) restating: Option<&'a mosaica_lifecycle::ShardSets>,
}

/// The items of a window's join rows and of the items it places in artifacts in place, by shard:
/// the only rows that can restate a membership.
pub(super) fn joining_items<W, S>(
    closed: &[mosaica_lifecycle::ClosedEntry<W, S>],
) -> mosaica_lifecycle::ShardSets {
    mosaica_lifecycle::ShardSets::of_items(closed.iter().flat_map(|entry| {
        let rows = entry.rows();
        let joined = &entry.items[rows.len() + entry.edits().len()..];
        rows.iter()
            .zip(&entry.items)
            .filter(|(row, _)| row.join)
            .map(|(_, item)| *item)
            .chain(joined.iter().copied())
    }))
}

/// The item one membership's row index names, and `None` for a row the source does not carry.
pub(super) type ItemOfRow<'a> = &'a dyn Fn(usize, u32) -> Option<mosaica_types::ItemAt>;

fn row_not_carried(layer: &str, row: u32) -> String {
    format!("column '{layer}' names row {row}, which this batch does not carry")
}

/// The joins both write doors' resolved memberships declared, as one set per shard of each
/// `(layer, level, ordinal)` less what the artifact already holds. A key with no ordinal was minted
/// at this commit.
pub(super) fn grouped_growth<'a>(
    sources: impl Iterator<Item = &'a [mosaica_lifecycle::ResolvedMembership]>,
    item_of: ItemOfRow<'_>,
    held: Option<HeldMembers<'_>>,
) -> Result<Vec<(WalRecord, usize)>, String> {
    use std::collections::BTreeMap;
    /// One `(layer, level)`'s joins: the source to blame for the append, the layer's space, and
    /// the items joining each ordinal.
    type Level<T> = (usize, LayerSpace, BTreeMap<u32, T>);
    // Ordered, so replay does not depend on hash iteration.
    let mut items: BTreeMap<(&str, u32), Level<Vec<mosaica_types::ItemAt>>> = BTreeMap::new();
    for (index, memberships) in sources.enumerate() {
        for join in memberships {
            let Some(ordinal) = join.ordinal else {
                continue;
            };
            let (_, _, ordinals) = items
                .entry((join.layer.as_str(), join.level))
                .or_insert_with(|| (index, join.space, BTreeMap::new()));
            let joining = ordinals.entry(ordinal).or_default();
            for row in &join.rows {
                joining
                    .push(item_of(index, *row).ok_or_else(|| row_not_carried(&join.layer, *row))?);
            }
        }
    }
    let mut by_level: BTreeMap<(&str, u32), Level<mosaica_lifecycle::ShardSets>> = items
        .into_iter()
        .map(|(at, (index, space, ordinals))| {
            let ordinals = ordinals
                .into_iter()
                .map(|(ordinal, joining)| {
                    (ordinal, mosaica_lifecycle::ShardSets::of_items(joining))
                })
                .collect();
            (at, (index, space, ordinals))
        })
        .collect();
    // What the artifact already holds is not a join, or a restating row would append a record
    // that changes nothing and pins the log. Read against the store before this commit applies.
    if let Some(held) = held {
        for ((layer, level), (_, _, ordinals)) in &mut by_level {
            for (ordinal, joining) in ordinals.iter_mut() {
                let _ = joining.try_for_each_mut(|shard, set| {
                    let restates = held
                        .restating
                        .is_none_or(|rows| rows.get(shard).is_some_and(|rows| set.intersect(rows)));
                    let record = held
                        .store
                        .slice(shard)
                        .and_then(|slice| slice.get(layer, *level, *ordinal));
                    if let Some(record) = record.filter(|_| restates) {
                        record.members.remove_from(set);
                    }
                    Ok::<(), std::convert::Infallible>(())
                });
            }
        }
    }
    Ok(by_level
        .into_iter()
        .filter_map(|((layer, level), (index, space, ordinals))| {
            let joins = ordinals
                .iter()
                .map(|(ordinal, joining)| (*ordinal, joining));
            mosaica_lifecycle::membership::growth_record(layer, space, level, joins)
                .map(|record| (record, index))
        })
        .collect())
}

/// How many memberships each entry adds: an item joining an artifact that did not hold it,
/// counted for the first entry naming the pair. A key the close mints holds nothing yet. `store`
/// is read only where a row can restate a membership, which `restating` says; each item is asked
/// of its own shard's slice.
pub(super) fn joined_per_entry<W, S>(
    closed: &[mosaica_lifecycle::ClosedEntry<W, S>],
    store: Option<&mosaica_lifecycle::ArtifactStore>,
) -> Vec<u64> {
    use std::collections::BTreeMap;
    type Joined<'a> = (&'a str, u32, Option<&'a str>, &'a str, ShardId);
    let mut seen: BTreeMap<Joined<'_>, croaring::Bitmap> = BTreeMap::new();
    closed
        .iter()
        .map(|entry| {
            let mut joined = 0u64;
            for join in &entry.memberships {
                // Each shard's members of the artifact, read once for the join.
                let mut held: Vec<(ShardId, Option<&mosaica_lifecycle::membership::Members>)> =
                    Vec::new();
                for row in &join.rows {
                    let Some(item) = entry.items.get(*row as usize) else {
                        continue;
                    };
                    // Entity space is `u32`-wide, so the narrowing is total.
                    let entity = item.entity.raw() as u32;
                    let members = match held.iter().find(|(shard, _)| *shard == item.shard) {
                        Some((_, members)) => *members,
                        None => {
                            let members = join.ordinal.zip(store).and_then(|(ordinal, store)| {
                                let slice = store.slice(item.shard)?;
                                let record = slice.get(&join.layer, join.level, ordinal)?;
                                Some(slice.members_of(record))
                            });
                            held.push((item.shard, members));
                            members
                        }
                    };
                    if members.is_some_and(|members| members.contains(entity)) {
                        continue;
                    }
                    let at = (
                        join.layer.as_str(),
                        join.level,
                        join.view.as_deref(),
                        join.key.as_str(),
                        item.shard,
                    );
                    if seen.entry(at).or_default().add_checked(entity) {
                        joined += 1;
                    }
                }
            }
            joined
        })
        .collect()
}

/// The artifacts both write doors' resolved memberships named and no artifact holds, one per
/// `(layer, level, key)`. Two sources naming one unknown key mint once and both join it.
pub(super) fn grouped_mints<'a>(
    sources: impl Iterator<Item = &'a [mosaica_lifecycle::ResolvedMembership]>,
    item_of: ItemOfRow<'_>,
) -> Result<MintPlan, String> {
    let mut wanted: std::collections::BTreeMap<MintKey, (usize, Vec<mosaica_types::ItemAt>)> =
        std::collections::BTreeMap::new();
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
                .or_insert_with(|| (index, Vec::new()));
            for row in &join.rows {
                members
                    .push(item_of(index, *row).ok_or_else(|| row_not_carried(&join.layer, *row))?);
            }
        }
    }
    Ok(wanted
        .into_iter()
        .map(|(key, (index, members))| {
            (
                key,
                (index, mosaica_lifecycle::ShardSets::of_items(members)),
            )
        })
        .collect())
}

/// The growth records one closed window owes, in the order to append them. The entities are
/// `items[row]`, the assignment this window just made.
pub(super) fn growth_records<W, S>(
    closed: &[mosaica_lifecycle::ClosedEntry<W, S>],
    held: Option<HeldMembers<'_>>,
) -> Result<Vec<(WalRecord, usize)>, String> {
    grouped_growth(
        closed.iter().map(|entry| entry.memberships.as_slice()),
        &|entry, row| closed[entry].items.get(row as usize).copied(),
        held,
    )
}

/// What a closed window is about to mint, and the edges its close has to settle. `None` where it
/// has neither. Minting happens at the close, not admission, so [`Executor::prepare_mints`]
/// re-resolves the plan against the store in case a publication landed since.
pub(super) fn mint_plan<W, S>(
    closed: &[mosaica_lifecycle::ClosedEntry<W, S>],
) -> Result<Option<(MintPlan, Vec<mosaica_lifecycle::BatchEdge>)>, String> {
    let wanted = grouped_mints(
        closed.iter().map(|entry| entry.memberships.as_slice()),
        &|entry, row| closed[entry].items.get(row as usize).copied(),
    )?;
    let edges: Vec<mosaica_lifecycle::BatchEdge> =
        closed.iter().flat_map(|e| e.edges.iter().cloned()).collect();
    if wanted.is_empty() && edges.is_empty() {
        return Ok(None);
    }
    Ok(Some((wanted, edges)))
}

/// An artifact a key names: `(layer, level, view, key)`, the view `None` on an entity-scoped layer.
pub(super) type MintKey = (String, u32, Option<String>, String);

/// What one commit is about to mint: each key to the source that first named it, and the items
/// joining it.
pub(super) type MintPlan =
    std::collections::BTreeMap<MintKey, (usize, mosaica_lifecycle::ShardSets)>;

/// A key that acquired an artifact between its resolution and its preparation grows into it
/// rather than minting a second one: writes the ordinals found back onto the memberships, so
/// `growth_records` carries them as ordinary joins.
pub(super) fn settle_resolved_ordinals(
    memberships: &mut [mosaica_lifecycle::ResolvedMembership],
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
    edges: &[mosaica_lifecycle::BatchEdge],
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
    /// Resolve one batch's membership keys, and decide the edges its adjacency declared, as the
    /// writer of `scope` may name them. `Err` refuses the whole batch without effect.
    pub(super) fn resolve_memberships(
        &self,
        artifacts: &mosaica_lifecycle::BatchArtifacts,
        scope: &WriteScope,
    ) -> Result<
        (
            Vec<mosaica_lifecycle::ResolvedMembership>,
            Vec<mosaica_lifecycle::BatchEdge>,
        ),
        String,
    > {
        if artifacts.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let generation = self.generation.load_full();
        let resolver = self.resolver(&generation, scope);
        self.live.with_publication_state(|registry, store| {
            let reach = crate::resolver::ArtifactReach::new(&resolver, registry, store);
            let memberships: Vec<mosaica_lifecycle::ResolvedMembership> = artifacts
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
                            &reach,
                        )
                        .map(|ordinal| mosaica_lifecycle::ResolvedMembership {
                            layer: join.layer.clone(),
                            space: registry
                                .get(&join.layer)
                                .expect("a key resolved in a layer that is registered")
                                .space,
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
                    &reach,
                ) {
                    Ok(mosaica_lifecycle::EdgeCheck::Agrees) => {}
                    // Carried to the close: on the publication that creates the child, or as a
                    // fill on a child that exists without a parent.
                    Ok(
                        mosaica_lifecycle::EdgeCheck::Mints
                        | mosaica_lifecycle::EdgeCheck::Records,
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
        use mosaica_types::layer::attribute_value_key;

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
                if vocabulary.is_some() && code == mosaica_store::vocabulary::ABSENT_CODE {
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
            let prepared = self.live.with_publication_state(|registry, store| {
                let fresh: Vec<String> = wanted
                    .iter()
                    .filter(|key| store.ordinal_of_key(&layer, 0, None, key).is_none())
                    .cloned()
                    .collect();
                if fresh.is_empty() {
                    return Ok(None);
                }
                registry
                    .prepare_derive(&layer, 0, &fresh, store)
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
        closed: &mut [mosaica_lifecycle::ClosedEntry<IngestReply, WriteScope>],
    ) -> Result<(Vec<WalRecord>, Vec<u64>), String> {
        let mut minted_per_entry = vec![0u64; closed.len()];
        let Some((wanted, edges)) = mint_plan(closed)? else {
            return Ok((Vec::new(), minted_per_entry));
        };
        // Each writer's scope once, however many of its batches the window holds.
        let mut scopes: Vec<&WriteScope> = Vec::new();
        for entry in closed.iter() {
            if !scopes.iter().any(|held| held.is(&entry.scope)) {
                scopes.push(&entry.scope);
            }
        }
        let PreparedMints {
            records,
            resolved,
            minted,
        } = self.prepare_mints(&wanted, &edges, &scopes)?;

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
    ///
    /// The keys and parents are resolved as every writer of `scopes` may name them, by the rule
    /// admission resolved them by: a key an artifact outside one of their reaches has come to hold
    /// since refuses the window, whose batches then close one at a time.
    pub(super) fn prepare_mints(
        &self,
        wanted: &MintPlan,
        edges: &[mosaica_lifecycle::BatchEdge],
        scopes: &[&WriteScope],
    ) -> Result<PreparedMints, String> {
        use std::collections::BTreeMap;
        let served = self.generation.load_full();
        let views = crate::write::ServedViews(&served.bundle.manifest, None);
        let resolvers: Vec<crate::resolver::Resolver<'_>> = scopes
            .iter()
            .filter(|scope| matches!(scope, WriteScope::Terms(..)))
            .map(|scope| self.resolver(&served, scope))
            .collect();
        self.live.with_publication_state(|registry, store| {
            let reach = crate::resolver::ReachOfAll(
                resolvers
                    .iter()
                    .map(|resolver| crate::resolver::ArtifactReach::new(resolver, registry, store))
                    .collect(),
            );
            let parents = parent_of_each_child(edges)?;
            // Re-resolved here, not trusted from admission, since a publication may land between.
            let mut resolved: BTreeMap<MintKey, u32> = BTreeMap::new();
            // One publication per view, since a parent is resolved inside its child's view.
            type Minting<'a> = Vec<(&'a str, &'a mosaica_lifecycle::ShardSets)>;
            let mut to_mint: BTreeMap<(&str, u32, Option<&str>), Minting<'_>> = BTreeMap::new();
            for (at, (_, members)) in wanted {
                let (layer, level, view, key) = at;
                match registry
                    .resolve_or_mint(layer, *level, view.as_deref(), key, store, &reach)
                    .map_err(|e| e.to_string())?
                {
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
                let incoming: Vec<mosaica_lifecycle::IncomingArtifact> = keys
                    .iter()
                    .map(|(key, members)| mosaica_lifecycle::IncomingArtifact {
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
                        // A key column states no label; admission refused a layer that reads them.
                        access: None,
                    })
                    .collect();
                let pending = |key: &str| {
                    let coarser = level.checked_sub(1)?;
                    assigned.get(&(*layer, coarser, *view, key)).map(|ordinal| {
                        mosaica_lifecycle::wal::ParentRef {
                            level: coarser,
                            ordinal: *ordinal,
                        }
                    })
                };
                let record = registry
                    .prepare_publish(layer, *level, &incoming, store, &pending, &views, &reach)
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
                BTreeMap<mosaica_lifecycle::wal::ParentRef, Vec<mosaica_lifecycle::wal::ParentRef>>,
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
                        mosaica_lifecycle::wal::ParentRef {
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
                        .map(|((_, level, _, _), ordinal)| mosaica_lifecycle::wal::ParentRef {
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
                        &reach,
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

    /// Hold one accepted write's delta in `shard` until the tick, when the level's row forms in that
    /// shard take the run of them: the record's part in `shard`, and every ordinal it published or
    /// filled, whose records every shard's form reads. `refused` names the growth entries the store
    /// did not take; a record whose every entry was refused is still held, empty, since a form's
    /// delta versions must stay consecutive.
    pub(super) fn hold_delta(
        &mut self,
        record: &WalRecord,
        shard: ShardId,
        before: u64,
        refused: &[usize],
    ) {
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
                ..
            } => {
                let part = |sets: &[mosaica_lifecycle::wal::WalShardSet]| {
                    mosaica_lifecycle::ShardSets::from_wal(sets)
                        .map(|sets| sets.get(shard).cloned().unwrap_or_default())
                };
                let mut joins = Vec::new();
                let mut pages = Vec::new();
                for (index, grown) in growth.iter().enumerate() {
                    if refused.contains(&index) {
                        continue;
                    }
                    let Some(joining) = part(&grown.joining) else {
                        continue;
                    };
                    match grown.set {
                        mosaica_lifecycle::wal::GrownSet::Membership => {
                            if !joining.is_empty() {
                                joins.push((grown.ordinal, joining));
                            }
                        }
                        mosaica_lifecycle::wal::GrownSet::GeneratingSet { rank, cardinality } => {
                            // A leave in any shard re-derives the operator whole: a union cannot
                            // express one.
                            let leaves = !grown.leaving.is_empty();
                            pages.push(crate::artifacts::SetPage {
                                ordinal: grown.ordinal,
                                position: rank,
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
            .entry((layer.clone(), level, shard))
            .or_default()
            .push(crate::artifacts::LevelDelta { before, kind });
    }

    /// Applies durable artifact records to the registry and the store, and holds the delta each
    /// made in every shard whose form of the level it moved, from the version that shard's form was
    /// at before it. `publish` says only when the growth reaches a side-manifest.
    pub(super) fn apply_artifact_records(
        &mut self,
        records: &[&WalRecord],
        positions: &[u64],
        publish: Publish,
    ) {
        if records.is_empty() {
            return;
        }
        let mut refused_per_record: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
        let mut moved_per_record: Vec<Vec<(ShardId, u64)>> = vec![Vec::new(); records.len()];
        let mut settled: Vec<(String, u32)> = Vec::new();
        let undecodable = self.live.with_publication_state(|registry, store| {
            let versions = |store: &mosaica_lifecycle::ArtifactStore, layer: &str, level: u32| {
                store
                    .shards()
                    .filter_map(|shard| {
                        Some((shard, store.slice(shard)?.level_version(layer, level)))
                    })
                    .collect::<Vec<_>>()
            };
            records
                .iter()
                .zip(positions)
                .zip(refused_per_record.iter_mut().zip(&mut moved_per_record))
                .map(|((record, position), (refused, moved))| {
                    let at = artifact_level_of(record);
                    let before = at.map(|(layer, level)| versions(store, layer, level));
                    registry.apply(record);
                    settled.extend(registry.settle_layout(store, record));
                    let undecodable = store.apply_reporting(record, *position, refused);
                    if let (Some((layer, level)), Some(before)) = (at, before) {
                        let after = versions(store, layer, level);
                        moved.extend(before.into_iter().filter(|held| !after.contains(held)));
                    }
                    undecodable
                })
                .sum::<usize>()
        });
        // A form held in the layout a level had before is never asked for again.
        for (layer, level) in &settled {
            self.deps.artifact_projections.forget_level(layer, *level);
        }
        if undecodable > 0 {
            tracing::error!(
                count = undecodable,
                "ALARM: artifact records did not survive their own round trip; those artifacts are absent"
            );
        }
        for ((record, moved), refused) in records
            .iter()
            .zip(moved_per_record)
            .zip(&refused_per_record)
        {
            for (shard, before) in moved {
                self.hold_delta(record, shard, before, refused);
            }
        }
        // Durable in the log and not yet in a manifest.
        match publish {
            Publish::Promptly => self.side_manifests.behind_live = true,
            Publish::AtTick => self.side_manifests.growth_unpublished = true,
        }
    }
}
