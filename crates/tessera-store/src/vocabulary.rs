//! Minting category codes: the draw, the never-reuse set, and exhaustion
//! (per-point-attributes §3.4, §3.6).
//!
//! **A code is not an ordinal, and nothing that assigns positions is reused to assign one.** The
//! auth dictionary's correctness argument — extents positional and append-only, the moved-under
//! discard — exists *because* an ordinal is a position in a concatenation. §3.4 requires the
//! opposite object: a code drawn at random from the declared width's unused space, recorded beside
//! its key, pinned forever. Codes are the `tessera_id` of vocabulary space; ordinals are its
//! `entity_id`, and the two are joined only through the key. That is why none of
//! `tessera_authz::dict` appears here.
//!
//! **The disclosure the scatter closes.** Dense first-seen codes make a *visible* code a lower
//! bound on vocabulary cardinality: a viewer holding code 7 learns at least seven values exist.
//! The draw is therefore uniform over the whole width, from OS entropy, at every call — a
//! first-fit fallback, a seeded RNG reaching production, or "draw from a counter, it's simpler"
//! each reintroduce the disclosure while every functional test still passes, because dense codes
//! work perfectly. [`VocabularyMinter::mint`] is the only place a code comes into being.
//!
//! **Never-reuse is enforced by [`VocabularyMinter::assigned`] and by nothing else.** A code that
//! already colours rows must never be drawn again — re-drawing it silently recolours every row
//! that carries it, with no error and no digest mismatch. So the set has to be seeded from *every*
//! home a binding can live in before the first draw: `MANIFEST.vocabularies`, the served
//! `SEGMENTS-<n>.json`'s `vocabulary_extensions`, replayed [`crate::manifest::VocabularyExtension`]
//! mints, and the codes a vocabulary's own `source` file seeds. Missing one is the failure this module is most
//! exposed to, and it is silent — `a_draw_excludes_codes_from_every_home` is the direct test.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use rand::rngs::OsRng;
use rand::RngCore;

use tessera_spatial::tiler::ScalarType;

use crate::manifest::{
    DeclaredScalar, ManifestVocabulary, ManifestVocabularyValue, Visibility, VocabularyExtension,
    VocabularyKind,
};

/// The reserved *absent* code (§3.6). Never drawn and never in a value block, so a row carrying no
/// value for a column is distinguishable from one carrying the first value.
pub const ABSENT_CODE: u32 = 0;

/// How many uniform draws are attempted before the minter switches to selecting the i-th free code
/// directly.
///
/// **Not a correctness parameter** — both branches are uniform over the free set, so the only thing
/// this trades is time. Rejection sampling is one draw per mint until the space is most of the way
/// full; the fallback is O(bindings) and exists so that a nearly-full `u8` (which reaches a 99.6%
/// fill in ordinary use) terminates promptly rather than spinning on rejections.
const DRAW_ATTEMPTS: usize = 64;

/// Why a key could not acquire a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// **The empty string is not a value.** It is what an unset field, a trimmed whitespace-only
    /// cell and a client bug all produce, so minting it would let a typo become a category with
    /// properties and a visibility consequence — the creation views §80 rules out. Refused loudly
    /// rather than mapped to [`ABSENT_CODE`], which would silently accept the same defect.
    EmptyKey { vocabulary: String },
    /// **The width's code space is full.** Never widen and never wrap: both recolour rows that
    /// already exist (§3.6). A `u8` holds 255 values, a `u16` 65,535, each one fewer than its range
    /// because code 0 is reserved.
    Exhausted {
        vocabulary: String,
        width: ScalarType,
        assigned: u64,
    },
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MintError::EmptyKey { vocabulary } => write!(
                f,
                "vocabulary '{vocabulary}': the empty string is not a value key \
                 (per-point-attributes §3.4). An unset field is *absent* — code 0 — and minting a \
                 code for it would make a typo a category"
            ),
            MintError::Exhausted {
                vocabulary,
                width,
                assigned,
            } => write!(
                f,
                "vocabulary '{vocabulary}': the {} code space is full at {assigned} assigned codes \
                 (per-point-attributes §3.6). Widening or wrapping would recolour every row that \
                 carries an existing code, so neither is done; the vocabulary needs a wider \
                 declaration and a rebuild",
                width.arrow_type_name()
            ),
        }
    }
}

impl std::error::Error for MintError {}

/// A binding that contradicts one already held — the same key at a different code, or the same
/// code under a different key.
///
/// **Corruption of acked state, not a race.** Every row written under either binding is now of
/// unknowable colour, so the caller's answer is to refuse to open rather than to pick a winner:
/// picking one silently recolours the other's rows. Reported by [`VocabularyMinter::seed_value`]
/// and by replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingConflict {
    pub vocabulary: String,
    pub key: String,
    pub code: u32,
    /// What was already held: the code this key had, or the key this code was under.
    pub held: HeldBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldBinding {
    /// This key is already bound to a different code.
    KeyAt(u32),
    /// This code is already under a different key.
    CodeUnder(String),
}

impl std::fmt::Display for BindingConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let BindingConflict {
            vocabulary,
            key,
            code,
            held,
        } = self;
        match held {
            HeldBinding::KeyAt(existing) => write!(
                f,
                "vocabulary '{vocabulary}': key '{key}' is bound to code {existing} and also to \
                 code {code}. Rows exist under both, so their colour is unknowable and this is \
                 refused rather than resolved"
            ),
            HeldBinding::CodeUnder(existing) => write!(
                f,
                "vocabulary '{vocabulary}': code {code} is bound to key '{existing}' and also to \
                 key '{key}'. Rows exist under both, so their colour is unknowable and this is \
                 refused rather than resolved"
            ),
        }
    }
}

impl std::error::Error for BindingConflict {}

/// Whether [`VocabularyMinter::mint`] found the key or created it.
///
/// The caller must distinguish them: a fresh code has to reach the WAL and the manifest before the
/// row that uses it is acknowledged, where an existing one is already durable in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Minted {
    /// The key was already bound; this is its pinned code, unchanged.
    Existing(u32),
    /// The key was novel and this code was drawn for it. The binding is in this minter and nowhere
    /// else until the caller records it.
    Fresh(u32),
}

impl Minted {
    pub fn code(self) -> u32 {
        match self {
            Minted::Existing(code) | Minted::Fresh(code) => code,
        }
    }
}

/// The live binding set for one vocabulary: key → code, plus every code ever spent.
///
/// **Vocabulary-scoped, not column-scoped.** A `ManifestVocabulary` is a named object several
/// columns may share (§3.9); codes are shared with it so that cross-column and cross-view legends
/// compose. Two columns sharing a discovered vocabulary mint into one code space through one of
/// these.
#[derive(Debug, Clone)]
pub struct VocabularyMinter {
    name: String,
    kind: VocabularyKind,
    visibility: Visibility,
    width: ScalarType,
    /// Key → code. **The key bytes are `Arc<str>` and are shared with [`Self::by_code`]**, so the
    /// reverse map costs a pointer per value rather than a second copy of every key — which at 10⁷
    /// values is the difference between a pointer table and another 705 MB of heap
    /// (`value-suggestion.md` §6.1, measured).
    codes: BTreeMap<Arc<str>, u32>,
    /// Code → key: the map `/v1/categories?codes=` resolves through.
    ///
    /// **It replaces a walk of the whole binding set per request.** The `Codes` arm tested
    /// `codes.contains(&code)` against every binding, which at 10⁶ values is a million-step walk
    /// per legend resolve — tens of milliseconds where it is now microseconds
    /// (`value-suggestion.md` §9). A hash map rather than a `BTreeMap` because a code is drawn at
    /// random over the width and has no order a reader wants; the *response* order is still key
    /// order, taken from the caller's codes being resolved and then sorted.
    by_code: HashMap<u32, Arc<str>>,
    /// Per-value presentation, keyed as `codes` is; absent for a value given no title, which is
    /// every value a *discovered* vocabulary mints — there was no author to write one.
    ///
    /// **Here rather than read back from the manifest at the point of use**, because a bound value
    /// lives in one of two homes (the manifest, or a `SEGMENTS-<n>.json` extension) and a reader
    /// that consulted only the first would serve a legend missing every value minted since the
    /// last build. This type already exists to be the union of those homes.
    titles: BTreeMap<Arc<str>, String>,
    /// Every code that must never be drawn: bound values and authored `reserved` retirements
    /// alike. [`ABSENT_CODE`] is excluded by the draw itself rather than held here, so that a
    /// vocabulary's assigned count is the number of codes it has actually spent.
    assigned: BTreeSet<u32>,
}

impl VocabularyMinter {
    /// An empty minter over `width`'s code space.
    ///
    /// `width` must be a category width (§3.6); anything else is a schema defect caught at parse,
    /// and is treated here as `u32` rather than panicking — the widest domain, so a wrong width can
    /// only fail to exhaust, never to collide.
    pub fn new(
        name: impl Into<String>,
        kind: VocabularyKind,
        visibility: Visibility,
        width: ScalarType,
    ) -> Self {
        VocabularyMinter {
            name: name.into(),
            kind,
            visibility,
            width,
            codes: BTreeMap::new(),
            by_code: HashMap::new(),
            titles: BTreeMap::new(),
            assigned: BTreeSet::new(),
        }
    }

    /// Seed one already-durable binding. Idempotent for an identical one, so seed-then-replay is
    /// order-insensitive and a restated binding is not a conflict.
    pub fn seed_value(&mut self, key: &str, code: u32) -> Result<(), BindingConflict> {
        if let Some(&held) = self.codes.get(key) {
            if held == code {
                return Ok(());
            }
            return Err(BindingConflict {
                vocabulary: self.name.clone(),
                key: key.to_string(),
                code,
                held: HeldBinding::KeyAt(held),
            });
        }
        if self.assigned.contains(&code) {
            // The code is spent. Under a different key it is a conflict; under `reserved` it is an
            // authored retirement that a value block then re-used, which is the same defect seen
            // from the other side.
            let under = self
                .by_code
                .get(&code)
                .map(|k| k.to_string())
                .unwrap_or_else(|| format!("<reserved {code}>"));
            return Err(BindingConflict {
                vocabulary: self.name.clone(),
                key: key.to_string(),
                code,
                held: HeldBinding::CodeUnder(under),
            });
        }
        self.bind(key, code);
        self.assigned.insert(code);
        Ok(())
    }

    /// The one place a key and a code become a pair, so the forward and reverse maps cannot
    /// disagree about one.
    fn bind(&mut self, key: &str, code: u32) {
        let key: Arc<str> = Arc::from(key);
        self.codes.insert(Arc::clone(&key), code);
        self.by_code.insert(code, key);
    }

    /// Seed a retired code (§3.4's `reserved`). Spent, so never drawn, but bound to no key.
    pub fn seed_reserved(&mut self, code: u32) {
        self.assigned.insert(code);
    }

    /// Seed from a compiled manifest entry — the build-time and folded bindings, plus `reserved`.
    pub fn seed_manifest(
        &mut self,
        vocabulary: &ManifestVocabulary,
    ) -> Result<(), BindingConflict> {
        for value in &vocabulary.values {
            self.seed_value(&value.key, value.code)?;
            if let Some(title) = &value.title {
                // Keyed by the `Arc` the binding above interned, so a title costs no second copy
                // of its key either.
                if let Some((key, _)) = self.codes.get_key_value(value.key.as_str()) {
                    self.titles.insert(Arc::clone(key), title.clone());
                }
            }
        }
        for &code in &vocabulary.reserved {
            self.seed_reserved(code);
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the existence of this vocabulary's values is sensitive (§3.8) — what
    /// `/v1/categories` gates on.
    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    pub fn width(&self) -> ScalarType {
        self.width
    }

    /// Bound this minter's code space to `width`, for a column declared at a running service over
    /// a vocabulary no column named before (`ingest.md` §1.3).
    ///
    /// A vocabulary no column names is seeded at `u32`, the widest domain, and the first column
    /// to name it is what fixes the width the rows store; the same rule a build applies through
    /// the columns it compiles (`Vocabularies::seed`). Refused where a code already bound would
    /// not fit: narrowing past a bound code would leave a row whose stored code the column cannot
    /// hold, and the offending code is returned so the refusal can name it. Widening is never
    /// asked for, because a column that names the vocabulary already fixed the width and a
    /// differing declaration is refused before this is reached.
    pub fn narrow_to(&mut self, width: ScalarType) -> std::result::Result<(), u32> {
        let max = usable_max(width);
        if let Some(&code) = self.assigned.iter().next_back() {
            if code > max {
                return Err(code);
            }
        }
        self.width = width;
        Ok(())
    }

    /// Whether a key this minter does not carry is a typo or a value waiting for a code.
    ///
    /// **Consulted at the boundary, not here.** The refusal for a declared vocabulary belongs in
    /// the ingest handler, where the whole batch can be rejected without effect and the caller
    /// gets a 422 naming the key; by the time a novel key reaches [`Self::mint`] the decision that
    /// it *may* be minted has already been made.
    pub fn kind(&self) -> VocabularyKind {
        self.kind
    }

    /// The code bound to `key`, or `None`. **A caller mapping ingest or build data through this
    /// must treat `None` as a refusal, not as absent** for a declared vocabulary: §5's
    /// declare-then-use rule exists because a category carries properties and, through its
    /// postings, a visibility consequence.
    pub fn code_of(&self, key: &str) -> Option<u32> {
        self.codes.get(key).copied()
    }

    /// The key `code` is bound to, or `None` where nothing is — an unbound code, a `reserved`
    /// retirement, or the absent sentinel.
    ///
    /// **The reverse of [`Self::code_of`], in one lookup rather than a walk.** See
    /// [`Self::by_code`].
    pub fn key_of(&self, code: u32) -> Option<&str> {
        self.by_code.get(&code).map(|k| &**k)
    }

    /// Every binding whose key sorts strictly after `after`, ascending — `/v1/categories`' page
    /// cursor, resumed by a range rather than by walking the map from its start and discarding.
    pub fn bindings_after<'a>(&'a self, after: &str) -> impl Iterator<Item = (&'a str, u32)> {
        use std::ops::Bound;
        self.codes
            .range::<str, _>((Bound::Excluded(after), Bound::Unbounded))
            .map(|(k, &c)| (&**k, c))
    }

    /// Every binding, ascending by key.
    ///
    /// **Key order, and `/v1/categories` pages in it.** Code order would be the obvious choice for
    /// a numeric cursor and is the wrong one twice over: codes are drawn at random from the width
    /// (§3.4), so code order is arbitrary to a reader and sorts a legend into nonsense, and the
    /// map is keyed by key, so producing it would mean collecting and sorting the whole vocabulary
    /// on every page of every request. Keys are unique, so a key is a total order and therefore a
    /// usable cursor.
    pub fn bindings(&self) -> impl Iterator<Item = (&str, u32)> {
        self.codes.iter().map(|(k, &c)| (&**k, c))
    }

    /// This value's presentation title, where an author wrote one. `None` is ordinary — the key is
    /// the display fallback, and a discovered value never has one.
    pub fn title_of(&self, key: &str) -> Option<&str> {
        self.titles.get(key).map(String::as_str)
    }

    /// Give a bound value the title its author supplied, replacing any title it holds
    /// (`ingest.md` §1.3; decision 0136's amendment).
    ///
    /// **A title upserts; a value's identity does not.** The key-to-code binding is immutable and
    /// a code is never reused, so nothing a viewer's stored code means can change here. A title is
    /// presentation, and an operator recolouring a legend is doing what the control plane exists
    /// for. A key nothing has bound is ignored, because a title without a binding has no value to
    /// present.
    pub fn set_title(&mut self, key: &str, title: String) {
        // Keyed by the `Arc` the binding interned, so a title costs no second copy of its key.
        if let Some((interned, _)) = self.codes.get_key_value(key) {
            self.titles.insert(Arc::clone(interned), title);
        }
    }

    /// How many codes are spent — the quantity a cardinality alarm watches.
    pub fn assigned_count(&self) -> u64 {
        self.assigned.len() as u64
    }

    /// The key's code, drawing one if it has none.
    ///
    /// **View-first**: a bound key returns its pinned code without touching the draw, which is what
    /// makes two commit windows minting the same novel key agree — the second consults this and
    /// finds the first's binding. It is also why minting may happen *only* where the view is
    /// authoritative and serial (the write executor, at the commit-window close). Two request
    /// handlers racing a novel key would each draw, and one key would end up with two codes and its
    /// rows split between them; whichever binding survived would recolour the other's rows. That
    /// hazard is closed structurally by handlers not calling this, and this comment is the
    /// statement of where minting may happen — the invariant-bearing half.
    pub fn mint(&mut self, key: &str) -> Result<Minted, MintError> {
        if key.is_empty() {
            return Err(MintError::EmptyKey {
                vocabulary: self.name.clone(),
            });
        }
        if let Some(&code) = self.codes.get(key) {
            return Ok(Minted::Existing(code));
        }
        let code = self.draw()?;
        self.bind(key, code);
        self.assigned.insert(code);
        Ok(Minted::Fresh(code))
    }

    /// Draw an unassigned code, uniformly over the width's usable space.
    ///
    /// Two branches, both uniform over exactly the free set. The mask makes the raw draw uniform
    /// over `0..=max` without modulo bias; `0` and the assigned set are rejected.
    fn draw(&self) -> Result<u32, MintError> {
        let max = usable_max(self.width);
        let free = u64::from(max) - self.assigned.len() as u64;
        if free == 0 {
            return Err(MintError::Exhausted {
                vocabulary: self.name.clone(),
                width: self.width,
                assigned: self.assigned.len() as u64,
            });
        }

        let mut rng = OsRng;
        for _ in 0..DRAW_ATTEMPTS {
            let candidate = rng.next_u32() & max;
            if candidate != ABSENT_CODE && !self.assigned.contains(&candidate) {
                return Ok(candidate);
            }
        }

        // Dense: take the i-th free code rather than keep rejecting. Walking the assigned set's
        // gaps rather than materialising the free set keeps this O(bindings) — a `u32` vocabulary
        // dense enough to reach here would make the free *list* tens of gigabytes.
        let mut i = uniform_below(&mut rng, free);
        let mut candidate: u64 = 1;
        for &spent in self.assigned.range(1..) {
            let gap = u64::from(spent) - candidate;
            if i < gap {
                break;
            }
            i -= gap;
            candidate = u64::from(spent) + 1;
        }
        Ok((candidate + i) as u32)
    }
}

/// Why a bundle's bindings could not be assembled into a live view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedError {
    /// Two durable homes disagree about a binding. Corruption of acked state — see
    /// [`BindingConflict`].
    Conflict(BindingConflict),
    /// A column names a vocabulary the manifest does not carry. Its codes would decode to nothing,
    /// so the bundle does not open rather than serving marks of unknowable colour.
    UndeclaredVocabulary { column: String, vocabulary: String },
    /// Two columns share a vocabulary at different widths. One code space, and one of the columns
    /// cannot hold the other's codes (§3.9) — caught at build, so reaching it means a hand-edited
    /// or corrupt manifest.
    WidthDisagreement {
        vocabulary: String,
        column: String,
        width: ScalarType,
        other_width: ScalarType,
    },
    /// An extension names a vocabulary `MANIFEST.vocabularies` does not carry. The fold folds
    /// extensions into that table by name, so a name with no home would be dropped at the next
    /// fold and every row carrying its codes would lose its key.
    ExtensionWithoutVocabulary { vocabulary: String },
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::Conflict(c) => write!(f, "{c}"),
            SeedError::UndeclaredVocabulary { column, vocabulary } => write!(
                f,
                "column '{column}' names vocabulary '{vocabulary}', which MANIFEST.vocabularies \
                 does not carry. Its codes decode to nothing, so this bundle does not open"
            ),
            SeedError::WidthDisagreement {
                vocabulary,
                column,
                width,
                other_width,
            } => write!(
                f,
                "vocabulary '{vocabulary}' is shared by columns declared {} and {} (at '{column}'). \
                 A shared vocabulary is one code space, and one column cannot hold the other's \
                 codes (per-point-attributes §3.9)",
                other_width.arrow_type_name(),
                width.arrow_type_name()
            ),
            SeedError::ExtensionWithoutVocabulary { vocabulary } => write!(
                f,
                "SEGMENTS vocabulary_extensions carries '{vocabulary}', which \
                 MANIFEST.vocabularies does not declare. The fold folds extensions in by name, so \
                 these bindings have no home to be folded into"
            ),
        }
    }
}

impl std::error::Error for SeedError {}

impl From<BindingConflict> for SeedError {
    fn from(c: BindingConflict) -> Self {
        SeedError::Conflict(c)
    }
}

/// Every vocabulary the bundle declares, seeded from its durable homes.
///
/// **Seeded before WAL replay, and replay's mints apply over the seed** — the established
/// seed-before-replay order (contracts §2.3, and the deny seeding beside it). Bindings are
/// append-only and never rebound, so seed-then-replay is order-insensitive except for conflicts,
/// which refuse.
///
/// **Its completeness *is* the never-reuse invariant.** Every home must be represented before the
/// first draw; see this module's header.
#[derive(Debug, Clone, Default)]
pub struct Vocabularies {
    by_name: BTreeMap<String, VocabularyMinter>,
}

impl Vocabularies {
    /// Assemble the live view from a bundle's `MANIFEST.json` and the served
    /// `SEGMENTS-<n>.json`'s extensions.
    ///
    /// The width comes from the `declared_scalars` entry that names the vocabulary, not from the
    /// vocabulary itself: a value set is a set of keys and codes, and what bounds the code space is
    /// the column that stores them.
    pub fn seed(
        vocabularies: &[ManifestVocabulary],
        declared_scalars: &[DeclaredScalar],
        extensions: &[VocabularyExtension],
    ) -> std::result::Result<Self, SeedError> {
        let mut widths: BTreeMap<&str, (ScalarType, &str)> = BTreeMap::new();
        for scalar in declared_scalars {
            let Some(name) = scalar.vocabulary.as_deref() else {
                continue;
            };
            if !vocabularies.iter().any(|v| v.name == name) {
                return Err(SeedError::UndeclaredVocabulary {
                    column: scalar.name.clone(),
                    vocabulary: name.to_string(),
                });
            }
            match widths.get(name) {
                Some(&(width, _)) if width != scalar.arrow_type => {
                    return Err(SeedError::WidthDisagreement {
                        vocabulary: name.to_string(),
                        column: scalar.name.clone(),
                        width: scalar.arrow_type,
                        other_width: width,
                    });
                }
                _ => {
                    widths.insert(name, (scalar.arrow_type, scalar.name.as_str()));
                }
            }
        }

        let mut by_name = BTreeMap::new();
        for vocabulary in vocabularies {
            // **A column that names the vocabulary is the authority; the declaration's own width
            // is the answer where none does.** A built vocabulary no column names is carried but
            // unusable, so either answer would serve; one declared at a running service is minted
            // into before any column names it (`ingest.md` §1.3), and taking the widest domain
            // there would draw codes the column later declared for it cannot hold. A width this
            // build cannot parse is `u32`, the widest domain, which can only fail to exhaust and
            // never to collide.
            let width = widths
                .get(vocabulary.name.as_str())
                .map(|&(w, _)| w)
                .or_else(|| ScalarType::parse(&vocabulary.width))
                .unwrap_or(ScalarType::U32);
            let mut minter = VocabularyMinter::new(
                vocabulary.name.clone(),
                vocabulary.kind,
                vocabulary.visibility,
                width,
            );
            minter.seed_manifest(vocabulary)?;
            by_name.insert(vocabulary.name.clone(), minter);
        }

        for extension in extensions {
            let minter = by_name.get_mut(&extension.name).ok_or_else(|| {
                SeedError::ExtensionWithoutVocabulary {
                    vocabulary: extension.name.clone(),
                }
            })?;
            for value in &extension.values {
                minter.seed_value(&value.key, value.code)?;
                // **The extension's title wins over the manifest's**, the manifest having been
                // seeded above: an extension carries a value whose title was supplied since, and
                // dropping it here would lose an upsert at every restart before the fold
                // (`ingest.md` §1.3).
                if let Some(title) = &value.title {
                    minter.set_title(&value.key, title.clone());
                }
            }
        }
        Ok(Vocabularies { by_name })
    }

    pub fn get(&self, name: &str) -> Option<&VocabularyMinter> {
        self.by_name.get(name)
    }

    /// Add a vocabulary declared while the service runs (`ingest.md` §1.3), replacing any minter
    /// held under the same name.
    ///
    /// **Only the executor may call this**, on [`VocabularyMinter::mint`]'s rule: a name added
    /// between another thread's check and its draw is a second code space for one vocabulary. The
    /// caller has already refused a redeclaration under a different identity, so a replacement
    /// here is the same vocabulary seen twice.
    pub fn insert(&mut self, minter: VocabularyMinter) {
        self.by_name.insert(minter.name().to_string(), minter);
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut VocabularyMinter> {
        self.by_name.get_mut(name)
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The extension set to write into the next `SEGMENTS-<n>.json`: every live binding that
    /// `MANIFEST.vocabularies` does not already carry.
    ///
    /// **Append, never restate.** The caller starts from the manifest it is extending and unions
    /// this in, so a gap in this derivation can only fail to *add* a binding — never delete one
    /// the previous manifest held. That asymmetry is the whole reason `vocabulary_extensions` is
    /// carried forward where `deny` is restated: `deny` must be able to shrink and a binding must
    /// not.
    ///
    /// **The title travels with the binding.** This is the only path by which a value bound
    /// between builds reaches a manifest, and [`fold_extensions_into`] copies what it finds
    /// verbatim into the next `MANIFEST.vocabularies` — so a title dropped here is a title the
    /// fold destroys, in a value set whose keys and codes survive. A discovered value has no
    /// title and carries `None`, which is what it had before.
    ///
    /// **A value the manifest already binds is carried where its live title differs from the
    /// manifest's**, which is what a title upserted onto a build-declared value needs
    /// (decision 0136's amendment). Its key and its code are the manifest's own, restated
    /// unchanged; only the title is new. Without this the upsert would live in the minters alone
    /// and the next fold would publish the title the build wrote.
    pub fn extensions_beyond(
        &self,
        vocabularies: &[ManifestVocabulary],
    ) -> Vec<VocabularyExtension> {
        let mut out = Vec::new();
        for (name, minter) in &self.by_name {
            let held: BTreeMap<&str, Option<&str>> = vocabularies
                .iter()
                .find(|v| &v.name == name)
                .map(|v| {
                    v.values
                        .iter()
                        .map(|value| (value.key.as_str(), value.title.as_deref()))
                        .collect()
                })
                .unwrap_or_default();
            let values: Vec<ManifestVocabularyValue> = minter
                .bindings()
                .filter(|(key, _)| match held.get(key) {
                    None => true,
                    Some(&title) => minter.title_of(key).is_some() && minter.title_of(key) != title,
                })
                .map(|(key, code)| ManifestVocabularyValue {
                    title: minter.title_of(key).map(str::to_string),
                    key: key.to_string(),
                    code,
                })
                .collect();
            if !values.is_empty() {
                out.push(VocabularyExtension {
                    name: name.clone(),
                    values,
                });
            }
        }
        out
    }
}

/// Fold a prefix's `vocabulary_extensions` into the `MANIFEST.vocabularies` the next prefix will
/// carry — the compaction fold's whole vocabulary duty (§3.3).
///
/// **Verbatim, and verbatim is the whole rule.** Keys and codes move across byte-identical and
/// `reserved` is untouched. A fold that re-derived, re-sorted or re-numbered would recolour the
/// entire corpus with no error and no digest mismatch, because `columns.arrow` stores the code and
/// nothing else records what it stood for.
///
/// A binding that arrives twice — the same key at the same code, from two partitions or from a
/// restated manifest — folds once. A *contradicting* one cannot arrive: [`Vocabularies::seed`]
/// refuses to open a bundle whose homes disagree, so by the time a fold runs the two are known to
/// agree. An extension naming a vocabulary the table lacks is refused at the same place, so it is
/// skipped here rather than tolerated with a silent home of its own.
pub fn fold_extensions_into(
    vocabularies: &mut [ManifestVocabulary],
    extensions: &[VocabularyExtension],
) {
    for extension in extensions {
        let Some(vocabulary) = vocabularies.iter_mut().find(|v| v.name == extension.name) else {
            continue;
        };
        for value in &extension.values {
            match vocabulary
                .values
                .iter_mut()
                .find(|held| held.key == value.key)
            {
                // **The title is the one thing an extension changes.** A value the table already
                // carries keeps its key and its code — that is the verbatim rule — and an
                // extension carrying a title replaces the one the table holds, which is what a
                // page onto a build-declared vocabulary supplies (`ingest.md` §1.3, decision
                // 0136's amendment). An extension with no title leaves the held one alone: a
                // discovered value carries `None` and would otherwise erase a name.
                Some(held) => {
                    if value.title.is_some() {
                        held.title = value.title.clone();
                    }
                }
                None => vocabulary.values.push(value.clone()),
            }
        }
    }
}

/// The highest usable code at `width` — also the mask that makes a raw `u32` draw uniform over the
/// width's domain. Code 0 is reserved, so the count of usable codes equals this value.
fn usable_max(width: ScalarType) -> u32 {
    match width {
        ScalarType::U8 => u32::from(u8::MAX),
        ScalarType::U16 => u32::from(u16::MAX),
        _ => u32::MAX,
    }
}

/// A uniform `0..n`, rejecting the biased tail rather than taking a plain remainder.
fn uniform_below(rng: &mut OsRng, n: u64) -> u64 {
    debug_assert!(n > 0);
    let limit = u64::MAX - (u64::MAX % n);
    loop {
        let r = rng.next_u64();
        if r < limit {
            return r % n;
        }
    }
}

/// The bindings this minter holds, as a manifest value list — ascending by key, so the bytes under
/// the digest are a function of the set and not of an iteration order.
pub fn values_of(minter: &VocabularyMinter) -> Vec<ManifestVocabularyValue> {
    minter
        .bindings()
        .map(|(key, code)| ManifestVocabularyValue {
            key: key.to_string(),
            code,
            title: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minter(width: ScalarType) -> VocabularyMinter {
        VocabularyMinter::new(
            "departments",
            VocabularyKind::Discovered,
            Visibility::Derived,
            width,
        )
    }

    /// **Never-reuse holds only if the assigned set is seeded from every home a binding lives in.**
    /// Risk 2 of the design memo, and it is silent: a draw that lands on a live code recolours
    /// every row already carrying it, and every functional test over fresh state still passes.
    #[test]
    fn a_draw_excludes_codes_from_every_home() {
        let mut m = minter(ScalarType::U8);
        // MANIFEST.vocabularies, including a `reserved` retirement.
        m.seed_manifest(&ManifestVocabulary {
            name: "departments".to_string(),
            kind: VocabularyKind::Declared,
            visibility: Visibility::Derived,
            width: "u32".to_string(),
            values: (1..=100)
                .map(|c| ManifestVocabularyValue {
                    key: format!("built-{c}"),
                    code: c,
                    title: None,
                })
                .collect(),
            reserved: (101..=150).collect(),
        })
        .expect("a consistent manifest seeds");
        // SEGMENTS vocabulary_extensions, and a replayed mint on top of them.
        for c in 151..=200 {
            m.seed_value(&format!("ext-{c}"), c).expect("no conflict");
        }
        for c in 201..=250 {
            m.seed_value(&format!("wal-{c}"), c).expect("no conflict");
        }

        // 250 spent of 255 usable: every remaining draw must come from the five free codes, which
        // also exercises the dense branch.
        let mut drawn = Vec::new();
        for i in 0..5 {
            match m.mint(&format!("novel-{i}")).expect("space remains") {
                Minted::Fresh(code) => drawn.push(code),
                Minted::Existing(code) => panic!("a novel key must draw, got {code}"),
            }
        }
        drawn.sort_unstable();
        assert_eq!(
            drawn,
            vec![251, 252, 253, 254, 255],
            "the only free codes are the ones no home had spent"
        );
        assert_eq!(
            m.mint("one-too-many"),
            Err(MintError::Exhausted {
                vocabulary: "departments".to_string(),
                width: ScalarType::U8,
                assigned: 255,
            })
        );
    }

    /// **A dense draw is still uniform.** The fallback selects the i-th free code; if it were
    /// first-fit instead, this would return the same code every time.
    #[test]
    fn the_dense_branch_does_not_degrade_to_first_fit() {
        let mut seen = BTreeSet::new();
        for _ in 0..64 {
            let mut m = minter(ScalarType::U8);
            for c in 1..=245 {
                m.seed_value(&format!("k{c}"), c).unwrap();
            }
            if let Minted::Fresh(code) = m.mint("novel").unwrap() {
                seen.insert(code);
            }
        }
        assert!(
            seen.len() > 1,
            "64 draws from a 10-code free set returned only {seen:?} — a first-fit fallback \
             reintroduces the cardinality disclosure the scatter closes"
        );
    }

    /// **Dense codes are the disclosure §3.4 exists to close**, and no functional test notices
    /// them, because dense codes work perfectly. Minting into an empty `u16` must not enumerate.
    #[test]
    fn minting_does_not_produce_a_dense_prefix() {
        let mut m = minter(ScalarType::U16);
        let codes: Vec<u32> = (0..32)
            .map(|i| m.mint(&format!("k{i}")).unwrap().code())
            .collect();
        let dense: Vec<u32> = (1..=32).collect();
        assert_ne!(
            codes, dense,
            "codes 1..k make a visible code a lower bound on vocabulary cardinality"
        );
        // The probability of 32 uniform draws from 65,535 all landing below 256 is ~0.
        assert!(
            codes.iter().any(|&c| c > 255),
            "a draw confined to the low byte is not uniform over the declared width: {codes:?}"
        );
    }

    /// View-first: the second minting of a key returns the first's code rather than drawing again.
    /// This is what makes two commit windows minting the same novel key agree.
    #[test]
    fn a_bound_key_never_draws_twice() {
        let mut m = minter(ScalarType::U16);
        let first = m.mint("k9-unit").unwrap();
        assert!(matches!(first, Minted::Fresh(_)));
        let second = m.mint("k9-unit").unwrap();
        assert_eq!(second, Minted::Existing(first.code()));
        assert_eq!(m.assigned_count(), 1);
    }

    /// The empty string is refused rather than mapped to absent — a typo must not become a value,
    /// and silently storing it as code 0 accepts the same defect without saying so.
    #[test]
    fn the_empty_key_is_refused_not_treated_as_absent() {
        let mut m = minter(ScalarType::U16);
        assert_eq!(
            m.mint(""),
            Err(MintError::EmptyKey {
                vocabulary: "departments".to_string()
            })
        );
        assert_eq!(m.assigned_count(), 0);
    }

    /// A restated binding is idempotent; a contradicting one is refused. Seed-then-replay depends
    /// on the first, and the second is the backstop that turns a lost mint record loud.
    #[test]
    fn a_conflicting_binding_is_refused_and_an_identical_one_is_not() {
        let mut m = minter(ScalarType::U16);
        m.seed_value("k9-unit", 4711).unwrap();
        m.seed_value("k9-unit", 4711)
            .expect("restating a binding is not a conflict");

        assert_eq!(
            m.seed_value("k9-unit", 99),
            Err(BindingConflict {
                vocabulary: "departments".to_string(),
                key: "k9-unit".to_string(),
                code: 99,
                held: HeldBinding::KeyAt(4711),
            })
        );
        assert_eq!(
            m.seed_value("canine", 4711),
            Err(BindingConflict {
                vocabulary: "departments".to_string(),
                key: "canine".to_string(),
                code: 4711,
                held: HeldBinding::CodeUnder("k9-unit".to_string()),
            })
        );
    }

    fn declared(name: &str, width: ScalarType, vocabulary: Option<&str>) -> DeclaredScalar {
        DeclaredScalar {
            name: name.to_string(),
            arrow_type: width,
            vocabulary: vocabulary.map(str::to_string),
            analyser: None,
            index: false,
            render: true,
        }
    }

    fn vocabulary(name: &str, values: &[(&str, u32)]) -> ManifestVocabulary {
        ManifestVocabulary {
            name: name.to_string(),
            kind: VocabularyKind::Declared,
            visibility: Visibility::Derived,
            width: "u32".to_string(),
            values: values
                .iter()
                .map(|(key, code)| ManifestVocabularyValue {
                    key: key.to_string(),
                    code: *code,
                    title: None,
                })
                .collect(),
            reserved: Vec::new(),
        }
    }

    /// The width bounding a vocabulary's code space is the *column's*, not the vocabulary's — a
    /// value set is keys and codes, and what bounds the space is what stores it.
    #[test]
    fn a_vocabularys_width_comes_from_the_column_that_stores_it() {
        let v = Vocabularies::seed(
            &[vocabulary("departments", &[("ops", 9)])],
            &[declared("department", ScalarType::U8, Some("departments"))],
            &[],
        )
        .expect("a consistent bundle seeds");
        assert_eq!(v.get("departments").unwrap().width(), ScalarType::U8);
        assert_eq!(v.get("departments").unwrap().code_of("ops"), Some(9));
    }

    /// §3.9: a shared vocabulary is one code space, so two widths over it is one column unable to
    /// hold the other's codes. Caught at build; reaching it means a hand-edited manifest.
    #[test]
    fn two_widths_over_one_vocabulary_refuse_to_open() {
        let err = Vocabularies::seed(
            &[vocabulary("shared", &[])],
            &[
                declared("a", ScalarType::U8, Some("shared")),
                declared("b", ScalarType::U16, Some("shared")),
            ],
            &[],
        )
        .expect_err("one code space cannot have two widths");
        assert!(matches!(err, SeedError::WidthDisagreement { .. }), "{err}");
    }

    /// A column whose vocabulary is missing would store codes that decode to nothing. The bundle
    /// does not open, rather than serving marks of unknowable colour.
    #[test]
    fn a_column_naming_no_vocabulary_refuses_to_open() {
        let err = Vocabularies::seed(
            &[],
            &[declared("department", ScalarType::U8, Some("departments"))],
            &[],
        )
        .expect_err("a category with no value set is not openable");
        assert!(
            matches!(err, SeedError::UndeclaredVocabulary { .. }),
            "{err}"
        );
    }

    /// The extension set written to the next manifest is what the build does *not* already carry.
    /// It is unioned into the manifest being extended, never used to replace it — so a gap here can
    /// only fail to add a binding, never delete one.
    #[test]
    fn extensions_carry_only_what_the_build_does_not() {
        let built = vec![vocabulary("departments", &[("ops", 9)])];
        let mut v = Vocabularies::seed(
            &built,
            &[declared("department", ScalarType::U16, Some("departments"))],
            &[],
        )
        .unwrap();
        assert!(
            v.extensions_beyond(&built).is_empty(),
            "a bundle straight out of the build extends nothing"
        );

        let minted = v.get_mut("departments").unwrap().mint("k9-unit").unwrap();
        let extensions = v.extensions_beyond(&built);
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].name, "departments");
        assert_eq!(
            extensions[0]
                .values
                .iter()
                .map(|value| (value.key.as_str(), value.code))
                .collect::<Vec<_>>(),
            vec![("k9-unit", minted.code())],
            "only the minted binding — the built one already lives in MANIFEST.vocabularies"
        );

        // Seeding a fresh view from the manifest plus those extensions recovers the same state,
        // which is what makes a restart lossless.
        let reopened = Vocabularies::seed(
            &built,
            &[declared("department", ScalarType::U16, Some("departments"))],
            &extensions,
        )
        .expect("the two homes agree");
        assert_eq!(reopened.get("departments").unwrap().code_of("ops"), Some(9));
        assert_eq!(
            reopened.get("departments").unwrap().code_of("k9-unit"),
            Some(minted.code())
        );
    }

    /// An extension whose vocabulary the build does not declare has no home to be folded into, so
    /// the next fold would drop it and every row carrying its codes would lose its key.
    #[test]
    fn an_extension_with_no_vocabulary_refuses_to_open() {
        let err = Vocabularies::seed(
            &[vocabulary("departments", &[])],
            &[declared("department", ScalarType::U8, Some("departments"))],
            &[VocabularyExtension {
                name: "ghosts".to_string(),
                values: vec![ManifestVocabularyValue {
                    key: "k".to_string(),
                    code: 4,
                    title: None,
                }],
            }],
        )
        .expect_err("an extension needs a vocabulary to extend");
        assert!(
            matches!(err, SeedError::ExtensionWithoutVocabulary { .. }),
            "{err}"
        );
    }

    /// **A title travels with its binding, at both ends of the extension path.**
    ///
    /// `extensions_beyond` is the only route by which a value bound between builds reaches a
    /// manifest, and `fold_extensions_into` is what copies it into the next
    /// `MANIFEST.vocabularies` before the rotation reclaims the record it came from. A title
    /// dropped at either end is a title the fold destroys while keeping the key and the code — the
    /// value survives and the name a client draws does not, with nothing to notice.
    ///
    /// The last arm is the upsert (decision 0136's amendment): a title supplied for a value the
    /// manifest already binds makes an extension of its own, and the fold replaces the title the
    /// manifest held.
    #[test]
    fn an_extension_carries_a_title_and_the_fold_writes_it_onto_a_held_value() {
        let mut minter = minter(ScalarType::U16);
        let alpha = minter.mint("alpha").unwrap().code();
        let beta = minter.mint("beta").unwrap().code();
        minter.set_title("alpha", "Alpha".to_string());
        let mut live = Vocabularies::default();
        live.insert(minter);

        // Nothing built, so both bindings are extensions and the titled one carries its title.
        let extensions = live.extensions_beyond(&[]);
        assert_eq!(extensions.len(), 1);
        let carried: Vec<(&str, u32, Option<&str>)> = extensions[0]
            .values
            .iter()
            .map(|v| (v.key.as_str(), v.code, v.title.as_deref()))
            .collect();
        assert_eq!(
            carried,
            [("alpha", alpha, Some("Alpha")), ("beta", beta, None)]
        );

        // The fold: a value the table already holds without a title gains one, and every key and
        // code is untouched.
        let mut table = vec![ManifestVocabulary {
            name: "departments".to_string(),
            kind: VocabularyKind::Declared,
            visibility: Visibility::Derived,
            width: "u16".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "alpha".to_string(),
                code: alpha,
                title: None,
            }],
            reserved: Vec::new(),
        }];
        fold_extensions_into(&mut table, &extensions);
        let folded: Vec<(&str, u32, Option<&str>)> = table[0]
            .values
            .iter()
            .map(|v| (v.key.as_str(), v.code, v.title.as_deref()))
            .collect();
        assert_eq!(
            folded,
            [("alpha", alpha, Some("Alpha")), ("beta", beta, None)],
            "the held value keeps its code and gains the title the page supplied"
        );

        // **A title upserts.** A value the table holds under another title takes the extension's,
        // its key and its code untouched.
        table[0].values[0].title = Some("Authored".to_string());
        fold_extensions_into(&mut table, &extensions);
        assert_eq!(table[0].values[0].title.as_deref(), Some("Alpha"));
        assert_eq!(table[0].values[0].code, alpha);
        // An extension with no title leaves the held one alone: a discovered value carries `None`
        // and must not erase a name.
        assert_eq!(table[0].values[1].title, None);
    }

    /// **A title upserted onto a value the manifest already binds reaches the manifest.** The
    /// binding is not new, so nothing but the changed title makes this value an extension; without
    /// it the upsert would live in the minters alone and the next fold would publish the title the
    /// build wrote (decision 0136's amendment).
    #[test]
    fn a_title_changed_on_a_built_value_is_carried_as_an_extension_and_folded() {
        let built = vec![ManifestVocabulary {
            name: "departments".to_string(),
            kind: VocabularyKind::Declared,
            visibility: Visibility::Derived,
            width: "u16".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "alpha".to_string(),
                code: 41,
                title: Some("Alpha".to_string()),
            }],
            reserved: Vec::new(),
        }];
        let mut live = Vocabularies::seed(&built, &[], &[]).expect("the built table seeds");
        live.get_mut("departments")
            .unwrap()
            .set_title("alpha", "Alpha Team".to_string());

        let extensions = live.extensions_beyond(&built);
        assert_eq!(extensions.len(), 1, "the changed title makes an extension");
        let carried: Vec<(&str, u32, Option<&str>)> = extensions[0]
            .values
            .iter()
            .map(|v| (v.key.as_str(), v.code, v.title.as_deref()))
            .collect();
        assert_eq!(
            carried,
            [("alpha", 41, Some("Alpha Team"))],
            "the key and the code are the manifest's own, restated"
        );

        let mut table = built.clone();
        fold_extensions_into(&mut table, &extensions);
        assert_eq!(table[0].values[0].title.as_deref(), Some("Alpha Team"));
        assert_eq!(table[0].values[0].code, 41);

        // And a restart before the fold: the extension's title beats the manifest's.
        let reopened = Vocabularies::seed(&built, &[], &extensions).expect("the reopen seeds");
        assert_eq!(
            reopened.get("departments").unwrap().title_of("alpha"),
            Some("Alpha Team")
        );

        // A page restating the title the deployment holds makes no extension at all.
        let mut settled = Vocabularies::seed(&table, &[], &[]).expect("the folded table seeds");
        settled
            .get_mut("departments")
            .unwrap()
            .set_title("alpha", "Alpha Team".to_string());
        assert!(settled.extensions_beyond(&table).is_empty());
    }

    /// The fold moves bindings between homes and must not change one. A code that came back
    /// different — re-derived, re-sorted, re-numbered — would recolour every row carrying it, with
    /// no error and no digest mismatch anywhere.
    #[test]
    fn the_fold_moves_bindings_verbatim() {
        let mut built = vec![ManifestVocabulary {
            name: "departments".to_string(),
            kind: VocabularyKind::Declared,
            visibility: Visibility::Derived,
            width: "u32".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "ops".to_string(),
                code: 4711,
                title: Some("Operations".to_string()),
            }],
            reserved: vec![99],
        }];
        let extensions = vec![VocabularyExtension {
            name: "departments".to_string(),
            values: vec![
                ManifestVocabularyValue {
                    key: "k9-unit".to_string(),
                    code: 31_337,
                    title: None,
                },
                // Restated from an earlier manifest: folded once, not twice.
                ManifestVocabularyValue {
                    key: "ops".to_string(),
                    code: 4711,
                    title: None,
                },
            ],
        }];

        fold_extensions_into(&mut built, &extensions);

        let folded: Vec<(&str, u32)> = built[0]
            .values
            .iter()
            .map(|v| (v.key.as_str(), v.code))
            .collect();
        assert_eq!(folded, vec![("ops", 4711), ("k9-unit", 31_337)]);
        assert_eq!(
            built[0].values[0].title.as_deref(),
            Some("Operations"),
            "a restated binding must not strip the title the build gave it"
        );
        assert_eq!(built[0].reserved, vec![99], "retirements are carried");

        // Folding again is a no-op: the next prefix's side-manifest restates an empty set, but a
        // fold that ran twice over the same input must not duplicate a value either.
        fold_extensions_into(&mut built, &extensions);
        assert_eq!(built[0].values.len(), 2);
    }

    /// An extension naming no vocabulary is skipped rather than given a home of its own — the
    /// bundle that carried it would not have opened (`an_extension_with_no_vocabulary_refuses`).
    #[test]
    fn the_fold_skips_an_extension_with_no_vocabulary() {
        let mut built = vec![vocabulary("departments", &[])];
        fold_extensions_into(
            &mut built,
            &[VocabularyExtension {
                name: "ghosts".to_string(),
                values: vec![ManifestVocabularyValue {
                    key: "k".to_string(),
                    code: 4,
                    title: None,
                }],
            }],
        );
        assert_eq!(built.len(), 1);
        assert!(built[0].values.is_empty());
    }

    /// Exhaustion names the column and its width and never widens or wraps: both recolour rows
    /// that already exist. One test at the `u8` boundary — the 255th mint succeeds, the 256th does
    /// not.
    #[test]
    fn the_last_usable_code_mints_and_the_next_refuses() {
        let mut m = minter(ScalarType::U8);
        for i in 0..255 {
            m.mint(&format!("k{i}")).expect("255 usable codes at u8");
        }
        assert_eq!(m.assigned_count(), 255);
        assert!(matches!(
            m.mint("k255"),
            Err(MintError::Exhausted {
                width: ScalarType::U8,
                ..
            })
        ));
        // Code 0 stays absent's, which is why a u8 holds 255 values and not 256.
        assert!(m.bindings().all(|(_, code)| code != ABSENT_CODE));
    }
}
