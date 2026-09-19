//! `GET /v1/meta`: the bundle facts a client needs, and resolving a name against them.

use super::*;

/// One declared view, as `GET /v1/meta` publishes it.
///
/// **The projection and the frame are both here, because that is where each is declared**
/// (`projections.md` §3, decision 0040): a projection and an extent belong to a view, and two
/// views of one bundle may be projected — and quantised — differently. What a client draws under
/// a view is a function of the two together, and the server derives `tile` from them here rather
/// than at the wire (`projections.md` §9).
#[derive(Debug, Clone)]
pub struct MetaView {
    pub id: String,
    pub display_name: String,
    /// The frame every position in this view is quantised against, immutable for the view's life
    /// (decision 0040) — what a client decodes a Morton prefix with, and what the write path
    /// checks a coordinate against.
    pub quantisation: Quantisation,
    /// What placed every position in this view before the frame did — the closed set of
    /// `projections.md` §5, and [`Projection::None`] for a view that projects nothing.
    pub projection: Projection,
    /// The tile this view's frame **is**, in the scheme that addresses it — or `None` where no
    /// published scheme does, which is every projection but an aligned `web_mercator` one.
    ///
    /// **The scheme and the address are one field because neither is meaningful alone**: an
    /// address without a scheme names nothing, and a scheme with no address gives a client no
    /// tiles to ask for. The wire publishes them as two (`projections.md` §9) and they are absent
    /// together.
    pub tile: Option<TileAddress>,
    /// Where this view sits in its group's roster (`views.md` §3.2), or `None` for a plain view.
    ///
    /// **A plain view has no roster entry, and that is a fact rather than an omission**: a key is
    /// a group's, so a plain view carrying an empty one would invite a client to order a set of
    /// one.
    pub roster: Option<MetaRoster>,
    /// The declaration's `point_visibility.default`, or `None` where it named none
    /// (decision 0133). Read by `/control/ingest` to fill a row whose `access` list is empty, or
    /// to refuse the batch. **On no wire**: `/v1/meta` builds its body by hand and does not
    /// publish it.
    pub point_default: Option<String>,
}

/// One view's roster record, as `GET /v1/meta` publishes it beside the view (`views.md` §3.2).
///
/// The key is the caller's own and is a view's only address
/// ([decision 0113](../../../docs/decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md));
/// the metadata is the group's declared names with this view's typed values. **Order is the list's
/// order** — creation order, which is roster-record order — so a client offers previous-and-next
/// by walking the group's `views` array rather than by interpreting a key or a number.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaRoster {
    pub group: String,
    pub key: String,
    /// Typed, one entry per name the owning group declared. Empty on a `members` group's views,
    /// whose metadata belongs to the owner (`views.md` §3.3).
    pub metadata: BTreeMap<String, ViewMetadataValue>,
}

/// One view group, as `GET /v1/meta` publishes it: the name and its views in creation order.
///
/// **A group is not a view** — it cannot be named on a viewer verb and has no row space — so what
/// is published here is the ordering and nothing else: every setting a group holds is already on
/// each of its views, and a second copy of the frame beside the roster is a second thing to
/// disagree with the first.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaGroup {
    pub name: String,
    /// The group's declared title, `None` where it declared none. Presentation metadata on an
    /// object whose visibility is already decided, so it is a deployment constant and not a
    /// per-principal field: a principal who sees the group sees its title.
    pub title: Option<String>,
    /// The group whose keys these are, where this group declares `members`
    /// (`views.md` §3.3); `None` where it owns them.
    pub members_of: Option<String>,
    /// This group's view ids — the joined `group:key` form a request names — in creation order.
    pub views: Vec<String>,
}

/// The tile a view's frame corresponds to, and the scheme it is a tile of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileAddress {
    /// The tile scheme's name — [`tessera_spatial::frame::XYZ`], the slippy-map `z/x/y` every
    /// basemap server publishes, is the only one this system can name.
    pub scheme: &'static str,
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

/// `GET /v1/meta`'s payload (R5) — the bundle-level facts a viewer client needs before it can
/// issue a sensible `/v1/viewport` call.
#[derive(Debug, Clone)]
pub struct EngineMeta {
    pub api_version: u32,
    pub bundle_format: u32,
    /// The declared views, **in serving order** (`views.md` §3.2): the plain views in manifest
    /// order, then each group's views in creation order. Each carries its own frame (decision
    /// 0040) and, for a group's view, its roster record. There is no bundle-level extent:
    /// [`EngineMeta::quantisation_of`] answers for a named view, and a caller with no view id is
    /// asking a question the bundle cannot answer.
    pub views: Vec<MetaView>,
    /// The view groups, in manifest order, each listing its views in creation order.
    ///
    /// Empty is the ordinary case — a declaration of plain views alone — and it is the same
    /// answer as "this bundle has no group", there being nothing else empty could mean.
    pub groups: Vec<MetaGroup>,
    pub declared_scalars: Vec<DeclaredScalar>,
    /// The **group-scoped attribute column families** (`views.md` §5), flattened over the groups
    /// in manifest order: one entry per family, each naming the group whose views it has a column
    /// per and the view ids that have one.
    ///
    /// Empty is the ordinary case — a corpus whose attributes are all entity-scoped, which needs
    /// no declaration to say so.
    pub scoped_scalars: Vec<tessera_store::manifest::ScopedScalar>,
    /// The live category bindings, from the same generation as `declared_scalars`.
    ///
    /// **Ingest resolves keys through this, and never mints.** A declared vocabulary is immutable
    /// between builds, so a handler's snapshot cannot be stale for one; a discovered vocabulary's
    /// novel keys travel to the write executor as keys, because two handlers racing one novel key
    /// would draw two codes for it and split its rows between them.
    pub vocabularies: Arc<Vocabularies>,
    /// The idset (contracts §2.2/§2.6 r6). `GET /v1/meta` reports this
    /// verbatim as `idset`; `POST /v1/items/{tessera_id}` compares an optional
    /// caller-supplied `idset` against it. Never the identity **key** — that never leaves the
    /// server, on any plane (design Appendix C, C17; I10).
    pub idset: u32,
}

/// **The one statement of §3.3's ownership rule**: the key `view` holds in `group`'s roster,
/// given the roster record `view` carries — `(its own group, its key)` — and a lookup for what a
/// group declares `members` of.
///
/// A view of `group` holds its own key; a view of a group declaring `members` of `group` holds the
/// same key, the keys being the owner's by construction; anything else holds none. Two callers ask
/// it of different data — [`EngineMeta::owning_key`] of the meta document's roster records,
/// [`scoped_render_scalars`] of the manifest's — and the rule itself lives here so it cannot come
/// to mean two things. The build asks the same question of its own arguments
/// (`pipeline::scoped_render_targets`), across a crate boundary, and says so at that site.
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

/// What a filter leaf's column spelling resolves to under a request's view
/// ([`EngineMeta::resolve_filter_column`], `views.md` §5).
///
/// **Four outcomes, and three of them are refusals a caller can act on.** They are kept apart
/// here, in the engine, rather than collapsed into one error at the wire, because the codes they
/// carry differ: an ambiguous leaf is contracts §3.1's `422` — a malformed request, not an empty
/// answer, since a leaf with no column to read is not a constraint — where a pin naming nothing is
/// the `404` an unknown view already gets, and must stay indistinguishable from one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafColumn {
    /// Not a filterable column under any spelling: the ordinary unknown-column refusal.
    Unknown,
    /// The column the engine evaluates — the leaf's own name for an entity-scoped column, and one
    /// view's resolved name for a group-scoped family — and the family its values are read by.
    Resolved {
        column: String,
        family: crate::filter::Family,
    },
    /// A group-scoped attribute named bare under a view that decides no column of its family.
    Unpinned { group: String },
    /// A pin naming no view of the attribute's group — an undeclared key, or a view with no
    /// column.
    UnknownPin { group: String, pin: String },
    /// A pin on a column that has no scope: one column for the corpus, and nothing for a view to
    /// choose between.
    PinOnUnscoped { column: String },
}

impl EngineMeta {
    /// The projection that placed a named view's positions, or `None` for a view this bundle does
    /// not declare.
    ///
    /// **Keyed by view, never bundle-wide.** A projection is declared per view (`projections.md`
    /// §3) while the frame is not, so a single answer would have to pick one of two differently
    /// projected views — and both callers are about one view's rows: a `region` leaf names the view
    /// it filters, and a shape submission names the layer whose views it publishes into. Reading
    /// the *first* declared view instead is correct only while a bundle carries one, and fails
    /// silently rather than loudly on the day one carries two: a shape would be placed by another
    /// view's projection and simply hold the wrong rows.
    ///
    /// An unknown name is `None` and the caller refuses. Defaulting it to [`Projection::None`]
    /// would put a degree through the identity transform and quantise it as a frame coordinate.
    /// The view a request's id names — a plain view's name, or a group's `<group>:<key>`
    /// (`views.md` §3.2).
    ///
    /// **One resolution for both planes.** A viewer verb's `view`, `x-tessera-view` and this
    /// document's own `views` are one namespace, and two resolutions of it would eventually
    /// disagree about what a `404` is — which contracts §3.1's closed code list does not allow.
    /// **The key is the only address a view has** (decision 0113): an id nothing declares is
    /// `None` whatever shape it has, and the caller's 404 says no more than "unknown view".
    pub fn resolve_view(&self, requested: &str) -> Option<&MetaView> {
        self.views.iter().find(|v| v.id == requested)
    }

    /// [`Self::resolve_view`] **through the session's visible-view set** (`views.md` §6) — the one
    /// place a viewer verb's `view` is resolved, and the only gate check the request path makes.
    ///
    /// **One set-membership lookup, on both outcomes, and that is the point.** The probe is made
    /// whether or not a view was found: a gate-failed name and a name nobody ever declared reach
    /// the caller as the same `None`, having cost the same work — no plugin call, no roster scan,
    /// no second branch. That is r23's work-indistinguishability standard, and the closure
    /// Appendix C's C4 records for `/v1/items`, applied to a view id. Making the probe conditional
    /// on a hit would put one hash lookup on the gate-failed path and none on the unknown one,
    /// which is the difference a timing test is built to find.
    ///
    /// The set itself was resolved at authorise and is fixed for the session's life, so a view
    /// created since is a `None` here until the session re-authorises — the owner ruling
    /// [`crate::Session::visible_views`] records.
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

    /// Resolve a **filter leaf's column spelling** under the view a request names
    /// (`views.md` §5) — the one place the `@` forms are read, on both the meta surface's side and
    /// the parser's.
    ///
    /// An entity-scoped column resolves to itself and takes no pin: there is one column for the
    /// corpus, and a pin on it would name a view that decides nothing. A **group-scoped** family
    /// resolves to exactly one view's column:
    ///
    /// - **under a view of the attribute's group**, or of a group sharing its views
    ///   (`views.md` §3.3), the request's own view decides and nothing is added to the wire;
    /// - **under any other view** the leaf must pin — `sentiment@2026-Q3`, by key and only by key
    ///   — resolved through the same `group:key` namespace [`EngineMeta::resolve_view`] answers a
    ///   viewer verb's `view` from, so the two cannot come to disagree about what a name means;
    /// - a **pin under a view of the same group** is allowed and means what it says: Q4's map
    ///   filtered by Q3's sentiment.
    ///
    /// The resolved column is an ordinary entity-space one and evaluates as its family's unscoped
    /// columns do — the scope decides which file, never how the values are read.
    ///
    /// **The scoped surface is inside the gate** (`views.md` §5, §6), and it collapses in one
    /// direction: for a principal whose group gate fails, the whole attribute is **undeclared**.
    /// Bare and pinned uses alike take [`LeafColumn::Unknown`] — the ordinary unknown-column
    /// refusal, which names no group — rather than the `Unpinned` 422 that names one or the
    /// `UnknownPin` 404 that confirms the key space. Without that the pinned leaf is a route
    /// around the gate: a principal failing `quarter`'s gate could filter their visible entities
    /// by a Q3 value, which is per-entity membership of a gated view. Decision 0090's argument —
    /// a gate at some surfaces and not others is fail-open — is the rule applied here, and
    /// `/v1/meta`'s `filter_operands` omits the family on the same test.
    ///
    /// Where the group *is* reachable, a **pin** resolves through
    /// [`EngineMeta::resolve_visible_view`], so a pin naming a view of the group this principal
    /// may not reach is the `UnknownPin` a key no view holds already gets.
    pub fn resolve_filter_column(
        &self,
        leaf: &str,
        view: &str,
        visible: &crate::gate::VisibleViews,
    ) -> LeafColumn {
        let (name, pin) = match leaf.split_once(crate::filter::PIN) {
            Some((name, pin)) => (name, Some(pin)),
            None => (leaf, None),
        };
        // **The entity-scoped columns first**, because a name is one or the other and never both:
        // the build refuses a scoped family that shares a declared column's name.
        if let Some(declared) = self
            .declared_scalars
            .iter()
            .find(|d| d.name == name && crate::filter::is_filterable(d))
        {
            return match pin {
                None => LeafColumn::Resolved {
                    column: name.to_string(),
                    family: crate::filter::Family::of(declared),
                },
                Some(_) => LeafColumn::PinOnUnscoped {
                    column: name.to_string(),
                },
            };
        }
        let Some(family) = self
            .scoped_scalars
            .iter()
            .find(|f| f.name == name && crate::filter::scoped_is_filterable(f))
        else {
            return LeafColumn::Unknown;
        };
        // **The group's gate, ahead of the pin/bare split**, so both spellings take the same
        // unknown-column answer and neither confirms the group or its keys (`views.md` §5).
        if !visible.contains_group(&family.group) {
            return LeafColumn::Unknown;
        }
        let resolved = |view_id: &str| LeafColumn::Resolved {
            column: crate::filter::scoped_column_name(name, view_id),
            family: crate::filter::Family::of_scoped(family),
        };
        match pin {
            Some(pin) => {
                let requested =
                    format!("{}{}{}", family.group, tessera_store::GROUP_SEPARATOR, pin);
                match self.resolve_visible_view(&requested, visible) {
                    // A view of the group that has no column — one created since the build — is
                    // the same answer as a key nobody declared, and the same answer a gate-failed
                    // one gets: what a caller learns is only that the pin names nothing to read.
                    Some(view) if family.views.contains(&view.id) => resolved(&view.id),
                    _ => LeafColumn::UnknownPin {
                        group: family.group.clone(),
                        pin: pin.to_string(),
                    },
                }
            }
            // **The request's own view, where it is one of the family's** — its own group's, or a
            // group sharing them, whose keys are the owner's by construction (`views.md` §3.3).
            None => match self.owning_key(view, &family.group) {
                Some(key) => {
                    let id = format!("{}{}{}", family.group, tessera_store::GROUP_SEPARATOR, key);
                    match family.views.contains(&id) {
                        true => resolved(&id),
                        false => LeafColumn::Unpinned {
                            group: family.group.clone(),
                        },
                    }
                }
                None => LeafColumn::Unpinned {
                    group: family.group.clone(),
                },
            },
        }
    }

    /// Resolve a **`/v1/categories` column spelling** — [`Self::resolve_filter_column`]'s question
    /// asked by the value-list route, whose admission is not the filter surface's.
    ///
    /// **A category has a value list whether or not it is filterable**, and that difference is the
    /// whole reason this is a second function. An entity-scoped category declared with neither
    /// `render` nor `index` is *blob-resident* (records §3): no hot column, no entity-space
    /// structure, no operand — and `/v1/meta` still publishes its `category` block, drill-down
    /// still returns its code, and the code still needs a key. Resolving such a name through the
    /// filter admission would answer `404` for a column the schema declares and the rest of the
    /// surface talks about.
    ///
    /// So the entity-scoped columns are resolved here **by declaration alone**, ahead of that
    /// admission, and everything else — every group-scoped family, the gate that collapses one,
    /// the pin, the bare leaf with nothing to decide it — falls through to
    /// [`Self::resolve_filter_column`] unchanged. The scoped surface therefore keeps exactly one
    /// site deciding what a principal may reach, which is what `views.md` §5 requires of it; what
    /// is widened is only the entity-scoped half, where there is no view and no gate to widen.
    ///
    /// A name that is a declared *non-category* resolves to itself and its own family, and the
    /// caller refuses it as it refuses a name that is nothing at all — this route must not become
    /// a finer answer than `/v1/meta`'s about which columns are categories.
    pub fn resolve_category_column(
        &self,
        leaf: &str,
        view: &str,
        visible: &crate::gate::VisibleViews,
    ) -> LeafColumn {
        let (name, pin) = match leaf.split_once(crate::filter::PIN) {
            Some((name, pin)) => (name, Some(pin)),
            None => (leaf, None),
        };
        if let Some(declared) = self.declared_scalars.iter().find(|d| d.name == name) {
            return match pin {
                None => LeafColumn::Resolved {
                    column: name.to_string(),
                    family: crate::filter::Family::of(declared),
                },
                Some(_) => LeafColumn::PinOnUnscoped {
                    column: name.to_string(),
                },
            };
        }
        self.resolve_filter_column(leaf, view, visible)
    }

    /// The key `view` holds in `group`'s roster — its own if it is a view of that group, and the
    /// key it shares if its group declares `members` of it (`views.md` §3.3). `None` for a plain
    /// view, or a view of an unrelated group.
    ///
    /// **Public because the ingest boundary asks it too** (decision 0116): a scoped value's address
    /// is `(attribute → its group, key)`, so which families a batch may name is this question and
    /// not a spelling test on the view id. One resolution, three surfaces — the filter leaf, the
    /// render list, and the write.
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

    /// Every view id whose row space carries one column of `family` — the owning group's own
    /// ids, and the same keys under every group declaring `members` of it (`views.md` §3.3, §5).
    ///
    /// **`ScopedScalar::views` is the owner's list and is not the answer a client needs.** A
    /// request names a view, and under `quarter_map:2026-Q1` the column arrives though only
    /// `quarter:2026-Q1` is named there — so publishing the stored list alone would tell a client
    /// reading a sharing group's map that the column it is receiving does not exist. This is the
    /// same expansion [`scoped_render_scalars`] makes at the request; there it resolves one view,
    /// here it enumerates them.
    ///
    /// Unfiltered: the caller applies the gate, `/v1/meta`'s per-principal narrowing being the
    /// server's own (`views.md` §6).
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

    /// The frame a named view's positions are quantised against, or `None` for a view this bundle
    /// does not declare.
    ///
    /// **Keyed by view, never bundle-wide** (decision 0040), on exactly the argument
    /// [`Self::projection_of`] makes for the projection beside it: the extent is the view's, so a
    /// single answer would have to pick one of two differently framed views, and reading the
    /// *first* declared view is correct only while a bundle carries one. It fails silently on the
    /// day one carries two — a region canonicalised against another view's grid, or an ingest
    /// row's cell checked against a frame it does not live in.
    ///
    /// An unknown name is `None` and the caller refuses. There is no default frame.
    pub fn quantisation_of(&self, view: &str) -> Option<Quantisation> {
        self.views
            .iter()
            .find(|v| v.id == view)
            .map(|v| v.quantisation)
    }
}

impl Engine {
    /// `GET /v1/meta` (R5): read-only bundle facts, no session/authorisation involved. Loads the
    /// generation once, like every other request path.
    pub fn meta(&self) -> EngineMeta {
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        // **One derivation of what a client is looking at** (`projections.md` §9). The scheme is a
        // function of the view's projection and the view's own frame together — both declared per
        // view — and it is derived here rather than at the wire so that the ingest plane, which
        // reads this same structure, cannot come to a different answer about the same bundle.
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
        // **Serving order is the roster's order** (`views.md` §3.2): the plain views in manifest
        // order, then each group's views in creation order — which is the order the roster
        // records themselves are in, a build's declarations first and each create appended after
        // (decision 0113). Nothing is sorted here: the record order *is* the order, and a sort
        // would need a key nothing stores.
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
                // A roster entry with no declared view is refused at open
                // (`Manifest::validate_groups`), so this cannot silently drop one.
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
            // The **full** compiled schema, including `filter`-only columns: `/v1/meta` describes
            // what a caller may declare and supply on the ingest plane, not what occupies a row.
            // The segment-facing readers narrow to `render_scalars` at their own sites.
            declared_scalars: manifest.declared_scalars.clone(),
            scoped_scalars: manifest.scoped_scalars(),
            vocabularies: Arc::clone(&generation.vocabularies),
            idset: manifest.identity.idset,
        }
    }
}
