//! Serving a category vocabulary: what a code stands for, and who may be told
//! (per-point-attributes §3.2, §3.3, §3.8; contracts §3.2).
//!
//! **The hot path ships codes; this is where a code acquires a name.** A render column stores a
//! fixed-width code and nothing else, so a client holding a viewport response has integers and no
//! way to read them. Resolving them here rather than widening the points batch keeps the wire
//! narrow — a key is a string per *value*, and inlining it would make it a string per *point*.
//!
//! **The unit of address is the column, not the vocabulary.** Two columns may draw from one value
//! set (one `[[vocabulary]]` named by both), and they still have distinct member sets: a principal who may see
//! `finance` under `reviewing_department` may see nothing under `owner_department`. §3.2 makes
//! that normative, so the gate is applied per column even where the values behind it are shared.
//!
//! **View is not part of the address for an entity-scoped column**, and deliberately. Membership
//! is an *entity-space* question and entity ids are bundle-global, so a column rendered in several
//! views has one member set and one correct answer; adding view to the key would invent a
//! distinction the predicate does not have.
//!
//! **A group-scoped category is the one column where it is** (`views.md` §5), and for the opposite
//! reason: the family is one column *per view*, so two views have two value sets and two member
//! sets, and answering either from the other would be a wrong answer rather than a redundant key.
//! The address is the resolved column — the request's own view under a view of the group, or a pin
//! anywhere else — resolved at the same site, through the same gate, as a filter leaf naming it.
//! Membership stays entity space and the mask still meets it there; what the view decides is which
//! column's postings are read.
//!
//! ## What is gated, and what is not
//!
//! `visibility = "public"` is an authored assertion that the *existence* of these value names
//! discloses nothing (§3.8) — the names came from an artefact someone wrote, not from the corpus —
//! so the set is served as authored to any principal with a session.
//!
//! `visibility = "derived"` is the C11 channel: a value is visible iff at least one item carrying
//! it is (§3.3), derived per request from inside `M_auth`, never maintained. The per-`(column,
//! code)` membership sets it needs are the category's derived postings, which the build writes for
//! every `derived` column whatever its `index` says (`filter-index.md` §2.3) — so the
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
//! an existence oracle over exactly what `derived` hides — the same rule §3.8 states for a
//! filter naming an invisible value, and the same precedent contracts §3.2 sets for an unmatched
//! token.

use tessera_analyse::SuggestionField;
use tessera_store::manifest::{Visibility, VocabularyKind};
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
    /// Unique bundle-wide — `tessera_build::config` refuses a duplicate — and restricted to a
    /// path-safe character set for that reason.
    pub column: String,
    pub vocabulary: String,
    pub kind: VocabularyKind,
    pub visibility: Visibility,
}

/// One value: the stable key a row's code stands for, and its presentation.
///
/// **The key is not the display name** (§3.1). `title` is amendable without a build; the key is
/// what the code means and is the display fallback when no author wrote a title — which is every
/// value a discovered vocabulary mints.
#[derive(Debug, Clone)]
pub struct CategoryValue {
    pub code: u32,
    pub key: String,
    pub title: Option<String>,
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

/// The vocabulary a **resolved** column's codes index — the entity-scoped column's own name, or one
/// view's column of a group-scoped family (`views.md` §5), which is the `name@<group>:<key>` form
/// `filter::scoped_column_name` mints and `EngineMeta::resolve_filter_column` returns.
///
/// **The caller has already resolved and gated it.** This function reads a manifest and nothing
/// else: which spellings a principal may turn into which resolved column — the group's gate, the
/// pin, the request's own view — is decided at the one site `views.md` §5 puts it, ahead of here.
/// A scoped family that is on no filter surface is not a value list either: the one
/// `index`-or-`render` licence (`scoped_is_filterable`) decides both, so a name that resolves to
/// nothing here is the `None` an undeclared column gets.
fn vocabulary_of(manifest: &tessera_store::manifest::Manifest, column: &str) -> Option<String> {
    if let Some(scalar) = manifest.declared_scalars.iter().find(|s| s.name == column) {
        return scalar.vocabulary.clone();
    }
    let (name, view_id) = column.split_once(crate::filter::PIN)?;
    manifest
        .scoped_scalars()
        .into_iter()
        .find(|f| {
            f.name == name
                && crate::filter::scoped_is_filterable(f)
                && f.views.iter().any(|v| v == view_id)
        })
        .and_then(|f| f.vocabulary)
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
                // Both `kind` and `visibility` are read from the **vocabulary**, which is the object
                // that carries them; a column referencing one that does not exist is refused at
                // seed (`Vocabularies::seed`), so this cannot silently drop a declared column.
                let vocabulary = generation.vocabularies.get(name)?;
                Some(CategoryColumn {
                    column: scalar.name.clone(),
                    vocabulary: name.to_string(),
                    kind: vocabulary.kind(),
                    visibility: vocabulary.visibility(),
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
    /// **`column` is a resolved column**, which for a group-scoped family is one view's
    /// ([`vocabulary_of`]): the caller resolves and gates the spelling a request carried before it
    /// reaches here.
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
        let Some(vocabulary_name) = vocabulary_of(&generation.bundle.manifest, column) else {
            return Ok(None);
        };
        let Some(vocabulary) = generation.vocabularies.get(&vocabulary_name) else {
            return Ok(None);
        };

        // The gate, before a single value is read — and before the two request forms diverge, so
        // that neither can acquire a route around it.
        //
        // **A `public` set has no predicate at all**, and that is the whole of the difference: the
        // names came from an artefact someone wrote, so there is nothing to derive and no mask to
        // consult. A `derived` set is derived from inside `M_auth` per request, per §3.3.
        let candidate = match vocabulary.visibility() {
            Visibility::Public => None,
            Visibility::Derived => {
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
                // **Probed per code, through the vocabulary's own reverse map.** It used to walk
                // every binding testing `codes.contains`, which is a million-step walk per legend
                // resolve at 10⁶ values — tens of milliseconds per viewport where this is
                // microseconds (`value-suggestion.md` §9).
                //
                // **The response order is unchanged**: key order, as the paged form returns, taken
                // by sorting the resolved values rather than by the walk's own order. Sorting a
                // caller's page-sized list is not the walk it replaces.
                let mut values: Vec<CategoryValue> = Vec::new();
                let mut seen = rustc_hash::FxHashSet::default();
                for &code in codes {
                    // Duplicates in the request are tolerated (the header) and must not become
                    // duplicates in the response, which the walk got for free and a probe does not.
                    if !seen.insert(code) {
                        continue;
                    }
                    let Some(key) = vocabulary.key_of(code) else {
                        continue;
                    };
                    if !visible(code)? {
                        continue;
                    }
                    values.push(CategoryValue {
                        code,
                        key: key.to_string(),
                        title: vocabulary.title_of(key).map(str::to_string),
                    });
                }
                values.sort_by(|a, b| a.key.cmp(&b.key));
                (values, None)
            }
            CategoryQuery::Page { after, limit } => {
                // **Filtered before the page is cut, never after.** Taking `limit` values and then
                // dropping the invisible ones would return short pages whose length is a count of
                // what the principal cannot see — a per-page disclosure of exactly what `visibility`
                // withholds — and would terminate the walk early, hiding visible values behind
                // invisible ones.
                let mut page: Vec<CategoryValue> = Vec::new();
                let mut next = None;
                // **Resumed by a range, not by walking from the start and discarding.** The cursor
                // is a key and the map is keyed by key, so the second page of a 10⁶-value
                // vocabulary need not step over the first (`value-suggestion.md` §9).
                let bindings: Box<dyn Iterator<Item = (&str, u32)>> = match after {
                    Some(after) => Box::new(vocabulary.bindings_after(after)),
                    None => Box::new(vocabulary.bindings()),
                };
                for (key, code) in bindings {
                    if !visible(code)? {
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
                        title: vocabulary.title_of(key).map(str::to_string),
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

/// Where a suggestion matched, in **characters of the served string** — so a client highlights
/// without re-implementing the fold (`value-suggestion.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchSpan {
    /// Which served string matched: `key` or `title`. A word-start match reports the field the word
    /// came from and the start of that word.
    pub field: SuggestionField,
    pub start: u32,
    pub len: u32,
}

/// One suggested value.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub code: u32,
    pub key: String,
    pub title: Option<String>,
    pub span: MatchSpan,
    /// The number of items carrying this value that this viewer may see — present iff the request
    /// asked for counts. **Never `0` as a stand-in for absent**: a served `0` is a real answer that
    /// this surface cannot produce, since a value with no visible member is not suggested at all.
    pub count: Option<u64>,
}

/// One suggestion page. Uncursored: a typeahead never pages, the user types another character.
#[derive(Debug, Clone)]
pub struct SuggestPage {
    pub column: String,
    pub values: Vec<Suggestion>,
    /// `true` iff the walk stopped before its range was exhausted — the page filled, or the walk
    /// budget was spent.
    ///
    /// **On a spent budget this is a thresholded, pre-mask count of the values under the prefix**,
    /// hidden ones included: it says at least `walk_budget` values sit there. That is the quantity
    /// `architecture.md` Appendix C registers as **C31**, at one bit of resolution, and it is
    /// registered as on the wire and not only in time. The alternative — `false` on a spent budget,
    /// so the flag counted visible values alone — under-reports, and a broad prefix would hide
    /// visible values behind a flag saying there were none.
    pub more: bool,
}

impl Engine {
    /// `GET /v1/categories/{column}/suggest` (contracts §3.2): the values whose key, title or word
    /// start begins with what the caller typed, and that this viewer may see.
    ///
    /// # Two doors, one gate
    ///
    /// **Everything [`Engine::categories`] decides about who may be told a value name is decided
    /// here, by the same code.** The same [`vocabulary_of`] resolution, the same composed candidate,
    /// the same `category_membership`, the same refusal. A gate applied to one listing surface and
    /// not to the other is the existence oracle reached by the second door, so the two differ in
    /// shape — order, paging, titles, spans — and in nothing else.
    ///
    /// `Ok(None)` means the bundle declares no category column of this name, returned identically
    /// for a name that is nothing at all and for a plain scalar, exactly as the enumeration does.
    ///
    /// # The walk
    ///
    /// Fold `q`; two binary searches give the entry range; walk it merged with the vocabulary's
    /// side map in folded order, skipping what a title amendment retracted and what has already
    /// been emitted, testing `carries` per value against the memory-mapped posting as a **boolean
    /// that short-circuits**. Stop at `limit` values or when `walk_budget` values have been
    /// examined.
    ///
    /// For a `public` column there is no predicate and every value in the range is emitted in
    /// order — which is the whole of the difference, as it is on the enumeration.
    ///
    /// **The walk's cost is a function of how many values sit under the prefix, hidden ones
    /// included.** That is the timing channel the owner accepted on 2026-09-02 and Appendix C
    /// registers as **C31**; §8 of the design carries what bounds it. It is not a defect of this
    /// implementation to fix, and closing it is §6.3's priced lever rather than a change here.
    ///
    /// # Errors
    ///
    /// A `derived` column whose member sets cannot be read refuses, as the enumeration does. **A
    /// read that fails part-way through the walk refuses the whole column too**, rather than
    /// serving the values found so far: refusing at the value the read failed at would make the
    /// refusal a function of the prefix the caller typed, which is an oracle over value names in a
    /// fault state.
    pub fn suggest(
        &self,
        session: &Session,
        column: &str,
        q: &str,
        limit: usize,
        counts: bool,
        walk_budget: u64,
    ) -> Result<Option<SuggestPage>> {
        let generation = self.generation.load_full();
        let Some(vocabulary_name) = vocabulary_of(&generation.bundle.manifest, column) else {
            return Ok(None);
        };
        let Some(vocabulary) = generation.vocabularies.get(&vocabulary_name) else {
            return Ok(None);
        };
        let Some(live) = generation.suggest.get(&vocabulary_name) else {
            return Err(EngineError::SuggestionUnavailable {
                column: column.to_string(),
                detail: format!("vocabulary '{vocabulary_name}' has no suggestion index"),
            });
        };

        // **The gate, before a single entry is read**, and it is `Engine::categories`' gate
        // verbatim. A `public` set has no predicate at all; a `derived` one is derived from inside
        // `M_auth` per request, against the composed candidate rather than against the request's
        // filters (I3, I12).
        //
        // A count is the viewer's own `and_cardinality` and needs the mask whatever the visibility
        // says, so `counts` composes the candidate for a `public` column too. That only ever
        // narrows a number; it never widens the set of values served, which the visibility alone
        // still decides.
        let derived = vocabulary.visibility() == Visibility::Derived;
        let candidate = if derived || counts {
            let fragment = self.fragment_for(session, &generation)?;
            Some(crate::filter::candidate(
                &fragment,
                &session.satisfied,
                &generation.overlay,
                &generation.buffer,
            ))
        } else {
            None
        };
        let membership = match &candidate {
            None => None,
            Some(candidate) => Some(
                generation
                    .filter_columns
                    .category_membership(column, candidate)
                    .map_err(|e| {
                        // A `public` column reaches this only because a count was asked for, and a
                        // count it cannot compute is refused rather than omitted: a page whose
                        // `count` fields were silently absent would read as "asked and answered".
                        if derived {
                            EngineError::VocabularyVisibilityUnavailable {
                                column: column.to_string(),
                                detail: e.to_string(),
                            }
                        } else {
                            EngineError::SuggestionUnavailable {
                                column: column.to_string(),
                                detail: format!("counts were asked for and {e}"),
                            }
                        }
                    })?,
            ),
        };
        let visible = |code: u32| -> Result<bool> {
            match (&membership, derived) {
                // Either no mask was composed at all, or one was composed only to count with —
                // both are `public`, and a `public` value set is served as authored.
                (_, false) => Ok(true),
                // **Unreachable, and fail-closed rather than trusted to stay so.** A `derived`
                // column composes a candidate above and builds a membership from it or refuses, so
                // this pair cannot arise today. It is an error and not `Ok(true)` because the two
                // wrong answers are not symmetric: `true` here publishes every value name of a
                // `derived` vocabulary to a principal whose predicate was never evaluated, which is
                // the C11 disclosure itself and would be invisible — the page would look like a
                // wide principal's. A future edit that reorders the composition above turns that
                // into a refusal instead.
                (None, true) => Err(EngineError::VocabularyVisibilityUnavailable {
                    column: column.to_string(),
                    detail: "a derived column reached the walk with no composed membership"
                        .to_string(),
                }),
                (Some(membership), true) => membership.carries(code).map_err(|e| {
                    EngineError::VocabularyVisibilityUnavailable {
                        column: column.to_string(),
                        detail: e.to_string(),
                    }
                }),
            }
        };

        let fold = tessera_analyse::SuggestionFold::new();
        let unreadable = |e: std::io::Error| EngineError::SuggestionUnavailable {
            column: column.to_string(),
            detail: e.to_string(),
        };
        // **The walk itself is `crate::suggest`'s**, and everything above this line is the gate.
        // The split is where it is so that the bench measures the shipped walk rather than a
        // transcription of it, and so that this function reads as what it is: the same gate
        // `Engine::categories` applies, over a different traversal.
        let (found, more) = crate::suggest::walk(
            live,
            &fold,
            q,
            crate::suggest::WalkBudget {
                limit,
                walk_budget,
                counts,
            },
            &|code| visible(code),
            &|code| match &membership {
                Some(membership) => membership.count(code).map_err(|e| {
                    EngineError::SuggestionUnavailable {
                        column: column.to_string(),
                        detail: e.to_string(),
                    }
                }),
                // Unreachable: `counts` composes a candidate for both visibilities, so a count is
                // only ever asked for where a membership exists.
                None => Ok(0),
            },
            &unreadable,
        )?;
        let values: Vec<Suggestion> = found
            .into_iter()
            .map(|found| Suggestion {
                code: found.code,
                key: found.key,
                title: found.title,
                span: MatchSpan {
                    field: found.field,
                    start: found.start,
                    len: found.len,
                },
                count: found.count,
            })
            .collect();

        debug_assert!(
            !values.iter().any(|v| v.code == ABSENT_CODE),
            "code 0 is the absent sentinel and is never bound to a key (§3.6)"
        );
        Ok(Some(SuggestPage {
            column: column.to_string(),
            values,
            more,
        }))
    }
}

