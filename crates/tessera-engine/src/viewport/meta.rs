//! `GET /v1/meta`: the bundle facts a client needs, and resolving a name against them.

use super::*;

/// One declared view, as `GET /v1/meta` publishes it. A projection and an extent belong to a
/// view, and two views of one bundle may be projected and quantised differently; the server
/// derives `tile` from them here rather than at the wire.
#[derive(Debug, Clone)]
pub struct MetaView {
    pub id: String,
    pub display_name: String,
    /// The frame every position in this view is quantised against, immutable for the view's
    /// life: what a client decodes a Morton prefix with, and what the write path checks a
    /// coordinate against.
    pub quantisation: Quantisation,
    /// What placed every position in this view before the frame did, or [`Projection::None`]
    /// for a view that projects nothing.
    pub projection: Projection,
    /// The tile this view's frame is, in the scheme that addresses it, or `None` where no
    /// published scheme does. One field because neither the scheme nor the address is meaningful
    /// alone.
    pub tile: Option<TileAddress>,
    /// Where this view sits in its group's roster, or `None` for a plain view.
    pub roster: Option<MetaRoster>,
    /// The declaration's `point_visibility.default`, or `None` where it named none. Read by
    /// `/control/ingest` to fill a row whose `access` list is empty, or to refuse the batch. Not
    /// published on the wire.
    pub point_default: Option<String>,
}

/// One view's roster record, as `GET /v1/meta` publishes it beside the view. The key is the
/// caller's own and is a view's only address. Order is the list's own order — creation order —
/// so a client offers previous-and-next by walking the group's `views` array.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaRoster {
    pub group: String,
    pub key: String,
    /// Typed, one entry per name the owning group declared. Empty on a `members` group's views,
    /// whose metadata belongs to the owner.
    pub metadata: BTreeMap<String, ViewMetadataValue>,
}

/// One view group, as `GET /v1/meta` publishes it: the name and its views in creation order. A
/// group is not a view — it cannot be named on a viewer verb and has no row space.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaGroup {
    pub name: String,
    /// The group's declared title, `None` where it declared none.
    pub title: Option<String>,
    /// The group whose keys these are, where this group declares `members`; `None` where it owns
    /// them.
    pub members_of: Option<String>,
    /// This group's view ids — the joined `group:key` form a request names — in creation order.
    pub views: Vec<String>,
}

/// The tile a view's frame corresponds to, and the scheme it is a tile of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileAddress {
    /// The tile scheme's name — [`tessera_spatial::frame::XYZ`] is the only one this system can
    /// name.
    pub scheme: &'static str,
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

/// `GET /v1/meta`'s payload: the bundle-level facts a viewer client needs before it can issue a
/// sensible `/v1/viewport` call.
#[derive(Debug, Clone)]
pub struct EngineMeta {
    pub api_version: u32,
    pub bundle_format: u32,
    /// The declared views, in serving order: the plain views in manifest order, then each
    /// group's views in creation order. There is no bundle-level extent: a caller with no view
    /// id is asking a question the bundle cannot answer.
    pub views: Vec<MetaView>,
    /// The view groups, in manifest order, each listing its views in creation order.
    pub groups: Vec<MetaGroup>,
    pub declared_scalars: Vec<DeclaredScalar>,
    /// Where each of `declared_scalars`' values is read from, positionally.
    pub homes: Vec<crate::filter::FieldHomes>,
    /// The group-scoped attribute column families, flattened over the groups in manifest order:
    /// one entry per family, naming the group whose views it has a column per and the view ids
    /// that have one.
    pub scoped_scalars: Vec<tessera_store::manifest::ScopedScalar>,
    /// The live category bindings, from the same generation as `declared_scalars`. Ingest
    /// resolves keys through this and never mints: two handlers racing one novel key would
    /// otherwise draw two codes for it and split its rows between them.
    pub vocabularies: Arc<Vocabularies>,
    /// The idset. `GET /v1/meta` reports this verbatim; `POST /v1/items/{tessera_id}` compares a
    /// caller-supplied idset against it. Never the identity key — that never leaves the server.
    pub idset: u32,
}

/// The ownership rule: the key `view` holds in `group`'s roster, given the roster record `view`
/// carries — `(its own group, its key)` — and a lookup for what a group declares `members` of.
/// A view of `group` holds its own key; a view of a group declaring `members` of `group` holds
/// the same key; anything else holds none.
pub(crate) fn owning_key_of<'a>(
    roster: (&'a str, &'a str),
    members_of: impl FnOnce(&str) -> Option<&'a str>,
    group: &str,
) -> Option<&'a str> {
    let (own_group, key) = roster;
    if own_group == group {
        return Some(key);
    }
    (members_of(own_group)? == group).then_some(key)
}

/// What a filter leaf's column spelling resolves to — see [`EngineMeta::resolve_filter_column`].
/// Four outcomes, three of them refusals with different codes: an ambiguous leaf is a `422`, and
/// a pin naming nothing is the `404` an unknown view already gets, indistinguishably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafColumn {
    /// Not a filterable column under any spelling: the ordinary unknown-column refusal.
    Unknown,
    /// The column the engine evaluates and the family its values are read by.
    Resolved {
        column: String,
        family: crate::filter::Family,
    },
    /// A group-scoped attribute named bare under a view that decides no column of its family.
    Unpinned { group: String },
    /// A pin naming no view of the attribute's group — an undeclared key, or a view with no
    /// column.
    UnknownPin { group: String, pin: String },
    /// A pin on a column that has no scope.
    PinOnUnscoped { column: String },
}

/// What [`EngineMeta::resolve_column`] finds: [`LeafColumn`]'s outcomes, with the declaration a
/// resolved column came from.
pub(crate) enum Resolution<'a> {
    /// An entity-scoped column, by its position in [`EngineMeta::declared_scalars`].
    Declared(usize),
    /// A group-scoped family's column for one view.
    Scoped {
        family: &'a tessera_store::manifest::ScopedScalar,
        view: String,
    },
    Unknown,
    Unpinned {
        group: String,
    },
    UnknownPin {
        group: String,
        pin: String,
    },
    PinOnUnscoped {
        column: String,
    },
}

impl Resolution<'_> {
    /// The outcome in the terms the filter surface and the value-list route answer in.
    pub(crate) fn leaf_column(self, meta: &EngineMeta) -> LeafColumn {
        match self {
            Resolution::Declared(index) => {
                let declared = &meta.declared_scalars[index];
                LeafColumn::Resolved {
                    column: declared.name.clone(),
                    family: crate::filter::Family::of(declared),
                }
            }
            Resolution::Scoped { family, view } => LeafColumn::Resolved {
                column: crate::filter::scoped_column_name(&family.name, &view),
                family: crate::filter::Family::of_scoped(family),
            },
            Resolution::Unknown => LeafColumn::Unknown,
            Resolution::Unpinned { group } => LeafColumn::Unpinned { group },
            Resolution::UnknownPin { group, pin } => LeafColumn::UnknownPin { group, pin },
            Resolution::PinOnUnscoped { column } => LeafColumn::PinOnUnscoped { column },
        }
    }
}

impl EngineMeta {
    /// The view a request's id names — a plain view's name, or a group's `<group>:<key>` — or
    /// `None` for a view this bundle does not declare. One resolution for both planes: a viewer
    /// verb's `view`, `x-tessera-view` and this document's own `views` are one namespace, and the
    /// key is the only address a view has.
    pub fn resolve_view(&self, requested: &str) -> Option<&MetaView> {
        self.views.iter().find(|v| v.id == requested)
    }

    /// [`Self::resolve_view`] through the session's visible-view set — the one place a viewer
    /// verb's `view` is resolved, and the only gate check the request path makes. The
    /// set-membership probe runs whether or not a view was found, so a gate-failed name and a
    /// name nobody declared reach the caller as the same `None`, at the same cost: a probe
    /// conditional on a hit would let a timing difference tell a viewer a view exists. The set is
    /// fixed for the session's life, so a view created since is `None` until it re-authorises.
    pub fn resolve_visible_view(
        &self,
        requested: &str,
        visible: &crate::gate::VisibleViews,
    ) -> Option<&MetaView> {
        let resolved = self.resolve_view(requested);
        let probe = resolved.map_or(crate::gate::NO_SUCH_VIEW, |v| v.id.as_str());
        match visible.contains_view(probe) {
            true => resolved,
            false => None,
        }
    }

    /// Resolve a filter leaf's column spelling under the view a request names. An entity-scoped
    /// column resolves to itself and takes no pin. A group-scoped family resolves to exactly one
    /// view's column: under a view of the attribute's group or a sharing group, the request's own
    /// view decides; under any other view the leaf must pin by key (`sentiment@2026-Q3`).
    ///
    /// The scoped surface is inside the gate and collapses in one direction: for a principal
    /// whose group gate fails, the whole attribute is undeclared. Bare and pinned uses alike take
    /// [`LeafColumn::Unknown`] rather than the `Unpinned` or `UnknownPin` refusals that would
    /// confirm the group or its keys — otherwise a pinned leaf would let a principal filter their
    /// visible entities by a value from a group they cannot reach. Where the group is reachable, a
    /// pin naming an unreachable view gets the same `UnknownPin` a key no view holds gets.
    pub fn resolve_filter_column(
        &self,
        leaf: &str,
        view: &str,
        visible: &crate::gate::VisibleViews,
    ) -> LeafColumn {
        self.resolve_column(
            leaf,
            view,
            visible,
            crate::filter::is_filterable,
            crate::filter::scoped_is_filterable,
        )
        .leaf_column(self)
    }

    /// Resolve a `/v1/categories` column spelling — [`Self::resolve_filter_column`]'s question
    /// asked by the value-list route, whose admission is not the filter surface's. A category has
    /// a value list whether or not it is filterable, so an entity-scoped column is resolved by
    /// declaration alone; a group-scoped one is admitted as the filter surface admits it.
    pub fn resolve_category_column(
        &self,
        leaf: &str,
        view: &str,
        visible: &crate::gate::VisibleViews,
    ) -> LeafColumn {
        self.resolve_column(
            leaf,
            view,
            visible,
            |_| true,
            crate::filter::scoped_is_filterable,
        )
        .leaf_column(self)
    }

    /// Resolve a column spelling under `view`, over the entity-scoped columns `declared` admits
    /// and the group-scoped families `scoped` admits: the one resolution the filter surface, the
    /// value-list route and a records read share. See [`Self::resolve_filter_column`] for the
    /// rules.
    pub(crate) fn resolve_column(
        &self,
        leaf: &str,
        view: &str,
        visible: &crate::gate::VisibleViews,
        declared: impl Fn(&DeclaredScalar) -> bool,
        scoped: impl Fn(&tessera_store::manifest::ScopedScalar) -> bool,
    ) -> Resolution<'_> {
        let (name, pin) = match leaf.split_once(crate::filter::PIN) {
            Some((name, pin)) => (name, Some(pin)),
            None => (leaf, None),
        };
        // Entity-scoped columns first: a name is one or the other, never both.
        if let Some(index) = self
            .declared_scalars
            .iter()
            .position(|d| d.name == name && declared(d))
        {
            return match pin {
                None => Resolution::Declared(index),
                Some(_) => Resolution::PinOnUnscoped {
                    column: name.to_string(),
                },
            };
        }
        let Some(family) = self
            .scoped_scalars
            .iter()
            .find(|f| f.name == name && scoped(f))
        else {
            return Resolution::Unknown;
        };
        // The group's gate, ahead of the pin/bare split, so neither spelling confirms the group.
        if !visible.contains_group(&family.group) {
            return Resolution::Unknown;
        }
        let group_view = |key: &str| format!("{}{}{key}", family.group, tessera_store::GROUP_SEPARATOR);
        match pin {
            Some(pin) => match self.resolve_visible_view(&group_view(pin), visible) {
                // A view with no column reads the same as a key nobody declared or a gate-failed
                // one: the pin names nothing to read either way.
                Some(view) if family.views.contains(&view.id) => Resolution::Scoped {
                    family,
                    view: view.id.clone(),
                },
                _ => Resolution::UnknownPin {
                    group: family.group.clone(),
                    pin: pin.to_string(),
                },
            },
            // The request's own view, where it is one of the family's own group or a sharing one.
            None => match self.owning_key(view, &family.group).map(group_view) {
                Some(id) if family.views.contains(&id) => Resolution::Scoped { family, view: id },
                _ => Resolution::Unpinned {
                    group: family.group.clone(),
                },
            },
        }
    }

    /// The key `view` holds in `group`'s roster — its own if it is a view of that group, and the
    /// key it shares if its group declares `members` of it. `None` for a plain view, or a view of
    /// an unrelated group. Public because the ingest boundary asks it too.
    pub fn owning_key(&self, view: &str, group: &str) -> Option<&str> {
        let roster = self.resolve_view(view)?.roster.as_ref()?;
        owning_key_of(
            (&roster.group, &roster.key),
            |name| {
                self.groups
                    .iter()
                    .find(|g| g.name == name)?
                    .members_of
                    .as_deref()
            },
            group,
        )
    }

    /// Every view id whose row space carries one column of `family`: the owning group's own ids,
    /// and the same keys under every group declaring `members` of it. Unfiltered: the caller
    /// applies the gate.
    pub fn scoped_family_views(
        &self,
        family: &tessera_store::manifest::ScopedScalar,
    ) -> Vec<String> {
        let keys: Vec<&str> = family
            .views
            .iter()
            .filter_map(|id| {
                id.strip_prefix(family.group.as_str())?
                    .strip_prefix(tessera_store::GROUP_SEPARATOR)
            })
            .collect();
        let mut out = family.views.clone();
        for group in self
            .groups
            .iter()
            .filter(|g| g.members_of.as_deref() == Some(family.group.as_str()))
        {
            for id in &group.views {
                let holds = id
                    .strip_prefix(group.name.as_str())
                    .and_then(|rest| rest.strip_prefix(tessera_store::GROUP_SEPARATOR))
                    .is_some_and(|key| keys.contains(&key));
                if holds {
                    out.push(id.clone());
                }
            }
        }
        out
    }

    pub fn projection_of(&self, view: &str) -> Option<Projection> {
        self.views
            .iter()
            .find(|v| v.id == view)
            .map(|v| v.projection)
    }

    /// The frame a named view's positions are quantised against, or `None` for a view this
    /// bundle does not declare. Keyed by view, never bundle-wide: reading the first declared view
    /// instead would check a region or an ingest row against the wrong frame once a bundle
    /// carries two. There is no default frame.
    pub fn quantisation_of(&self, view: &str) -> Option<Quantisation> {
        self.views
            .iter()
            .find(|v| v.id == view)
            .map(|v| v.quantisation)
    }
}

impl Engine {
    /// `GET /v1/meta`: read-only bundle facts, no session or authorisation involved. Loads the
    /// generation once, like every other request path.
    pub fn meta(&self) -> EngineMeta {
        meta_of(&self.generation.load_full())
    }
}

/// [`Engine::meta`] over a generation the caller already loaded, so a request resolves names
/// against the same generation it reads.
pub(crate) fn meta_of(generation: &Generation) -> EngineMeta {
    let manifest = &generation.bundle.manifest;
    // The tile scheme is a function of the view's projection and frame together, derived
    // here rather than at the wire, so the ingest plane cannot come to a different answer
    // about the same bundle.
    let meta_view = |s: &tessera_store::manifest::ViewDescriptor,
                     roster: Option<MetaRoster>| MetaView {
        id: s.id.clone(),
        display_name: s.display_name.clone(),
        quantisation: s.quantisation,
        projection: s.projection,
        tile: tessera_spatial::frame::tile_scheme(
            s.projection,
            &Bounds {
                x_min: s.quantisation.x_min,
                x_max: s.quantisation.x_max,
                y_min: s.quantisation.y_min,
                y_max: s.quantisation.y_max,
            },
        )
        .map(|(scheme, square)| TileAddress {
            scheme,
            z: square.z,
            x: square.x,
            y: square.y,
        }),
        roster,
        point_default: s.point_default.clone(),
    };
    // Serving order is the roster's order: plain views in manifest order, then each group's
    // views in creation order. Nothing is sorted here — the record order is the order.
    let rostered: std::collections::HashSet<String> = manifest
        .groups
        .iter()
        .flat_map(|g| g.views.iter().map(move |v| format!("{}:{}", g.name, v.key)))
        .collect();
    let mut views: Vec<MetaView> = manifest
        .views
        .iter()
        .filter(|v| !rostered.contains(&v.id))
        .map(|v| meta_view(v, None))
        .collect();
    let mut groups: Vec<MetaGroup> = Vec::with_capacity(manifest.groups.len());
    for group in &manifest.groups {
        let mut ids = Vec::with_capacity(group.views.len());
        for entry in &group.views {
            let id = format!("{}:{}", group.name, entry.key);
            // A roster entry with no declared view is refused at open, so this cannot
            // silently drop one.
            let Some(descriptor) = manifest.views.iter().find(|v| v.id == id) else {
                continue;
            };
            ids.push(id);
            views.push(meta_view(
                descriptor,
                Some(MetaRoster {
                    group: group.name.clone(),
                    key: entry.key.clone(),
                    metadata: entry.metadata.clone(),
                }),
            ));
        }
        groups.push(MetaGroup {
            name: group.name.clone(),
            title: group.title.clone(),
            members_of: group.members_of.clone(),
            views: ids,
        });
    }
    EngineMeta {
        api_version: API_VERSION,
        bundle_format: manifest.bundle_format,
        views,
        groups,
        // The full compiled schema, including `filter`-only columns: `/v1/meta` describes
        // what a caller may declare, not what occupies a row. Segment-facing readers narrow
        // to `render_scalars` at their own sites.
        declared_scalars: manifest.declared_scalars.clone(),
        homes: manifest
            .declared_scalars
            .iter()
            .map(|d| crate::filter::FieldHomes::of(d, &manifest.vocabularies))
            .collect(),
        scoped_scalars: manifest.scoped_scalars(),
        vocabularies: Arc::clone(&generation.vocabularies),
        idset: manifest.identity.idset,
    }
}
