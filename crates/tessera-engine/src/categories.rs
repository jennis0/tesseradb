//! Serving a category vocabulary: what a code stands for, and who may be told
//! (per-point-attributes §3.2, §3.3, §3.8; contracts §3.2).
//!
//! **The hot path ships codes; this is where a code acquires a name.** A render column stores a
//! fixed-width code and nothing else, so a client holding a viewport response has integers and no
//! way to read them. Resolving them here rather than widening the points batch keeps the wire
//! narrow — a key is a string per *value*, and inlining it would make it a string per *point*.
//!
//! **The unit of address is the column, not the vocabulary.** Two columns may draw from one value
//! set (§3.9's `values_of`), and they still have distinct member sets: a principal who may see
//! `finance` under `reviewing_department` may see nothing under `owner_department`. §3.2 makes
//! that normative, so the gate is applied per column even where the values behind it are shared.
//!
//! **Slice is not part of the address**, and deliberately. Membership is an *entity-space*
//! question and entity ids are bundle-global, so a column rendered in several slices has one
//! member set and one correct answer; adding slice to the key would invent a distinction the
//! predicate does not have.
//!
//! ## What is gated, and what is not
//!
//! `listing = "public"` is an authored assertion that the *existence* of these value names
//! discloses nothing (§3.8) — the names came from an artefact someone wrote, not from the corpus —
//! so the set is served as authored to any principal with a session.
//!
//! `listing = "per_viewer"` is the C11 channel: a value is visible iff at least one item carrying
//! it is (§3.3), derived per request from inside `M_auth`, never maintained. The per-`(column,
//! code)` membership sets it needs are the category's derived postings, which the build writes for
//! every `per_viewer` column whatever its `index` says (`filter-index.md` §2.3) — so the
//! predicate is `members(code) ∩ candidate ≠ ∅` against the *composed* candidate, the same
//! entity-space set a filter is evaluated under. **Derivation self-retires**: a value whose last
//! visible member is suppressed stops being offered with no third retirement rule, which is why
//! §3.3 rejects a maintained union.
//!
//! **The postings cover the base build alone**, so the extents a flush writes are swept too — a
//! value carried only by entities ingested since the build must still be offered to a principal who
//! can see one of them. A **buffered** entity is not covered: it has no row and no extent, so it
//! contributes no membership until its flush. That is `filter-index.md` §5's ruling in its
//! vocabulary form — a bounded lag of one flush interval that only ever *withholds* a value, never
//! offers one.
//!
//! Where those member sets are not there at all, the column is **refused rather than served**
//! ([`EngineError::VocabularyVisibilityUnavailable`]). Serving it unfiltered would publish value
//! names on nobody's authority, and serving it empty would be indistinguishable from a principal
//! who may see none of them, which is the one answer a viewer must not be given by mistake.
//!
//! ## Two request forms, one gate
//!
//! A caller either names the codes it holds or pages the whole set. Both run the same gate: a gate
//! applied to enumeration and not to lookup is the existence oracle reached by the other door.
//!
//! **An unresolvable code is omitted, never refused.** "No such code", "a code no key explains"
//! and "a value you cannot see" are one outcome, because distinguishing them makes this endpoint
//! an existence oracle over exactly what `per_viewer` hides — the same rule §3.8 states for a
//! filter naming an invisible value, and the same precedent contracts §3.2 sets for an unmatched
//! token.

use tessera_store::manifest::{Listing, VocabularyKind};
use tessera_store::vocabulary::ABSENT_CODE;

use crate::session::{EngineError, Result, Session};
use crate::Engine;

/// One category column, as `/v1/meta` publishes it.
///
/// `vocabulary` is named because two columns may share a value set and a client may reuse a
/// resolved palette across them. It must **not** reuse a resolved *visibility*: see this module's
/// header.
#[derive(Debug, Clone)]
pub struct CategoryColumn {
    /// The declared column name, which is also its identifier in `/v1/categories/{column}`.
    /// Unique bundle-wide — `tessera_build::schema` refuses a duplicate — and restricted to a
    /// path-safe character set for that reason.
    pub column: String,
    pub vocabulary: String,
    pub kind: VocabularyKind,
    pub listing: Listing,
}

/// One value: the stable key a row's code stands for, and its presentation.
///
/// **The key is not the display name** (§3.1). `label` is amendable without a build; the key is
/// what the code means and is the display fallback when no author wrote a label — which is every
/// value a discovered vocabulary mints.
#[derive(Debug, Clone)]
pub struct CategoryValue {
    pub code: u32,
    pub key: String,
    pub label: Option<String>,
}

/// Which values a caller wants.
#[derive(Debug, Clone, Copy)]
pub enum CategoryQuery<'a> {
    /// Resolve these codes and no others — the viewer's normal path, since it knows exactly which
    /// codes it drew. Duplicates and unresolvable codes are tolerated; see the header.
    Codes(&'a [u32]),
    /// Page the whole value set, ascending by key, resuming after `after`.
    Page {
        after: Option<&'a str>,
        limit: usize,
    },
}

/// One page of one column's values.
#[derive(Debug, Clone)]
pub struct CategoryPage {
    pub column: String,
    pub values: Vec<CategoryValue>,
    /// The `after` for the next page, or `None` when the set is complete. Always `None` for
    /// [`CategoryQuery::Codes`], which is bounded by the request rather than by the set.
    pub next: Option<String>,
}

impl Engine {
    /// Every category column this bundle declares, for `/v1/meta`.
    ///
    /// Plain columns are omitted rather than carried with an empty descriptor: the presence of the
    /// descriptor is exactly what tells a client a `u16` is a category and not an integer.
    pub fn category_columns(&self) -> Vec<CategoryColumn> {
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        manifest
            .declared_scalars
            .iter()
            .filter_map(|scalar| {
                let name = scalar.vocabulary.as_deref()?;
                // Both `kind` and `listing` are read from the **vocabulary**, which is the object
                // that carries them; a column referencing one that does not exist is refused at
                // seed (`Vocabularies::seed`), so this cannot silently drop a declared column.
                let vocabulary = generation.vocabularies.get(name)?;
                Some(CategoryColumn {
                    column: scalar.name.clone(),
                    vocabulary: name.to_string(),
                    kind: vocabulary.kind(),
                    listing: vocabulary.listing(),
                })
            })
            .collect()
    }

    /// `GET /v1/categories/{column}` (contracts §3.2): resolve codes, or page the value set.
    ///
    /// `Ok(None)` means the bundle declares no category column of this name — returned identically
    /// for a name that is nothing at all and for a name that is a *plain* scalar, so the route
    /// cannot be used to probe which columns are categories beyond what `/v1/meta` already says.
    ///
    /// **The generation is loaded once** (lifecycle §1.1), and the bindings come from it rather
    /// than from `MANIFEST.vocabularies` directly, so a value minted since the last build — living
    /// in a `SEGMENTS-<n>.json` extension — resolves like any other. A legend missing exactly the
    /// newest values is the defect that reading the manifest alone would produce.
    pub fn categories(
        &self,
        session: &Session,
        column: &str,
        query: CategoryQuery<'_>,
    ) -> Result<Option<CategoryPage>> {
        let generation = self.generation.load_full();
        let Some(scalar) = generation
            .bundle
            .manifest
            .declared_scalars
            .iter()
            .find(|s| s.name == column)
        else {
            return Ok(None);
        };
        let Some(vocabulary_name) = scalar.vocabulary.as_deref() else {
            return Ok(None);
        };
        let Some(vocabulary) = generation.vocabularies.get(vocabulary_name) else {
            return Ok(None);
        };

        // The gate, before a single value is read — and before the two request forms diverge, so
        // that neither can acquire a route around it.
        //
        // **A `public` set has no predicate at all**, and that is the whole of the difference: the
        // names came from an artefact someone wrote, so there is nothing to derive and no mask to
        // consult. A `per_viewer` set is derived from inside `M_auth` per request, per §3.3.
        let candidate = match vocabulary.listing() {
            Listing::Public => None,
            Listing::PerViewer => {
                // **The session's own fragment is not enough.** It is a snapshot taken at
                // authorise, and composition treats entities below the live watermark as
                // fragment-resident — so a value carried only by entities a flush has published
                // since would be derived as invisible from the stale one, and a viewer would be
                // told a value they can see does not exist.
                let fragment = self.fragment_for(session, &generation)?;
                Some(crate::filter::candidate(
                    &fragment,
                    &session.satisfied,
                    &generation.overlay,
                    &generation.buffer,
                ))
            }
        };
        let membership = match &candidate {
            None => None,
            Some(candidate) => Some(
                generation
                    .filter_columns
                    .category_membership(column, candidate)
                    .map_err(|e| EngineError::VocabularyVisibilityUnavailable {
                        column: column.to_string(),
                        detail: e.to_string(),
                    })?,
            ),
        };
        // `Ok(true)` for a `public` column, uniformly — the two request forms below then share one
        // filter, which is what keeps the gate from being reachable through one door and not the
        // other.
        let visible = |code: u32| -> Result<bool> {
            match &membership {
                None => Ok(true),
                Some(membership) => membership.carries(code).map_err(|e| {
                    EngineError::VocabularyVisibilityUnavailable {
                        column: column.to_string(),
                        detail: e.to_string(),
                    }
                }),
            }
        };

        let (values, next) = match query {
            CategoryQuery::Codes(codes) => {
                // Walked once in key order rather than probed per code: the map is keyed by key,
                // so a per-code probe would need a reverse index built per request anyway, and the
                // walk keeps the response in the same order the paged form returns.
                let mut values = Vec::new();
                for (key, code) in vocabulary.bindings() {
                    if !codes.contains(&code) || !visible(code)? {
                        continue;
                    }
                    values.push(CategoryValue {
                        code,
                        key: key.to_string(),
                        label: vocabulary.label_of(key).map(str::to_string),
                    });
                }
                (values, None)
            }
            CategoryQuery::Page { after, limit } => {
                // **Filtered before the page is cut, never after.** Taking `limit` values and then
                // dropping the invisible ones would return short pages whose length is a count of
                // what the principal cannot see — a per-page disclosure of exactly what `listing`
                // withholds — and would terminate the walk early, hiding visible values behind
                // invisible ones.
                let mut page: Vec<CategoryValue> = Vec::new();
                let mut next = None;
                for (key, code) in vocabulary.bindings() {
                    if after.is_some_and(|a| key <= a) || !visible(code)? {
                        continue;
                    }
                    // One past the page, read rather than counted: "is there another page" is
                    // answered by the walk instead of by a second pass over the set.
                    if page.len() == limit {
                        next = Some(page.last().expect("limit > 0").key.clone());
                        break;
                    }
                    page.push(CategoryValue {
                        code,
                        key: key.to_string(),
                        label: vocabulary.label_of(key).map(str::to_string),
                    });
                }
                (page, next)
            }
        };

        debug_assert!(
            !values.iter().any(|v| v.code == ABSENT_CODE),
            "code 0 is the absent sentinel and is never bound to a key (§3.6)"
        );

        Ok(Some(CategoryPage {
            column: column.to_string(),
            values,
            next,
        }))
    }
}
