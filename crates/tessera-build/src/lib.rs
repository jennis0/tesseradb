//! `tessera build` — the batch build.
//!
//! Composes the tiler, the dictionary, the postings writer and the segment writers into one
//! verifiable bundle whose layout is contracts §2.1. Two builds live here and must agree
//! byte-for-byte: [`build`], the streaming one that ships (see [`mod@pipeline`]), and
//! [`build_in_memory`], the linear one retained as its oracle.
//!
//! ## Entity-ID assignment is permanent
//!
//! Items are ordered by their **signature** — the sorted list of their term IDs — and the new
//! entity ID is simply the position in that order (§11.1). Ties break on the external
//! (source-corpus) ID so the assignment is total and deterministic.
//!
//! This is not an optimisation that can be retrofitted. Entity IDs are append-only and never
//! reused (I9), so the ordering chosen at the first build is the ordering the corpus keeps
//! forever; a later build cannot re-sort entity space without invalidating every posting,
//! permutation and handle ever issued. The measured posting compression from this ordering is
//! 8.9–36.7x (`probes/results.md`) — the reason it must be in the first build rather than an
//! optimisation added later. The rule lives in [`signature_sort_key`] as a free function so the
//! serving allocator applies exactly the same rule to appended items.

pub mod artifact_pass;
pub mod check;
mod column;
pub mod config;
pub mod deep;
pub mod disclosure;
pub mod error;
pub mod input;
pub mod layers;
pub mod observer;
mod pipeline;
mod prose;
mod residency;
pub mod shapes;
pub(crate) mod spill;
pub mod unique_key;

use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use arrow::array::{ArrayRef, BinaryArray, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter as ArrowFileWriter;
use arrow::record_batch::RecordBatch;
use sha2::{Digest, Sha256};

use tessera_authz::{write_postings, DictWriter};
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::tiler::{sort_batch, ScalarValue, TilerItem};
use tessera_spatial::{split32, Bounds};
use tessera_store::manifest::{
    identity_key_fingerprint, CurrentPointer, DeclaredScalar, DictExtent, FileDigest,
    IdentityDescriptor, Manifest, ManifestVocabulary, ManifestVocabularyValue, PartitionDescriptor,
    SegmentDescriptor, SegmentsManifest, ViewDescriptor,
};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{write_current, write_manifest_json, PairsParquetWriter};
use tessera_types::{
    EntityId, IdentityKey, TermId, BUNDLE_FORMAT, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS,
    SMALL_TERM_THRESHOLD_DEFAULT,
};

pub use deep::{verify_deep, VerifyDeepReport, VerifyOpts};
pub use disclosure::write_disclosure_report;
pub use error::{BuildError, Result};
// [`BuildArgs::groups`]' own types. A caller assembling build arguments has to name them, and a
// caller that cannot reach `tessera-store` — every test above the store layer — could not
// otherwise declare a group at all (`views.md` §3.2).
pub use observer::{BuildObserver, BuildStage, NoopObserver};
pub use tessera_store::manifest::{
    GroupDescriptor, GroupMetadataField, GroupViewDescriptor, Quantisation, ViewMetadataType,
    ViewMetadataValue,
};
pub use unique_key::{
    footer_distinct_count, keyword_cardinalities, FooterCount, KeywordCardinality,
    UNIQUE_KEY_FRACTION,
};

/// The single bundle prefix a batch build writes. Later publications get their own prefix; the
/// batch build always starts a bundle from scratch.
const PREFIX: &str = "v00000";
/// This build writes exactly one partition: there are no compartments.
const PHASH: &str = "default";
/// One segment per (partition, view) at build (contracts §2.1).
const SEG_ID: &str = "seg-0";
/// The same, for the artifact pass, which reopens the segment the build wrote.
pub(crate) const BUILD_SEG_ID: &str = SEG_ID;

/// The shape layers' geometry report (`polygon-membership.md` §6.5), printed where the build's
/// other reports are — on stderr, before the artifact pass adds the resolution's cost.
pub(crate) fn report_shapes(reports: &[crate::shapes::ShapeLayerReport]) {
    if reports.is_empty() {
        return;
    }
    eprintln!("shape layers, from the geometry alone:");
    for report in reports {
        report.print();
    }
}

/// The treed layers' edges as graphs, printed beside the artifact pass's per-level lines: what a
/// cut climbs, and on a `dag` layer how many artifacts sit under more than one parent
/// (`dag-hierarchies.md` §3, decision 0092).
pub(crate) fn report_hierarchies(shapes: &[crate::layers::HierarchyShape]) {
    for s in shapes {
        eprintln!(
            "  {} level {} [{}]: {} artifact(s), {} edge(s), {} root(s), {} under more than one \
             parent (at most {})",
            s.layer, s.level, s.kind, s.artifacts, s.edges, s.roots, s.multi_parent, s.max_parents
        );
    }
}

/// One coordinate system a build materialises, and where its points come from
/// (`views.md` §7).
///
/// **A view owns everything downstream of the permutation and nothing upstream of it**
/// (`views.md` §1): the projection, the frame, the geometry source and the labels its own rows
/// carry are here; identity, the term index, the attributes and the layers are on
/// [`BuildArgs`], shared by every view of the build.
#[derive(Debug, Clone)]
pub struct ViewArgs {
    /// The view this row space belongs to: a plain view's name, or a group's view as the joined
    /// `group:key` id (`views.md` §3.2). [`tessera_store::view_path`] derives the on-disc path.
    pub view_id: String,
    /// What turns each row's coordinates into a position in this view's frame, before anything is
    /// quantised (`projections.md` §3). [`tessera_spatial::Projection::None`] — the default —
    /// transforms nothing, and is the exact identity.
    pub projection: tessera_spatial::Projection,
    /// The quantisation extent this view's Morton codes are computed against (contracts §2.5),
    /// **per view and never per bundle** (decision 0040): an embedding and a map cannot share a
    /// frame without one of them wasting most of the grid.
    pub extent: Bounds,
    /// Parquet file of this view's points: `entity_id` plus either `x`/`y` or `morton` (see
    /// [`input`]). The view's own `source` (`configuration.md` §1), overridable by
    /// `--file NAME=PATH`.
    pub points: PathBuf,
    /// Where the view's identity and geometry fields sit in that file — the view's `fields` map,
    /// resolved. [`config::Fields::default`] is canonical names throughout.
    pub point_fields: crate::config::Fields,
    /// Which of that file's rows are this view's, where a group's views share one points file
    /// (`views.md` §3.1's form B). `None` where the file *is* the view — every plain view, and
    /// every view of a form A group.
    ///
    /// **Every pass over the file applies it**: the id union, the label vocabulary and its scan,
    /// the geometry read, the frame survey and a group-scoped attribute's own column. A pass that
    /// forgot it would read another view's rows into this view's row space.
    pub select: Option<crate::config::ViewSelector>,
    /// Where each of this view's points gets its access terms, and what a point carrying none
    /// gets — the view's `point_visibility`, resolved.
    ///
    /// **The label is the entity's, not the row's** (`views.md` §7): pass one unions the label
    /// sets a view's rows carry over every view an entity appears in, and a disagreement is a
    /// refusal naming the entity and the files.
    pub access: crate::config::AccessInput,
    /// **This view's own gate** (`views.md` §6), compiled from the declaration
    /// (`config::compile_view_gate`): a list of access labels, each one term (decision 0132), or
    /// `None` for `public`. It reaches the manifest as
    /// [`tessera_store::manifest::ViewDescriptor::visibility`], which is the one input
    /// `Engine::authorise` evaluates a view's own half of the gate from.
    ///
    /// For a view of a group this is the **roster record's** gate — the group's own half is on
    /// [`BuildArgs::groups`], and the two are conjunctive.
    pub visibility: Option<Vec<String>>,
}

/// One group-scoped attribute, and the views of its group whose values this build reads
/// (`views.md` §5).
///
/// **The values are the views' own**, read one of two ways. Where the attribute declares no source
/// of its own, each view's column is read from that view's points file — which for a form B group
/// is the group's shared source under that view's own selection — so the family needs no file of
/// its own: it names the views, and each view already says where its rows are. Where it declares
/// one, that file carries one row per `(entity, view)` and its `fields.view` discriminator says
/// which view each row's value is for.
#[derive(Debug, Clone)]
pub struct ScopedColumnFamily {
    /// The column, exactly as an entity-scoped one is declared.
    pub attribute: crate::config::Attribute,
    /// The group that owns the views — the `<group>` component of the column's path.
    pub group: String,
    /// Indices into [`BuildArgs::views`], one per view of that group, in registry order.
    pub views: Vec<usize>,
    /// The attribute's **own** source (`views.md` §5), or `None` to read each view's column from
    /// that view's points file. The keys a stray discriminator value is refused against are the
    /// group's own and are derived from `views` rather than carried, so the two cannot disagree.
    pub source: Option<crate::config::ScopedAttributeFile>,
}

/// One layer whose artifacts are a different set per view of a group (`views.md` §3.5).
///
/// **A scoped layer's artifact rows say which view each belongs to**, under the layer's own
/// `fields.view`, and an artifact is drawn only in that view: its membership is projected into
/// that view's row space and into no other. The keys are the group's, so a row naming one the
/// roster does not carry is refused, exactly as a points row is.
#[derive(Debug, Clone)]
pub struct ScopedLayer {
    /// The group whose views the artifact sets are per.
    pub group: String,
    /// The discriminator column on the artifacts source — the layer's `fields.view`, resolved.
    pub column: String,
    /// Every key of that group, sorted.
    pub keys: Vec<String>,
}

/// Arguments to [`build`].
#[derive(Clone)]
pub struct BuildArgs {
    /// Every coordinate system this build materialises, in **declaration order**
    /// (`views.md` §7): one entry per plain `[[view]]` and one per view of every
    /// `[[view_group]]`. Declaration order is what decides which view's Morton code an item
    /// absent from the anchor is tie-broken on (decision 0112).
    pub views: Vec<ViewArgs>,
    /// Index into [`BuildArgs::views`] of the **anchor view**: the one whose Morton code orders
    /// entity ids within a signature group (decision 0112, extending 0073).
    ///
    /// `[defaults].allocation_view` names it, and it is **required when the declaration carries
    /// more than one view** — explicit rather than positional, so reordering declaration blocks
    /// cannot silently re-key a rebuild, the ids being permanent (I9).
    pub anchor: usize,
    /// The view groups and their rosters (`views.md` §3.1), in declaration order — what the
    /// manifest publishes so a client can order and name a group's views. Empty for a
    /// declaration of plain views alone.
    ///
    /// **Recorded, never evaluated**: ⊘ no gate is evaluated anywhere (`views.md` §6), so a
    /// roster entry's `visibility` is a record of the declaration rather than a means of
    /// restricting reachability.
    pub groups: Vec<tessera_store::manifest::GroupDescriptor>,
    /// The declared attributes **grouped by the file each is read from**, and the identity column
    /// each group joins on (`configuration.md` §1's `[sources]` and `[defaults]`).
    ///
    /// **A group is a pass.** Each one is a merge sweep over its own file against this build's
    /// assigned ordinals, so a declaration whose columns sit in three files pays three passes and
    /// no file has to carry a column it does not have. Empty for an empty schema, which is what
    /// keeps a schema-less build's `columns.arrow` byte-identical to the one it wrote before this
    /// existed.
    ///
    /// **Separate from the view's own source**, and usually the same file: identity and geometry
    /// are per view, attributes are entity space, and a corpus whose geometry is recomputed does
    /// not rewrite its attributes to say so.
    pub attribute_sources: Vec<crate::config::AttributeSource>,
    /// The **group-scoped attribute column families** this build writes (`views.md` §5): one
    /// entity-space column per view of the group, each with its own presence bitmap, under
    /// `attrs/<column>/<group>/<key>/`.
    ///
    /// **Not part of [`BuildArgs::schema`], and deliberately.** `MANIFEST.declared_scalars` is one
    /// flat bundle-wide list and a family has no slot in it. The family's record is
    /// `MANIFEST.groups[..].scoped_scalars` instead (contracts §2.2), derived from this field at
    /// the manifest write, and it is what the engine opens the columns from and what
    /// `/v1/meta`'s `filter_operands` publishes the scope from.
    ///
    /// **`render` on a family reaches each view's row tail** (`views.md` §5): the column is
    /// permuted into the row space of every view of the group, and of any group sharing them, and
    /// of no other.
    ///
    /// **What a build writes is no longer all there is** (r24). A batch into a view of the owning
    /// group carries the family's values under their plain names, and a view created while the
    /// service runs acquires its columns — and the empty bases beneath them — at the first flush
    /// that covers it. ⊘ The one case that still needs a rebuild is a view of a group declaring
    /// `members` of this one: it renders the family and may not be written through, the column
    /// being the owner's and a second writer for one `(entity, view)` column being two layers
    /// claiming one entity.
    pub scoped_attributes: Vec<ScopedColumnFamily>,
    /// Bundle root to create.
    pub out: PathBuf,
    /// Prefix filter on the *source* entity ID: keep rows with `entity_id < limit`.
    pub limit: Option<u64>,
    /// The deployment's identity key (contracts §2.2). **Not** per bundle: it must be carried
    /// across rebuilds or every `tessera_id` any client holds silently breaks. Resolved by the
    /// CLI from the environment / `--identity-file` / `--carry-id-key-from` / `--mint-id-key`, and
    /// passed here already decided so that both build paths see the same bytes.
    pub identity_key: IdentityKey,
    /// `identity_key`'s canonical 32-lowercase-hex-character form, exactly as MANIFEST records
    /// it. Carried alongside the parsed key rather than recovered from it: `IdentityKey`
    /// deliberately has no hex accessor, to preserve its redacted `Debug` (a hex accessor would
    /// undo the redaction).
    pub identity_key_hex: String,
    /// MANIFEST `identity.idset` (contracts §2.2/§2a): advanced by the CLI when the operator
    /// passes `--bump-idset` or rotates the key, carried forward verbatim on a normal
    /// rebuild, reset to 1 by `--mint-id-key`.
    pub idset: u32,
    /// The §13.3 row-range shard this build produces. Always 0: there is no sharding.
    pub shard_id: u32,
    /// Mint an external ID for every item from its source entity id (8 bytes LE), and write
    /// the external-id extents and `ext-locator.u32`.
    ///
    /// **Off by default, deliberately** (2026-07-30 memo §3.2 D1; CLI `--mint-external-ids`):
    /// contracts §2.4 forbids manufacturing an external ID for an item whose caller supplied
    /// none, and the probe corpus supplies none — so the conformant default build writes no
    /// sidecar at all (the reader is built for that: no extents, no locator, every resolve is
    /// `None`). Bench fixtures pass the flag so they keep carrying the family's cost
    /// realistically, per the owner ruling that made it a representative cost rather than a
    /// reduction target.
    pub mint_external_ids: bool,
    /// The config's `[[layer]]` blocks, compiled, in declaration order — which is registration
    /// order, a layer having to follow every layer it names in `depends_on`. Empty for a bundle
    /// with no layers, which is what every build wrote before this input existed.
    ///
    /// **A build input on the schema's terms** — it compiles into the manifest, and the engine
    /// seeds its registry from there before replaying a WAL record. What it is *not* is a second
    /// authority: the declarations run through the same registry and the same allocator the
    /// control plane uses, so both routes refuse the same declarations and place the same ids.
    pub layers: Vec<tessera_types::layer::LayerDeclaration>,
    /// Where each layer's artifacts come from — its own Parquet of one row per artifact, or the
    /// rows written inline — and the `[layer.members]` source beside it, one row per
    /// `(artifact, entity)`. Parallel to [`BuildArgs::layers`] and refused against it: an input
    /// naming a layer this build does not declare is a name the manifest cannot carry.
    ///
    /// **One source per layer**, so no row carries the layer it belongs to.
    pub layer_inputs: Vec<crate::config::LayerSources>,
    /// Which of [`BuildArgs::layers`] are **scoped to a group** — a different artifact set per
    /// view of it (`views.md` §3.5) — by layer name. Absent is the default `scope = "entity"`:
    /// one artifact set, drawn on every view the layer names.
    pub scoped_layers: BTreeMap<String, ScopedLayer>,
    /// Write `pairs.parquet` (contracts §2.4). On by default; `--no-oracle-pairs` clears it.
    ///
    /// The file is read by nothing on any request path — its consumers are the test-only
    /// Python reference oracle and build-cadence tooling — so a deployment that runs no
    /// conformance suite against the bundle can skip writing and hashing it (~5–7 GB at 10⁹).
    /// A bundle without it is still verifiable: MANIFEST lists only what was written.
    pub emit_oracle_pairs: bool,
    /// Signature-sort batch size, in items (§11.1 r23: assignment is signature-sorted **within
    /// each append-only batch and only within one**; the fragmentation is monotone in batch
    /// count and permanent under I9).
    ///
    /// `None` = derive: the largest batch the memory budget supports, rounded down to a
    /// multiple of 2²⁴ items so budget jitter between machines does not gratuitously fork
    /// identities — usually the whole corpus in one batch, which reproduces the pre-batching
    /// output byte for byte. Whatever is *used* (derived or explicit, when it batches at all)
    /// is recorded in MANIFEST provenance, and an identity-preserving rebuild must replay it:
    /// a different batch size is a different permanent assignment, i.e. a different corpus.
    pub batch_items: Option<u64>,
    /// Peak-RSS budget in bytes for the build's own structures. `None` = detect from the
    /// machine (MemAvailable, damped). Drives batch and band sizing and the fail-closed
    /// pre-flight; it cannot buy off the irreducible floors (the sorted source ids, the
    /// entity-of-ordinal map, the per-term offsets), which the pre-flight states when refusing.
    pub memory_budget: Option<u64>,
    /// Override the derived postings band size, in pre-dedup rows. A tuning and **test** seam
    /// (a corpus small enough for a test cannot force multiple bands through the budget
    /// alone); band boundaries never affect output bytes, only transient memory. `None`
    /// derives from the budget.
    pub band_rows: Option<u64>,
    /// The config's entity-space half: the per-item columns this build writes into
    /// `columns.arrow`'s tail, in declared order, and the vocabularies they draw on (`--config`,
    /// bound value files via `--file`).
    ///
    /// **Default-empty, and that case must stay byte-identical.** Every bundle built before a
    /// config existed declared no scalar, and an empty schema must go on producing exactly the
    /// bytes it did — `tessera-cli`'s identity test asserts a byte-identical `columns.arrow`
    /// across rebuilds carrying one key, and a schema that widened the fixed table by default
    /// would break it for reasons unrelated to identity.
    pub schema: crate::config::Schema,
}

/// **Hand-written, not derived: `identity_key_hex` is the deployment key in plaintext.**
/// `IdentityKey`'s `Debug` is redacted and it has no hex accessor, but a derived `Debug` here
/// would print the hex carried beside it — so one `tracing::error!("{args:?}")` on a build
/// failure would put the deployment key in a log. The redaction is only worth as much as its
/// weakest carrier.
impl std::fmt::Debug for BuildArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildArgs")
            .field("views", &self.views)
            .field("anchor", &self.anchor)
            .field("groups", &self.groups)
            .field("scoped_attributes", &self.scoped_attributes)
            .field("attribute_sources", &self.attribute_sources)
            .field("out", &self.out)
            .field("limit", &self.limit)
            .field("identity_key", &self.identity_key)
            .field(
                "identity_key_hex",
                &identity_key_fingerprint(&self.identity_key_hex),
            )
            .field("idset", &self.idset)
            .field("shard_id", &self.shard_id)
            .field("mint_external_ids", &self.mint_external_ids)
            .field("emit_oracle_pairs", &self.emit_oracle_pairs)
            .field("batch_items", &self.batch_items)
            .field("memory_budget", &self.memory_budget)
            .field("band_rows", &self.band_rows)
            .finish()
    }
}

/// What a completed build produced.
#[derive(Debug, Clone)]
pub struct ViewReport {
    /// The view id: a plain view's name, or a group's view as `group:key` (`views.md` §3.2).
    pub view_id: String,
    /// Rows in this view's segment — the view's population, which is a **subset** of entity
    /// space wherever the view does not hold every item (`views.md` §8).
    pub rows: u64,
    /// What this view's frame gave the corpus, counted off its own sorted Morton codes.
    pub occupancy: Occupancy,
}

/// What a completed build produced, per view.
#[derive(Debug, Clone)]
pub struct BuildReport {
    pub prefix: String,
    /// One entry per view the build materialised, in registry order (`views.md` §7).
    pub views: Vec<ViewReport>,
    pub seg_id: String,
    /// Number of items (= `entity_id_high_water`, since the bootstrap build allocates from 0).
    pub items: u64,
    /// Number of distinct terms in the dictionary.
    pub terms: u64,
    /// Number of `(entity, term)` pairs written.
    pub pairs: u64,
    /// Total size on disk of every file the manifests name.
    pub bundle_bytes: u64,
    /// Member rows whose key said *this point is in no artifact* — a null key, or exactly `-1`
    /// (`artifacts-from-points.md` §2). Noise is a quarter of the points at each split of a
    /// condensed tree, so this is an ordinary number rather than a fault; it is here because a
    /// clustering that skipped *every* row named the wrong column, and only the count says so.
    pub unclustered_member_rows: u64,
    /// Artifacts **created by a member key no artifacts source declared**, under
    /// `value_set = "open"` (`artifacts-from-points.md` §3). The ordinary number for a bare
    /// clustering is *every* cluster, so this is not a fault either; it is here because minting
    /// cannot be undone — a mistyped key becomes a permanent object — and the count is the whole of
    /// what stands between an operator and noticing. An ingest batch reports the same number for
    /// itself in its own 200.
    pub minted_artifacts: u64,
    /// Every `(view, layer, level)` the post-bundle artifact pass observed, in the order the
    /// views were built (`crate::artifact_pass`).
    ///
    /// **Returned as well as printed, because a scoped layer's per-view separation is only
    /// visible here** (`views.md` §3.5): an artifact belongs to one view, so the artifact count
    /// with rows in a view is the layer's own set there and not the level's whole roster.
    pub artifact_levels: Vec<crate::artifact_pass::LevelLayoutReport>,
    /// Per treed level, its edges as a graph (decision 0092's report, for the edges).
    pub hierarchy_shapes: Vec<crate::layers::HierarchyShape>,
    /// What each declared attribute source's join met — the figures
    /// [`report_attribute_coverage`] prints, returned as well as printed.
    ///
    /// **Returned because the tallies are computed where nothing else can check them.** The
    /// streaming pipeline counts a column's presence inside a scatter it splits across threads,
    /// and a tally that lost or double-counted a lane would change this report without changing
    /// one byte of the bundle — the one defect a bundle comparison cannot see. The linear build
    /// counts the same thing serially, so the two are comparable (`tests/attribute_pass.rs`).
    pub attribute_coverage: Vec<AttributeCoverage>,
    /// Each indexed keyword column's distinct-key count against the rows carrying one, with the
    /// bytes its index cost (`unique_key`). Printed at every build; a key unique per row earns a
    /// warning and never a refusal.
    pub keyword_cardinalities: Vec<KeywordCardinality>,
}

/// **How many of the grid's cells the placed points actually landed in**, beside how many points
/// there were.
///
/// A frame goes wrong in two ways and the clamp count (`config::Frame`) sees only one of them.
/// Data *outside* the frame is pushed onto its edge, so those positions are actively wrong — that
/// is the clamp, and past half the corpus it is a refusal. Data *tiny inside* the frame clamps
/// nothing at all: every position is correct, and nearly all of the resolution is gone, because
/// points a long way apart in the source land in one cell and can no longer be told apart.
/// Coordinates spanning 100…118 against a 0…65536 frame do this with zero clamps.
///
/// **The frame report's bounding box cannot close that gap**, because it is derived from the
/// data's extremes: two far-flung outliers make the box span most of the grid while 99% of the
/// corpus still shares a handful of cells. The number that cannot be fooled that way is how many
/// cells hold at least one point, counted exactly over every point the build placed.
///
/// **A warning, never a refusal.** A clamped corpus is stored *wrong* and is worth stopping for; a
/// sparse one is stored *correctly but coarsely*, which is a legitimate thing to want — a small
/// pilot corpus, a deliberately coarse frame, headroom left for data still to arrive. Refusing it
/// would block builds the caller meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occupancy {
    /// Points placed — one per row written.
    pub points: u64,
    /// Distinct cells those points landed in. Zero only for a build with no points, which is
    /// refused before it reaches here.
    pub cells: u64,
}

/// The average points per occupied cell at which the build says so emphatically.
///
/// **Ten, because nothing at this system's operating point reaches it on a frame that fits.**
/// 4.3×10⁹ cells exist, so points spread over the whole grid collide rarely — about 1.0 per
/// occupied cell at 10⁸ and 1.1 at 10⁹ — and a corpus concentrated into a tenth of the frame's
/// area still only reaches 1.1 at 10⁸. The measure is a ratio rather than a count for a second
/// reason: ten points in ten cells is a perfectly framed tiny corpus, not a sparse one, and only a
/// count would scold it.
///
/// **The bound is not scale-free, and the limit is stated rather than left to be discovered.** A
/// uniform corpus's ratio climbs with points per *available* cell, so it rises with both scale and
/// concentration: at 10⁹ points in a hundredth of the frame's area it reaches about 23 and warns
/// even under a frame `auto` fitted, a dense core with two distant outliers being exactly that
/// corpus. Below 10⁹ no concentration reaches ten. This is a warning and not a refusal, so the
/// cost of that case is a line of output.
///
/// The one corpus this warns about honestly and unhelpfully is a source whose positions genuinely
/// coincide — a hundred documents recorded at one place really are one point. The raw numbers are
/// printed either way, beside the data's own bounds in the frame report, which is what lets a
/// caller tell the two apart.
pub const COLLAPSE_WARNING_POINTS_PER_CELL: f64 = 10.0;

impl Occupancy {
    /// Count the distinct cells over a build's Morton codes **in tiler order**.
    ///
    /// **A run count over the sorted codes, not a bitmap.** Both builds reach this holding the
    /// codes they are about to write to `morton.u32`, which is `(morton, tessera_id)` ascending
    /// by contract (contracts §2.6 r6) — so equal codes are adjacent and the exact answer is one
    /// comparison per point with nothing retained. A Roaring bitmap of the codes gives the same
    /// exact answer for unsorted input, and is what this would need if the count moved anywhere
    /// else; here it would allocate up to half a gigabyte (65,536 dense containers at 10⁹ points)
    /// at the segment write, which is precisely the stage the pipeline holds as little as possible
    /// beside. The order is asserted rather than assumed — out of order, a run count silently
    /// over-reports, and an over-report is a warning that does not fire.
    pub(crate) fn of_sorted_codes(codes: impl IntoIterator<Item = u32>) -> Occupancy {
        let (mut points, mut cells) = (0u64, 0u64);
        let mut previous: Option<u32> = None;
        for code in codes {
            points += 1;
            match previous {
                Some(last) => {
                    assert!(
                        code >= last,
                        "occupancy: Morton codes must arrive in tiler order ({last} then {code})"
                    );
                    cells += u64::from(code != last);
                }
                None => cells = 1,
            }
            previous = Some(code);
        }
        Occupancy { points, cells }
    }

    /// Points per occupied cell — one where every point has a cell to itself, and the factor by
    /// which the frame is coarser than this corpus needs where it is more. The reporting form is
    /// [`Self::distinct_fraction`]; this is what the threshold is expressed in.
    pub fn points_per_cell(&self) -> f64 {
        if self.cells == 0 {
            0.0
        } else {
            self.points as f64 / self.cells as f64
        }
    }

    /// **What is reported: how many of this corpus's points have a position of their own.** One
    /// hundred per cent is a point per cell; ten per cent means nine points in ten share a
    /// position with another and cannot be told apart.
    ///
    /// The obvious reading of "how full is the grid" — occupied cells over the 4.3×10⁹ that exist
    /// — is unreadable and nearly always wrong-looking: a corpus can never occupy more cells than
    /// it has points, so a perfectly framed ten-thousand-point build fills 0.0002% of the grid and
    /// a collapsed one fills 0.000003%. Both round to nothing, and the figure measures corpus size
    /// far more than it measures the frame. Against the corpus's own points the same two builds
    /// read 100% and 1.2%.
    pub fn distinct_fraction(&self) -> f64 {
        if self.points == 0 {
            return 1.0;
        }
        self.cells as f64 / self.points as f64
    }

    /// **What the build says about resolution, every time, whether or not anything is wrong.**
    /// The raw numbers, so a caller can judge a frame this does not warn about — and so silence
    /// never means nobody looked.
    pub fn report(&self, view: &str) -> String {
        format!(
            "view '{view}': {} point(s) landed in {} distinct cell(s) — {:.1}% of them have a \
             position of their own",
            self.points,
            self.cells,
            self.distinct_fraction() * 100.0
        )
    }

    /// The emphatic line this occupancy earns, if any — past
    /// [`COLLAPSE_WARNING_POINTS_PER_CELL`], and never a refusal.
    pub fn warning(&self, view: &str) -> Option<String> {
        if self.points_per_cell() < COLLAPSE_WARNING_POINTS_PER_CELL {
            return None;
        }
        Some(format!(
            "view '{view}': RESOLUTION LOST — only {:.1}% of these points have a position of \
             their own, so points far apart in the source are stored at the same position and \
             cannot be told apart. That is a frame far wider than the data it holds, unless the \
             source's own positions genuinely coincide. Built anyway: every position written is \
             correct, only coarse, and a pilot corpus, a deliberately wide frame or headroom for \
             data still to arrive are all reasons to mean it. Write `extent = \"auto\"` to fit \
             the frame to this data if it was not intended",
            self.distinct_fraction() * 100.0
        ))
    }
}

/// The signature-sorted assignment key (§11.1): an item's **sorted term-ID list**.
///
/// Items are ordered by this key lexicographically, ties broken by external ID, and each item's
/// new entity ID is its position in that order. Items with identical term sets therefore occupy
/// a contiguous entity-ID range, which is what turns their postings into runs — the measured
/// 8.9–36.7x compression. **Permanent under I9:** entity IDs are never reused, so this ordering
/// cannot be changed after the first build. The serving allocator calls this same function.
pub fn signature_sort_key(terms: &[TermId]) -> Vec<u32> {
    let mut key: Vec<u32> = terms.iter().map(|t| t.raw()).collect();
    key.sort_unstable();
    key.dedup();
    key
}

/// What a build needs in hand before it can turn a row into a term id: the descriptor every source
/// term names, and the source term a point carrying nothing is filled with.
///
/// **Established once, before either build's first pass**, because both passes of the streaming
/// build and the single pass of the linear one must agree on it exactly — a source term is a
/// position in a sorted vocabulary for a field-sourced view, and a disagreement about that list is
/// a disagreement about every permanent entity id (I9).
pub(crate) struct AccessPlan {
    pub descriptors: input::TermDescriptors,
    /// The source term a point of view `v` carrying none is given, indexed by
    /// [`BuildArgs::views`]. Meaningless for the relation route, which fills nothing.
    ///
    /// **One vocabulary, one term per view's default** (`views.md` §7): term ids are entity
    /// space and every view's labels are interned into the same dictionary, so the vocabulary is
    /// the union over every view's source; what stays per view is which of its entries an
    /// unlabelled point of that view takes.
    pub default_term: Vec<u64>,
}

/// How a build's views declare where their labels come from, checked once (`views.md` §7).
///
/// **A label is the entity's, not the row's.** The two routes cannot be mixed across the views of
/// one build, and two relations cannot be: an entity's term set has to be one set, and there is
/// nothing to check a second relation's disagreement against — the field route's per-view sets are
/// compared entity by entity (the count identity in [`pipeline`]'s batch loop), which a relation
/// carrying entity-space pairs is outside of.
enum AccessRoute<'a> {
    /// Every view reads its labels from a column of its own points file, or takes its default.
    /// One vocabulary over every view's distinct values, and one set per (entity, view) to agree.
    PerView,
    /// Every view names the same exploded `(entity_id, term_id)` relation. Entity space already,
    /// so it is scanned once and there is nothing to disagree.
    SharedRelation(&'a std::path::Path),
}

/// Which route this build's views declare, refusing a mixture.
fn access_route(args: &BuildArgs) -> Result<AccessRoute<'_>> {
    use crate::config::AccessSource;
    let mut relation: Option<&std::path::Path> = None;
    let mut per_view: Option<&str> = None;
    for view in &args.views {
        match &view.access.source {
            AccessSource::Relation(path) => match relation {
                None => relation = Some(path.as_path()),
                Some(first) if first == path.as_path() => {}
                Some(first) => {
                    return Err(BuildError::Invalid(format!(
                        "view '{}' reads its labels from {} and another view reads them from {}. \
                         A label is the entity's, not the row's (views §7), so two relations \
                         would be two answers to one question with nothing to reconcile them",
                        view.view_id,
                        path.display(),
                        first.display()
                    )))
                }
            },
            AccessSource::Field(_) | AccessSource::Default => per_view = Some(&view.view_id),
        }
    }
    match (relation, per_view) {
        (Some(path), None) => Ok(AccessRoute::SharedRelation(path)),
        (None, _) => Ok(AccessRoute::PerView),
        (Some(path), Some(view)) => Err(BuildError::Invalid(format!(
            "view '{view}' reads its labels from its own points file and another view reads them \
             from the relation {}. A build's views must declare one route (views §7): an \
             entity's label is one set, and the two routes cannot be checked against each other",
            path.display()
        ))),
    }
}

/// Read whatever a build must know before assigning term ids (see [`AccessPlan`]).
pub(crate) fn plan_access(args: &BuildArgs) -> Result<AccessPlan> {
    use crate::config::AccessSource;
    if let AccessRoute::SharedRelation(_) = access_route(args)? {
        // The relation supplies its own integer term ids and needs no vocabulary pass.
        return Ok(AccessPlan {
            descriptors: input::TermDescriptors::Ids,
            default_term: vec![0; args.views.len()],
        });
    }
    // **The union, sorted** — one dictionary over every view's distinct values. With one view
    // this is that view's own sorted vocabulary, unchanged, which is what keeps a single-view
    // bundle byte-identical across this change.
    let mut vocabulary: Vec<String> = Vec::new();
    for view in &args.views {
        let field = match &view.access.source {
            AccessSource::Field(field) => Some(field.as_str()),
            _ => None,
        };
        vocabulary.extend(input::read_access_vocabulary(
            &view.points,
            &view.point_fields,
            field,
            &view.access.default,
            args.limit,
            view.select.as_ref(),
        )?);
    }
    vocabulary.sort_unstable();
    vocabulary.dedup();
    let default_term = args
        .views
        .iter()
        .map(|view| {
            vocabulary
                .binary_search_by(|t| t.as_str().cmp(&view.access.default))
                .expect("every view's default is read into the vocabulary unconditionally")
                as u64
        })
        .collect();
    Ok(AccessPlan {
        descriptors: input::TermDescriptors::Vocabulary(vocabulary),
        default_term,
    })
}

/// Walk every view's access relation, whichever of the three shapes declared it, as
/// `(view index, source entity id, source term)`.
///
/// **Every view, in [`BuildArgs::views`] order**, because entity space is unioned over them
/// (`views.md` §7). The view index is what lets the caller count each view's contribution
/// separately, which is how the label-agreement refusal is made exact.
pub(crate) fn scan_access<F: FnMut(usize, u64, u64) -> std::ops::ControlFlow<()>>(
    args: &BuildArgs,
    plan: &AccessPlan,
    mut visit: F,
) -> Result<input::AccessFill> {
    use crate::config::AccessSource;
    if let AccessRoute::SharedRelation(path) = access_route(args)? {
        // Scanned **once**, not once per view: its rows are entity space, and a second pass over
        // them would double every posting.
        input::scan_pairs(path, &access_fields(args), args.limit, |id, term| {
            visit(0, id, term)
        })?;
        return Ok(input::AccessFill::default());
    }
    let input::TermDescriptors::Vocabulary(vocabulary) = &plan.descriptors else {
        unreachable!("planned by `plan_access` together")
    };
    let mut fill = input::AccessFill::default();
    for (index, view) in args.views.iter().enumerate() {
        let one = input::scan_access_field(
            &view.points,
            &view.point_fields,
            match &view.access.source {
                AccessSource::Field(field) => Some(field.as_str()),
                _ => None,
            },
            vocabulary,
            plan.default_term[index],
            args.limit,
            view.select.as_ref(),
            |id, term| visit(index, id, term),
        )?;
        fill.carried += one.carried;
        fill.filled += one.filled;
    }
    Ok(fill)
}

/// Say how many points carried terms of their own and how many took the view's default.
///
/// **Printed rather than merely counted** (`configuration.md` §5): a fill is a visibility
/// decision the build made on the caller's behalf, and a corpus that turned out to be almost
/// entirely default is one whose author wants to know before it is served.
pub(crate) fn report_access_fill(args: &BuildArgs, fill: input::AccessFill) {
    if fill.filled == 0 {
        return;
    }
    // Totalled over every view the build reads, which is what the number means: a point in two
    // views carries its label in both, and the fill is a property of the corpus rather than of
    // one row space.
    eprintln!(
        "{} view(s): {} point row(s) carried access terms of their own; {} took the declared \
         default",
        args.views.len(),
        fill.carried,
        fill.filled
    );
}

/// What one attribute source's join actually met: how many of this build's entities came away with
/// a value, and how many of the source's rows named an entity the build never loaded.
///
/// **Entities covered is the denominator, and rows dropped is not.** A legitimate superset and a
/// broken join both drop an overwhelming fraction of their rows — a sentiment table covering every
/// paper arXiv ever published against a 50,000-paper build drops 97% of itself and is perfectly
/// correct — so the number that separates the two is how much of *this* corpus came away with a
/// value. Both are printed, and only the first is the measure.
#[derive(Debug, Clone)]
pub struct AttributeCoverage {
    /// The caller's own name for the source, from `[sources]`.
    pub source: String,
    /// Entities in this build — the denominator.
    pub entities: u64,
    /// Rows of this source that resolved to one of them.
    pub matched_rows: u64,
    /// Rows that named an entity this build did not load. **Ignored, never refused**: that is
    /// what a join does, and it is fail-closed in both directions that matter — an absent
    /// attribute matches fewer points in a filter, and an absent access label leaves a point
    /// visible to nobody.
    pub unknown_rows: u64,
    /// Each declared column this source carries, and how many entities came away with a value in
    /// it. Fewer than [`AttributeCoverage::matched_rows`] where the source itself holds nulls.
    pub columns: Vec<(String, u64)>,
}

/// A count with thousands separators, because these are the numbers an operator compares by eye.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// **What each attribute source's join met, printed at every build** (`configuration.md` §1).
///
/// Reported and **never refused**, on the rule that separates a disclosure boundary from a build
/// input: an unmatched row discloses nothing and costs a rerun, and refusing would block the
/// legitimate case — a source covering a superset of this build's entities — as loudly as the
/// broken one. Zero coverage says so emphatically and still builds, which is the owner's ruling
/// twice over.
pub(crate) fn report_attribute_coverage(coverage: &[AttributeCoverage]) {
    for source in coverage {
        for (name, present) in &source.columns {
            if *present == 0 && source.entities > 0 {
                eprintln!(
                    "attribute '{name}': NO ENTITY HAS A VALUE — 0 of {} entities matched",
                    thousands(source.entities)
                );
            } else {
                eprintln!(
                    "attribute '{name}': {} of {} entities have a value",
                    thousands(*present),
                    thousands(source.entities)
                );
            }
        }
        eprintln!(
            "        {} source row(s) named entities this build did not load",
            thousands(source.unknown_rows)
        );
        // The join did not meet at all. Emphatic and not a refusal: the ids may simply be another
        // corpus's, and only the operator knows which — but a build that says nothing here ships a
        // bundle whose every declared column is empty.
        if source.matched_rows == 0 && source.entities > 0 {
            eprintln!(
                "        source '{}' AND THIS BUILD'S ENTITY SPACE DO NOT MEET — not one of its \
                 rows named an entity this build loaded. The join is on the entity id: check that \
                 `entity_id_field` names the column carrying it, and that these are the same ids \
                 the view's own source carries",
                source.source
            );
        }
    }
}

/// The access relation's own field names — canonical, and named for a refusal to quote.
///
/// `point_visibility` takes no `fields` map of its own (`configuration.md` §1): the exploded
/// relation is `entity_id` and `term_id` under those names. What this carries is the *object*, so
/// a file missing one of them is refused naming the view whose labels went unread.
fn access_fields(args: &BuildArgs) -> crate::config::Fields {
    crate::config::Fields::canonical(format!(
        "view '{}' point_visibility",
        args.views[args.anchor].view_id
    ))
}

/// Argument and destination checks shared by both build implementations.
fn validate_args(args: &BuildArgs) -> Result<()> {
    if args.views.is_empty() {
        return Err(BuildError::Invalid(
            "this build materialises no view, so it has no coordinate system to write a row \
             space in (views §7)"
                .into(),
        ));
    }
    if args.anchor >= args.views.len() {
        return Err(BuildError::Invalid(format!(
            "the anchor view is index {} of {} declared (decision 0112)",
            args.anchor,
            args.views.len()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(args.views.len());
    for view in &args.views {
        view.extent.validate().map_err(|detail| {
            BuildError::Invalid(format!("view '{}' extent: {detail}", view.view_id))
        })?;
        if seen.contains(&view.view_id.as_str()) {
            return Err(BuildError::Invalid(format!(
                "view '{}' is materialised twice; a view id names one row space (views §2)",
                view.view_id
            )));
        }
        seen.push(&view.view_id);
    }
    if args.batch_items == Some(0) {
        return Err(BuildError::Invalid(
            "--batch-items 0 is meaningless; omit it for a single batch".into(),
        ));
    }
    // An artifact names the layer it belongs to, so a layer file is what makes those names
    // resolvable. Refused rather than ignored: a build that quietly dropped the artifacts would
    // produce a bundle whose clusters are absent, which no viewer can tell from clusters that
    // failed their existence criterion.
    if args.layers.is_empty() && !args.layer_inputs.is_empty() {
        return Err(BuildError::Invalid(
            "an artifact source names artifacts in layers, and the config declares no `[[layer]]` \
             block for them to belong to"
                .into(),
        ));
    }
    // Every declared attribute must be read from somewhere, and the declaration refuses one that
    // is not, naming the columns (`config::Config::acquire`, `configuration.md` §1's
    // `[defaults]`); this is the same rule for the callers that build these arguments directly.
    // A column no group carries would be written as the absent sentinel for every row — a column
    // that cost its width to say nothing.
    if !args.schema.is_empty() {
        let mut carried: Vec<usize> = args
            .attribute_sources
            .iter()
            .flat_map(|s| s.attributes.iter().copied())
            .collect();
        carried.sort_unstable();
        let missing: Vec<&str> = (0..args.schema.attributes.len())
            .filter(|i| carried.binary_search(i).is_err())
            .map(|i| args.schema.attributes[i].name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(BuildError::Invalid(format!(
                "the schema declares {} attribute(s) and no source is bound to read {} of them \
                 from: {}. Every column names a `[sources]` key or takes `[defaults].source` \
                 (configuration.md §1)",
                args.schema.attributes.len(),
                missing.len(),
                missing.join(", ")
            )));
        }
    }
    // **The path is derived from the id, never the id used as a path** (`views.md` §3.2): a
    // group's view is `group:key` and lives at `views/<group>/<key>/`, so what has to be safe is
    // each component [`tessera_store::view_path`] derives, not the joined form.
    for view in &args.views {
        for component in tessera_store::view_path_components(&view.view_id) {
            if component.is_empty()
                || component.contains('/')
                || component.contains('\\')
                || component == "."
                || component == ".."
            {
                return Err(BuildError::Invalid(format!(
                    "view id '{}' is not a safe path: '{component}'",
                    view.view_id
                )));
            }
        }
    }

    // A batch build always writes prefix `v00000`, so building into a directory that already
    // holds a bundle would leave that bundle's files half-overwritten while its `CURRENT` still
    // points at them — and a stale higher-numbered `SEGMENTS-<n>.json` left behind would be the
    // one the reader picks. Refuse rather than produce that state (fail closed).
    if args.out.join("CURRENT").exists() {
        return Err(BuildError::Invalid(format!(
            "{} already contains a bundle (CURRENT exists); remove it or choose another --out",
            args.out.display()
        )));
    }
    Ok(())
}

/// One item after labelling, before entity-ID assignment.
///
/// The item's term set is held only as its `signature` — [`signature_sort_key`]'s sorted,
/// deduplicated term-ID list. That is both the ordering key and the postings input, so keeping a
/// second, unsorted copy alongside it would only create a way for the two to disagree.
struct StagedItem {
    source_id: u64,
    /// 32-bit fixed point per axis against the build extent, as `input::PointRow` carries it —
    /// not coordinates. Same width as the `f32` pair it replaces.
    qx: u32,
    qy: u32,
    /// The `split32` cell code of `(qx, qy)`, held rather than recomputed because it is a sort
    /// key: entity-id ties within a signature group break on it (decision 0073).
    morton: u32,
    signature: Vec<u32>,
}

/// Run the batch build, producing a complete bundle at `args.out`.
///
/// This is [`pipeline::build`] — the streaming pipeline, which holds a bounded set of packed
/// arrays rather than one struct per item. [`build_in_memory`] is the older, linear
/// implementation, kept as the byte-equality oracle the two are tested against.
pub fn build(args: &BuildArgs) -> Result<BuildReport> {
    pipeline::build(args, &observer::NoopObserver)
}

/// [`build`], reporting each pipeline stage's duration to `observer` as it completes.
///
/// Identical to `build` in every respect but the notifications — the same code path, not a
/// parallel one, so a measurement taken here describes the build that actually ships. Exists for
/// `tessera-bench`'s ingest arm, which asks which of the eleven stages bends with scale.
pub fn build_observed(
    args: &BuildArgs,
    observer: &dyn observer::BuildObserver,
) -> Result<BuildReport> {
    pipeline::build(args, observer)
}

/// The linear, fully in-memory build.
///
/// Superseded by [`build`] for anything but small inputs — it materialises one [`StagedItem`]
/// per point and the whole `per_term` posting relation before writing a byte, which at 10⁹
/// items is tens of gigabytes. It is retained, and exercised by
/// `tests/build_equivalence.rs`, as the **oracle** for the streaming pipeline: the two must
/// produce byte-identical bundles for any input, because the entity-ID assignment they encode
/// is permanent (I9) and every digest in the bundle depends on it.
pub fn build_in_memory(args: &BuildArgs) -> Result<BuildReport> {
    validate_args(args)?;
    // **The oracle materialises one view.** It exists to be the byte-equality reference for the
    // streaming pipeline's entity-id assignment, and a second implementation of pass one's union
    // would be a second thing to keep in step rather than a check on the first. A multi-view
    // declaration goes through `build` (`views.md` §7).
    let [view] = args.views.as_slice() else {
        return Err(BuildError::Invalid(format!(
            "the linear build materialises one view and this build declares {}: {}. It is the \
             byte-equality oracle for the streaming pipeline, not a second multi-view build \
             (views §7)",
            args.views.len(),
            args.views
                .iter()
                .map(|v| v.view_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };

    // **And one entity space, with no column family in it.** The oracle exists to be the
    // byte-equality reference for the streaming build's entity-id assignment; a second
    // implementation of the scoped families would be a second thing to keep in step rather than a
    // check on the first, and they change no byte of what this build writes.
    if let Some(family) = args.scoped_attributes.first() {
        return Err(BuildError::Invalid(format!(
            "the linear build writes no group-scoped column family, and this build declares one: \
             '{}' over group '{}'. It is the byte-equality oracle for the streaming pipeline \
             (views §5)",
            family.attribute.name, family.group
        )));
    }

    // ---- 1. read inputs --------------------------------------------------------------
    let mut points = input::read_points(
        &view.points,
        &view.point_fields,
        view.projection,
        &view.extent,
        args.limit,
        view.select.as_ref(),
    )?;
    // **No refusal for an empty points file** (decision 0091): the oracle writes the same
    // zero-item bundle the streaming pipeline does, which is what keeps `build_equivalence`'s
    // byte-identity claim true for `n = 0` as for any other n.
    // Sort by source ID before anything else: term IDs are assigned in first-appearance order,
    // so a stable, file-order-independent iteration is what makes the dictionary (and hence the
    // signature ordering, and hence the permanent entity IDs) reproducible from the same input
    // regardless of how the source file happens to be laid out.
    points.sort_unstable_by_key(|p| p.source_id);
    if points.windows(2).any(|w| w[0].source_id == w[1].source_id) {
        return Err(BuildError::Invalid(
            "points file contains duplicate entity_id values".into(),
        ));
    }
    // What every source term will be called, before any term id exists — a field-sourced view's
    // sorted vocabulary, or the relation's own integers (`AccessPlan`).
    let access = plan_access(args)?;
    let mut pairs_by_source: HashMap<u64, Vec<u64>> = HashMap::new();
    let fill = scan_access(args, &access, |_view, source_id, source_term| {
        pairs_by_source
            .entry(source_id)
            .or_default()
            .push(source_term);
        std::ops::ControlFlow::Continue(())
    })?;
    report_access_fill(args, fill);
    // Sorted and deduplicated per item: the label set is a *set*, and the signature key and the
    // postings writer both depend on it being one.
    for terms in pairs_by_source.values_mut() {
        terms.sort_unstable();
        terms.dedup();
    }

    // ---- 2. label each item through the plugin, interning descriptors ----------------
    let plugin = Passthrough::new();
    let bounds = plugin.declared_bounds();
    let dict_dir = args.out.join(PREFIX).join("dictionary");
    fs::create_dir_all(&dict_dir).map_err(|e| BuildError::io(&dict_dir, e))?;
    let mut dict = DictWriter::new(&dict_dir);

    // **`public` is interned first, so it is term 0 in every bundle** and is minted for no other
    // descriptor (`tessera_authz::PUBLIC_TERM`). Reserved unconditionally, whether or not any point
    // carries it: the label's identity has to be a property of the format rather than of the input,
    // since `Engine::authorise` adds it to every principal's satisfied set and a term whose number
    // moved with the data would make that addition mean something different per bundle.
    dict.intern(tessera_authz::PUBLIC_LABEL);

    let mut staged: Vec<StagedItem> = Vec::with_capacity(points.len());
    let mut over_bound_items = 0u64;
    for point in &points {
        let source_terms = pairs_by_source.remove(&point.source_id).unwrap_or_default();
        let labels: Vec<Vec<u8>> = source_terms
            .iter()
            .map(|t| access.descriptors.descriptor(*t).into_owned().into_bytes())
            .collect();
        let descriptors = plugin.terms_of_labels(&labels)?;
        if descriptors.len() > bounds.max_terms_per_item as usize {
            // A declared bound is a *declaration*: record it and carry on. Dropping terms here
            // would silently widen the item's visibility (I2/I3).
            //
            // This counts descriptors, and the streaming pipeline counts the item's signature
            // length; the two always agree. `read_pairs` returns each item's source terms sorted
            // and deduplicated, so the label list has distinct elements, passthrough yields one
            // distinct descriptor per element, and interning is injective — the descriptor count
            // *is* the distinct term count, which is what a signature holds.
            over_bound_items += 1;
        }
        let terms: Vec<TermId> = descriptors.iter().map(|d| dict.intern(d)).collect();
        staged.push(StagedItem {
            source_id: point.source_id,
            qx: point.qx,
            qy: point.qy,
            morton: split32(point.qx, point.qy).0.raw(),
            signature: signature_sort_key(&terms),
        });
    }
    if !pairs_by_source.is_empty() {
        return Err(BuildError::Invalid(format!(
            "pairs file references {} entity ids absent from the points file (first: {})",
            pairs_by_source.len(),
            pairs_by_source.keys().min().copied().unwrap_or_default()
        )));
    }
    if over_bound_items > 0 {
        eprintln!(
            "warning: {over_bound_items} item(s) exceed the plugin's declared \
             max_terms_per_item ({}); no term was dropped",
            bounds.max_terms_per_item
        );
    }

    // ---- 3. signature-sorted entity-ID assignment (I9, permanent — see module docs) ---
    // §11.1 r23: the sort's scope is one batch. `staged` is in ascending source-id order
    // (the sort above), i.e. ordinal order, so a batch is a contiguous chunk; each chunk is
    // signature-sorted independently and the concatenation is the batch-major assignment.
    // `None` (or one covering chunk) reproduces the historical global sort exactly.
    let batch = args
        .batch_items
        .unwrap_or(u64::MAX)
        .min(staged.len().max(1) as u64) as usize;
    // `(signature, morton, source_id)` — decision 0073. The Morton code is the minor key, so a
    // spatially coherent set lands in a contiguous run of entity ids; the source id survives
    // beneath it to keep the order total, since two items may share a cell.
    for chunk in staged.chunks_mut(batch) {
        chunk.sort_by(|a, b| {
            a.signature
                .cmp(&b.signature)
                .then(a.morton.cmp(&b.morton))
                .then(a.source_id.cmp(&b.source_id))
        });
    }
    let n = staged.len() as u64;
    if n > u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{n} items exceeds bundle_format 1's 2^32 entity-ID ceiling"
        )));
    }

    // The dictionary is the authority on how many terms exist — `max(term_id) + 1` over the
    // items would agree only as long as every interned term is still carried by some item, and
    // a postings file shorter than the dictionary would silently make its tail terms unaskable.
    let term_count = dict.len() as u64;

    // ---- 4/5. postings, pairs, external ids ------------------------------------------
    // Built by walking items in new-entity-ID order, so every per-term list comes out sorted
    // strictly ascending without a further sort — which is exactly what `write_postings`
    // requires (it rejects unsorted input rather than silently repairing it).
    let mut per_term: Vec<Vec<u32>> = vec![Vec::new(); term_count as usize];
    let mut pair_count = 0u64;
    for (position, item) in staged.iter().enumerate() {
        let new_id = position as u32;
        // `signature` is already the sorted, deduplicated term-id list computed at staging.
        for &term in &item.signature {
            per_term[term as usize].push(new_id);
            pair_count += 1;
        }
    }

    let partition_dir = args.out.join(PREFIX).join("partitions").join(PHASH);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    let view_dir = tessera_store::view_path(&partition_dir, &view.view_id);
    let segment_dir = view_dir.join("segments").join(SEG_ID);
    for dir in [&terms_dir, &entities_dir, &view_dir, &segment_dir] {
        fs::create_dir_all(dir).map_err(|e| BuildError::io(dir, e))?;
    }

    let dict_paths = dict.finish().map_err(|e| BuildError::io(&dict_dir, e))?;
    let dict_records = term_count;
    for path in &dict_paths {
        fsync_file(path)?;
    }

    let postings_path = terms_dir.join("postings.arrow");
    write_postings(&postings_path, &per_term, SMALL_TERM_THRESHOLD_DEFAULT)
        .map_err(|e| BuildError::io(&postings_path, e))?;
    fsync_file(&postings_path)?;

    let mut other_paths: Vec<PathBuf> = vec![postings_path.clone()];
    if args.emit_oracle_pairs {
        let pairs_path = terms_dir.join("pairs.parquet");
        write_pairs_parquet(&pairs_path, &per_term)?;
        other_paths.push(pairs_path);
    }

    // ---- the entity->term transpose (contracts §2.4) --------------------------------------
    //
    // The postings answer *which entities carry term t*; this answers the other direction, which
    // is what the drill-down's `labels` array intersects with the session's satisfied set
    // (decision 0114) and what the join rule's label arm compares a second view's row against
    // (`views.md` §4). Written from `staged` rather than by transposing `per_term`: `signature`
    // *is* the item's sorted, deduplicated term list, and `position` is its entity id, so the base
    // layer falls out of the same walk in the order the writer requires.
    //
    // **Unconditional, unlike `pairs.parquet`.** That file is an oracle input a deployment may
    // legitimately omit; this one backs a request path and the write path's refusal, so a bundle
    // without it would answer a drill-down short and accept a re-label through a second view.
    let entity_terms_dir = partition_dir.join(tessera_store::ENTITY_TERMS_DIR);
    let mut entity_terms = tessera_store::EntityTermsWriter::create(&entity_terms_dir)
        .map_err(|e| BuildError::Invalid(format!("entity-terms transpose: {e}")))?;
    for (position, item) in staged.iter().enumerate() {
        entity_terms
            .push(position as u32, &item.signature)
            .map_err(|e| BuildError::Invalid(format!("entity-terms transpose: {e}")))?;
    }
    for path in entity_terms
        .finish()
        .map_err(|e| BuildError::Invalid(format!("entity-terms transpose: {e}")))?
    {
        fsync_file(&path)?;
        other_paths.push(path);
    }

    // Minting is opt-in (see `BuildArgs::mint_external_ids`): with it off, no extent and no
    // locator exist, which the reader treats as "no item has an external ID" — the ordinary
    // case, not a degraded one.
    let external_ids_paths = if args.mint_external_ids {
        let (extent_paths, ext_locator_path) = write_external_ids(&entities_dir, &staged, n)?;
        other_paths.push(ext_locator_path);
        extent_paths
    } else {
        Vec::new()
    };

    // ---- 6. the identity, computed BEFORE the tiler (2026-07-30 fold, memo §6) --------
    // `tessera_id` is now a sort key (`priority = high16(tessera_id)`, and the storage order is
    // `(morton, tessera_id)`), so it must exist before `sort_batch` runs, not be written at the
    // row after it.
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let mut tiler_items: Vec<TilerItem> = Vec::with_capacity(n as usize);
    for (position, item) in staged.iter().enumerate() {
        let entity_id = EntityId::new(position as u64);
        let tessera_id = args.identity_key.forward(args.shard_id, entity_id)?;
        tiler_items.push(TilerItem {
            tessera_id,
            qx: item.qx,
            qy: item.qy,
            scalars: Vec::new(),
        });
    }

    // ---- 6b. the declared attribute tail ---------------------------------------------
    // One pass **per attribute source**, joined to the staged items **by source id**, because
    // `scan_attributes` visits rows in file order and staging is in entity order. Skipped
    // entirely when the schema is empty, which is what keeps a schema-less build's `columns.arrow`
    // byte-identical to the one it wrote before this existed.
    //
    // Every staged item must receive a *slot* for every declared column, which is what the absent
    // pre-fill below is for: a row this pass never visits would otherwise keep an empty `scalars`
    // vector, and the segment writer refuses that by name rather than padding it — padding would
    // put every later row's value under the wrong identity in a column whose width says nothing is
    // wrong. What it must **not** require is that every item be *matched*: an entity no source
    // names is a column that is absent for it, which is what a join does and what the coverage
    // report below states in numbers (`configuration.md` §1).
    //
    // `minters` seeds one live `VocabularyMinter` per discovered vocabulary from whatever the
    // schema already pins, and the scan mints into it for every novel key the corpus supplies.
    // Its final state — carried past this block — is what `write_manifests` records into
    // `MANIFEST.vocabularies` below, so a rebuild and the serving path see exactly what this
    // build minted.
    let mut minters = args.schema.open_minters();
    // Hoisted out of the block below so the report can carry it: what the join met is a figure of
    // the build, not of the pass.
    let mut attribute_coverage: Vec<AttributeCoverage> = Vec::new();
    if !args.schema.is_empty() {
        let position_of_source: HashMap<u64, usize> = staged
            .iter()
            .enumerate()
            .map(|(position, item)| (item.source_id, position))
            .collect();
        // **Absent, then overwritten.** Every item starts with one absent value per declared
        // column, so an entity no source names keeps a column of nothing rather than an empty
        // `scalars` vector the segment writer would refuse. That is what makes coverage a report:
        // an unmatched entity is a legitimate outcome of a join and is counted, not stopped.
        for item in tiler_items.iter_mut() {
            item.scalars = vec![ScalarValue::Null; args.schema.attributes.len()];
        }
        attribute_coverage.reserve(args.attribute_sources.len());
        for group in &args.attribute_sources {
            let columns: Vec<&crate::config::Attribute> = group
                .attributes
                .iter()
                .map(|&i| &args.schema.attributes[i])
                .collect();
            let mut matched_rows = 0u64;
            let mut unknown_rows = 0u64;
            let mut present = vec![0u64; group.attributes.len()];
            input::scan_attributes(
                &group.path,
                &group.fields,
                &columns,
                &mut minters,
                args.limit,
                // An attribute source is entity space and has no view to select (`views.md` §5).
                None,
                |batch| {
                    // **Serial, row by row, on purpose.** This is the reference build: the
                    // streaming pipeline splits a batch across its columns for the speed
                    // (`pipeline::read_one_attribute_source`), and a second implementation that
                    // did the same thing the same way would stop being an independent reading of
                    // the same input.
                    for &row in batch.rows {
                        let source_id = batch.ids[row as usize];
                        let Some(&position) = position_of_source.get(&source_id) else {
                            unknown_rows += 1;
                            continue;
                        };
                        matched_rows += 1;
                        for ((&column, decoded), count) in group
                            .attributes
                            .iter()
                            .zip(batch.decoded)
                            .zip(present.iter_mut())
                        {
                            let value = decoded.value(
                                row as usize,
                                &args.schema.attributes[column],
                                &args.schema,
                            )?;
                            if !matches!(value, ScalarValue::Null) {
                                *count += 1;
                            }
                            tiler_items[position].scalars[column] = value;
                        }
                    }
                    Ok(())
                },
            )?;
            attribute_coverage.push(AttributeCoverage {
                source: group.name.clone(),
                entities: staged.len() as u64,
                matched_rows,
                unknown_rows,
                columns: columns
                    .iter()
                    .zip(&present)
                    .map(|(a, &n)| (a.name.clone(), n))
                    .collect(),
            });
        }
        report_attribute_coverage(&attribute_coverage);
    }

    // ---- 6c. attribute filter postings (filter-index §4) -------------------------------
    // Before `sort_batch`, which permutes `tiler_items` into row order: entity id is a staged
    // item's *position*, so the values are entity-major exactly here and nowhere after.
    // Transposed into one vector per column because that is the shape the emit consumes — the
    // streaming pipeline reads its attributes column-major already, and one of the two builds
    // paying a transpose is better than two emit paths that could disagree about a record's
    // contents (which `write_manifests` exists to prevent for the same reason).
    let filter_paths = {
        // The oracle's columns are mapped exactly as the streaming pipeline's are (`column.rs`),
        // so this path holds its own `.build-tmp/` for the length of the emit.
        let tmp = spill::TmpDir::create(&args.out)?;
        let scratch = column::ColumnScratch::new(tmp.path());
        let by_entity: Vec<column::EntityColumn> = args
            .schema
            .attributes
            .iter()
            .enumerate()
            .map(|(index, attribute)| {
                // A `text` column's values are its extents in both builds (`crate::prose`), so
                // its slot here carries the length and nothing else.
                if attribute.ty == tessera_spatial::ScalarType::Text {
                    return column::EntityColumn::prose(&scratch, attribute.ty, tiler_items.len())
                        .map_err(|e| {
                            BuildError::Invalid(format!("attribute '{}': {e}", attribute.name))
                        });
                }
                column::EntityColumn::from_values(
                    &scratch,
                    attribute.ty,
                    tiler_items.iter().map(|i| i.scalars[index].clone()),
                    &attribute.name,
                )
                .map_err(|e| BuildError::Invalid(format!("attribute '{}': {e}", attribute.name)))
            })
            .collect::<Result<_>>()?;
        // One extent per text column, holding every value in entity order — which is the shape
        // the streaming pipeline reaches after several chunks, and the same reader serves both.
        let mut prose_columns: Vec<prose::ProseColumn> = Vec::new();
        for (index, attribute) in args.schema.attributes.iter().enumerate() {
            if attribute.ty != tessera_spatial::ScalarType::Text {
                continue;
            }
            let mut column = prose::ProseColumn::new(tmp.path(), index, &attribute.name);
            let rows: Vec<(u32, &str)> = tiler_items
                .iter()
                .enumerate()
                .filter_map(|(entity, item)| match &item.scalars[index] {
                    tessera_spatial::ScalarValue::Utf8(value) => {
                        Some((entity as u32, value.as_str()))
                    }
                    _ => None,
                })
                .collect();
            column.push_extent(&rows)?;
            prose_columns.push(column);
        }
        let open_prose: Vec<prose::OpenProse> = prose_columns
            .iter()
            .map(prose::ProseColumn::open)
            .collect::<Result<_>>()?;
        // The record blob beside the postings, from the same entity-major values — the two
        // builds must stay byte-identical, so this path writes every artefact the streaming
        // pipeline writes.
        // The text columns' timing is the streaming pipeline's stage split (`observer.rs`); this
        // path is the equivalence oracle and is not observed, so it drops it.
        let (mut paths, _text) = pipeline::write_filter_postings(
            &partition_dir,
            &args.schema,
            &by_entity,
            &open_prose,
            args.memory_budget
                .unwrap_or_else(pipeline::detect_memory_budget),
        )?;
        paths.extend(pipeline::write_record_blob(
            &partition_dir,
            &args.schema,
            &by_entity,
            &open_prose,
        )?);
        drop(open_prose);
        drop(prose_columns);
        drop(by_entity);
        tmp.close()?;
        paths
    };

    // ---- 7. tiler and segment ---------------------------------------------------------
    // Narrow each item's scalars to the render columns, in declaration order, so they align with
    // `scalar_schema_of`'s filtered list. Done after the filter emit above, which needs every
    // declared column including the `index`-only ones.
    //
    // **Unconditional, where it used to be skipped when every column rendered.**
    {
        let render: Vec<bool> = args.schema.attributes.iter().map(|a| a.render).collect();
        for item in &mut tiler_items {
            let mut kept = Vec::with_capacity(render.iter().filter(|k| **k).count());
            for (i, v) in item.scalars.iter().enumerate() {
                if render[i] {
                    kept.push(v.clone());
                }
            }
            item.scalars = kept;
        }
    }
    let scalar_schema = scalar_schema_of(&args.schema);
    let codes = sort_batch(&mut tiler_items, &mut entity_ids);
    // **The resolution this frame actually gave the corpus**, counted here because `codes` is the
    // row order — `(morton, tessera_id)` ascending — and is the same vector `write_segment` puts
    // into `morton.u32` below. The streaming pipeline counts the identical thing at its own
    // segment write; a figure that appeared on one path and not the other would be worse than
    // none, since which path ran is not something the caller chose.
    let occupancy = Occupancy::of_sorted_codes(codes.iter().copied());

    // The render columns' presence, after the sort because the bitmap is over **rows**, and before
    // the substitution below because that is what erases the distinction: `columns.arrow` is
    // non-nullable (contracts R4), so an absent value is written as the type's zero and this is
    // what says that zero means nothing (decision 0064).
    let mut presence_paths: Vec<PathBuf> = Vec::new();
    for (column, (name, _)) in scalar_schema.iter().enumerate() {
        let rows = pipeline::render_presence_of(
            tiler_items
                .iter()
                .map(|i| !matches!(i.scalars[column], ScalarValue::Null)),
        );
        let Some(rows) = rows else { continue };
        if let Some(path) =
            tessera_store::flush::write_render_presence(&segment_dir, name, rows, n as u32)
                .map_err(|e| BuildError::Invalid(format!("attribute '{name}': {e}")))?
        {
            presence_paths.push(path);
        }
    }
    // A `ScalarValue::Null` reaching the segment writer is a typed error rather than a drawn
    // point, so the placeholder goes in last — see `ScalarValue::or_render_placeholder`.
    for item in &mut tiler_items {
        for (value, (_, ty)) in item.scalars.iter_mut().zip(&scalar_schema) {
            if matches!(value, ScalarValue::Null) {
                *value = value.or_render_placeholder(*ty);
            }
        }
    }

    write_segment(&segment_dir, &tiler_items, &codes, &scalar_schema)
        .map_err(|e| BuildError::io(&segment_dir, e))?;
    fsync_file(&segment_dir.join("columns.arrow"))?;
    fsync_file(&segment_dir.join("morton.u32"))?;

    let permutation_path = view_dir.join("permutation.bin");
    let row_order: Vec<EntityId> = entity_ids;
    // `bound` is the partition view's max entity ID + 1. The bootstrap build allocates a dense
    // 0..n, so that is exactly the item count.
    write_permutation(&permutation_path, &row_order, n)
        .map_err(|e| BuildError::io(&permutation_path, e))?;
    fsync_file(&permutation_path)?;

    // The other direction, for the filtered viewport's per-tile route
    // (`tessera_store::row_entity`). `row_order` is already the row→entity vector, so this writes
    // what the permutation was just scattered from rather than deriving anything.
    let row_entity_path = view_dir.join(tessera_store::ROW_ENTITY_FILE);
    let rows_by_index: Vec<u32> = row_order.iter().map(|e| e.raw() as u32).collect();
    tessera_store::write_row_entity(&row_entity_path, &rows_by_index)
        .map_err(|e| BuildError::io(&row_entity_path, e))?;
    fsync_file(&row_entity_path)?;

    // ---- 8. layers, and the artifacts published into them ------------------------------
    // Entity ids are assigned by now — an item's entity is its position in `staged` — so a member
    // named by source id resolves, and the row-less region can be allocated against a settled
    // point mark.
    let mut published_layers = if args.layers.is_empty() {
        crate::layers::PublishedLayers::default()
    } else {
        {
            // The oracle build's `.build-tmp/`, for the member spill's runs — its own, because the
            // filter-postings block above closed the one it opened. `TmpDir::create` deletes a
            // stale directory rather than adopting it, so the two cannot overlap.
            let tmp = spill::TmpDir::create(&args.out)?;
            let mut plan = crate::layers::read(
                &args.layers,
                &args.layer_inputs,
                &args.scoped_layers,
                // The oracle build materialises exactly one view, so its one frame is the whole
                // of decision 0111's per-view slice.
                &[tessera_store::derived::ViewFrame::new(
                    &view.view_id,
                    view.projection,
                    view.extent,
                )],
                tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES,
                tmp.path(),
                args.memory_budget
                    .unwrap_or_else(pipeline::detect_memory_budget),
            )?;
            report_shapes(&plan.shape_reports);
            let by_source: HashMap<u64, u64> = staged
                .iter()
                .enumerate()
                .map(|(position, item)| (item.source_id, position as u64))
                .collect();
            let published = crate::layers::publish(
                &mut plan,
                &|source| by_source.get(&source).copied(),
                n,
                &args.out.join(PREFIX),
                PHASH,
                std::slice::from_ref(&view.view_id),
                // The linear build holds its values on the items rather than in typed entity
                // columns, which is the only thing about the two builds this rule sees.
                &crate::layers::predicate_artifact_keys(
                    &args.layers,
                    &args.schema,
                    &minters,
                    &|index| {
                        crate::pipeline::distinct_codes(
                            tiler_items.iter().map(|item| item.scalars[index].clone()),
                        )
                    },
                )?,
            )?;
            // The runs and the merged table are dead the moment the publication has read them,
            // and this is the success path — so the removal is reported rather than left to
            // `Drop`, which cannot say a file was still busy.
            drop(plan);
            tmp.close()?;
            published
        }
    };

    write_containment_report(&args.out, &published_layers)?;

    // ---- 8b. the post-bundle artifact pass ---------------------------------------------
    //
    // **Both builds run it, and that is not optional**: `tests/build_equivalence.rs` asserts the
    // two produce byte-identical bundles, and the pass writes files and edits the layer records the
    // manifest carries. The linear build writes its permutation at step 7, so row space already
    // exists here — the batched build has to wait for its tiler sort, which is the only reason the
    // two call sites sit at different step numbers. See `crate::artifact_pass`.
    let artifact_store = std::mem::take(&mut published_layers.store);
    let artifact_pass = crate::artifact_pass::run(
        &mut published_layers,
        &artifact_store,
        &args.out.join(PREFIX),
        PHASH,
        &view.view_id,
        n as u32,
        &plugin.data_plugin_hash(),
    );
    drop(artifact_store);
    crate::artifact_pass::report(&artifact_pass);
    report_hierarchies(&published_layers.hierarchy_shapes);
    published_layers
        .tile_index_extents
        .clone_from(&artifact_pass.tile_index_extents);
    published_layers
        .row_column_extents
        .clone_from(&artifact_pass.row_column_extents);
    published_layers
        .containment_extents
        .clone_from(&artifact_pass.containment_extents);
    published_layers
        .shape_rows_extents
        .clone_from(&artifact_pass.shape_rows_extents);
    published_layers
        .shape_held_extents
        .clone_from(&artifact_pass.shape_held_extents);

    // ---- 9. manifests ------------------------------------------------------------------
    other_paths.extend([
        permutation_path,
        row_entity_path,
        segment_dir.join("columns.arrow"),
        segment_dir.join("morton.u32"),
    ]);
    other_paths.extend(presence_paths);
    other_paths.extend(filter_paths);
    other_paths.extend(published_layers.paths.iter().cloned());
    other_paths.extend(artifact_pass.paths.iter().cloned());
    let mut report = write_manifests(
        args,
        &BundleFiles {
            dict_paths,
            dict_records,
            external_ids_paths,
            other_paths,
        },
        &plugin,
        n,
        term_count,
        pair_count,
        args.batch_items.filter(|&b| b < n),
        &minters,
        &published_layers,
        &[SegmentDescriptor {
            view: view.view_id.clone(),
            incarnation: tessera_store::manifest::DECLARED_INCARNATION,
            seg_id: SEG_ID.to_string(),
            row_count: n as u32,
            entity_lo: 0,
            entity_hi: n,
        }],
        std::slice::from_ref(&occupancy),
    )?;
    report.attribute_coverage = attribute_coverage;
    report.hierarchy_shapes = published_layers.hierarchy_shapes.clone();
    Ok(report)
}

/// The schema as the segment writer wants it: `(name, type)` in declared order.
///
/// One derivation, shared by both build implementations, so the two cannot come to disagree about
/// a column's width — which would produce two bundles the byte-equality oracle calls different
/// for a reason that is not the entity assignment it exists to check.
fn scalar_schema_of(
    schema: &crate::config::Schema,
) -> Vec<(String, tessera_spatial::tiler::ScalarType)> {
    // Render columns only — the segment's tail and `permute_attribute_tail`'s output must name the
    // same columns in the same order, or every row's values land under the wrong headings.
    schema
        .attributes
        .iter()
        .filter(|a| a.render)
        .map(|a| (a.name.clone(), a.ty))
        .collect()
}

/// Every file a build wrote, split by the role it plays in the manifests.
struct BundleFiles {
    dict_paths: Vec<PathBuf>,
    dict_records: u64,
    external_ids_paths: Vec<PathBuf>,
    other_paths: Vec<PathBuf>,
}

/// Write `SEGMENTS-0.json`, `MANIFEST.json` and `CURRENT` over the files a build produced.
/// Shared by both build implementations so the two cannot drift in the one place where a
/// difference would be invisible until a digest failed.
#[allow(clippy::too_many_arguments)]
fn write_manifests(
    args: &BuildArgs,
    files: &BundleFiles,
    plugin: &Passthrough,
    n: u64,
    term_count: u64,
    pair_count: u64,
    batch_items_recorded: Option<u64>,
    minters: &HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    published_layers: &crate::layers::PublishedLayers,
    segments_written: &[SegmentDescriptor],
    occupancies: &[Occupancy],
) -> Result<BuildReport> {
    let bounds = plugin.declared_bounds();
    let partition_dir = args.out.join(PREFIX).join("partitions").join(PHASH);
    // Contracts §2.2 / §2.3 divide the two `files` maps by *when* a file appeared:
    // `MANIFEST.files` covers every file present at build time, and `SEGMENTS-<n>.files` covers
    // only what has been added *since* that manifest was written (streamed segments, later
    // deltas). A batch build produces everything at build time, so every file it writes belongs
    // in `MANIFEST.files` and `SEGMENTS-0.json`'s map is legitimately empty. (`open_bundle`
    // accepts a file verified via either map, so both splits load — but the spec's wording is
    // what the Python oracle and the conformance byte-scanner will be written against.)
    let prefix_dir = args.out.join(PREFIX);
    let mut dict_extents = Vec::new();
    for path in &files.dict_paths {
        dict_extents.push(DictExtent {
            path: relative_to(&prefix_dir, path)?,
            records: files.dict_records,
        });
    }
    let mut external_id_runs = Vec::with_capacity(files.external_ids_paths.len());
    for path in &files.external_ids_paths {
        external_id_runs.push(relative_to(&prefix_dir, path)?);
    }
    // Digest in parallel, one worker per file: SHA-256 is inherently sequential per file, but
    // the files are independent, and at 10⁹ this stage re-reads ~47 GB. The map is assembled
    // from (name, digest) pairs afterwards, so the manifest bytes cannot depend on scheduling.
    let all_paths: Vec<&PathBuf> = files
        .dict_paths
        .iter()
        .chain(&files.external_ids_paths)
        .chain(&files.other_paths)
        .collect();
    let manifest_files: BTreeMap<String, FileDigest> = all_paths
        .into_par_iter()
        .map(|path| Ok((relative_to(&prefix_dir, path)?, digest_file(path)?)))
        .collect::<Result<_>>()?;

    let bundle_bytes: u64 = manifest_files.values().map(|f| f.size).sum();

    let segments = SegmentsManifest {
        watermark: n,
        entity_id_high_water: n,
        // The row-less region's mark, and it must be the manifest's: the WAL carries the same one
        // in its registration records and rotation reclaims those, so a mark that lived only there
        // is lost at the first rotation and the next registration is handed ids a live layer
        // already holds (decision 0074). `ROWLESS_CEILING` when the declaration carried no layer, which is
        // the untouched region rather than a default standing in for a lost value.
        entity_id_low_water: published_layers.low_water,
        layers: published_layers.layers.clone(),
        // Nothing a build writes has ever been dropped: a tombstone is a control-plane act against
        // a running node, and a build produces a bundle rather than editing one.
        layer_tombstones: Vec::new(),
        views: Vec::new(),
        scoped_columns: Vec::new(),
        dead_view_incarnations: Vec::new(),
        membership_extents: published_layers.membership_extents.clone(),
        level_versions: published_layers.level_versions.clone(),
        // **The post-bundle artifact pass's output** (`crate::artifact_pass`). Empty only where
        // the build published no artifacts, or where a derived structure would not compose — each
        // of which leaves the level composing it on first use, exactly as before the pass existed.
        containment_extents: published_layers.containment_extents.clone(),
        tile_index_extents: published_layers.tile_index_extents.clone(),
        row_column_extents: published_layers.row_column_extents.clone(),
        shape_rows_extents: published_layers.shape_rows_extents.clone(),
        shape_held_extents: published_layers.shape_held_extents.clone(),
        artifact_record_extents: published_layers.artifact_record_extents.clone(),
        // One per view the build materialised (`views.md` §7), in registry order. `entity_hi`
        // is inclusive, and an empty build has no entity range at all — hence the saturating
        // subtraction pass two hands over.
        segments: segments_written
            .iter()
            .map(|descriptor| SegmentDescriptor {
                entity_hi: descriptor.entity_hi.saturating_sub(1),
                ..descriptor.clone()
            })
            .collect(),
        deltas: Vec::new(),
        dict_extents,
        attr_extents: Vec::new(),
        record_extents: Vec::new(),
        // The base transpose covers every entity the build knows about, exactly as the base
        // record blob does; a flush's slices are the extents.
        entity_terms_extents: Vec::new(),
        text_extents: Vec::new(),
        external_id_runs,
        locator_extents: Vec::new(),
        tombstones: Vec::new(),
        deny: Vec::new(),
        // Nothing has been added since MANIFEST.json — see the note above.
        vocabulary_extensions: Vec::new(),
        files: BTreeMap::new(),
    };
    let segments_path = partition_dir.join("SEGMENTS-0.json");
    write_json(&segments_path, &segments)?;

    let manifest = Manifest {
        bundle_format: BUNDLE_FORMAT,
        created_at: chrono::Utc::now().to_rfc3339(),
        data_plugin_hash: plugin.data_plugin_hash(),
        declared_bounds: serde_json::json!({
            "max_distinct_terms": bounds.max_distinct_terms,
            "max_terms_per_item": bounds.max_terms_per_item,
            "max_terms_per_token": bounds.max_terms_per_token,
        }),
        // The schema, compiled. `MANIFEST.declared_scalars` is the *only* thing downstream reads:
        // `columns.arrow`'s tail is written in this order, `/control/ingest` builds each row's
        // scalar vector in this order, and flush, merge and the fold all take their writer schema
        // from it. Reordering the schema file therefore reorders every segment built after it,
        // which is why the compilation preserves declaration order rather than sorting by name.
        declared_scalars: args
            .schema
            .attributes
            .iter()
            .map(|a| DeclaredScalar {
                name: a.name.clone(),
                arrow_type: a.ty,
                vocabulary: a.vocabulary.clone(),
                // Resolved at the schema parse, so what a bundle records is the identity the build
                // actually indexed with rather than the name a schema asked for.
                analyser: a.analyser.clone(),
                index: a.index,
                render: a.render,
            })
            .collect(),
        // Sorted by name, unlike the columns: nothing indexes a vocabulary positionally, and a
        // `HashMap`'s iteration order would otherwise put non-determinism into the manifest bytes
        // — which are under a digest.
        //
        // A **closed** vocabulary's values are exactly what the config declared (`v.codes`,
        // unchanged — pinned or assigned alike). An **open** one's values come from
        // `minters[&v.name]` instead — the declaration's codes *plus* every code this build
        // minted for a key the declaration lacked —
        // because `v.codes` alone would silently omit everything minted during the scan. Either
        // way the values are read back sorted by key ([`tessera_store::vocabulary::values_of`]),
        // so the bytes here do not depend on a `BTreeMap`'s or a minter's internal order.
        vocabularies: {
            let mut compiled: Vec<ManifestVocabulary> = args
                .schema
                .vocabularies
                .values()
                .map(|v| {
                    let values = match minters.get(&v.name) {
                        Some(minter) => tessera_store::vocabulary::values_of(minter)
                            .into_iter()
                            .map(|value| ManifestVocabularyValue {
                                title: v.titles.get(&value.key).cloned(),
                                ..value
                            })
                            .collect(),
                        None => v
                            .codes
                            .iter()
                            .map(|(key, &code)| ManifestVocabularyValue {
                                key: key.clone(),
                                code,
                                title: v.titles.get(key).cloned(),
                            })
                            .collect(),
                    };
                    ManifestVocabulary {
                        name: v.name.clone(),
                        // The declaration's value set, carried verbatim: it is what ingest
                        // consults to decide whether a key nothing has bound is a typo or a new
                        // value. The manifest keeps its own two words for it.
                        kind: match v.value_set {
                            crate::config::ValueSet::Closed => {
                                tessera_store::manifest::VocabularyKind::Declared
                            }
                            crate::config::ValueSet::Open => {
                                tessera_store::manifest::VocabularyKind::Discovered
                            }
                        },
                        visibility: v.visibility,
                        values,
                        reserved: v.reserved.clone(),
                    }
                })
                .collect();
            compiled.sort_by(|a, b| a.name.cmp(&b.name));
            compiled
        },
        small_term_threshold: SMALL_TERM_THRESHOLD_DEFAULT,
        entity_id_high_water: n,
        identity: IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: args.identity_key_hex.clone(),
            shard_id: args.shard_id,
            idset: args.idset,
        },
        // **One entry per view, each carrying its own frame** (decision 0040): two views of one
        // bundle may quantise differently, and an embedding and a map cannot share a frame
        // without one of them wasting most of the grid (`views.md` §2).
        // **The roster, and the column families scoped to it** (`views.md` §5). The families are
        // derived here from [`BuildArgs::scoped_attributes`] rather than carried on the argument's
        // own group descriptors: the declaration says which attributes are scoped and to what, and
        // a second copy on the input would be a second thing to disagree with it.
        groups: args
            .groups
            .iter()
            .map(|group| tessera_store::manifest::GroupDescriptor {
                scoped_scalars: args
                    .scoped_attributes
                    .iter()
                    .filter(|family| family.group == group.name)
                    .map(|family| tessera_store::manifest::ScopedScalar {
                        name: family.attribute.name.clone(),
                        group: family.group.clone(),
                        arrow_type: family.attribute.ty,
                        vocabulary: family.attribute.vocabulary.clone(),
                        analyser: family.attribute.analyser.clone(),
                        index: family.attribute.index,
                        render: family.attribute.render,
                        // The views whose columns this build **wrote**, in the order the family
                        // names them, which is the roster's own order.
                        views: family
                            .views
                            .iter()
                            .map(|&index| args.views[index].view_id.clone())
                            .collect(),
                    })
                    .collect(),
                ..group.clone()
            })
            .collect(),
        views: args
            .views
            .iter()
            .map(|view| ViewDescriptor {
                id: view.view_id.clone(),
                display_name: view.view_id.clone(),
                // **The declared incarnation** (decision 0115). A key a build declared and a
                // running service later drops comes back at 1 or above, which is what keeps the
                // build's own segments out of the view created under the reused name.
                incarnation: tessera_store::manifest::DECLARED_INCARNATION,
                quantisation: Quantisation {
                    x_min: view.extent.x_min,
                    x_max: view.extent.x_max,
                    y_min: view.extent.y_min,
                    y_max: view.extent.y_max,
                },
                // What placed these positions before the frame did. A bundle that carries
                // projected positions and cannot say so is one the write path and the
                // differential oracle both have to be told about out of band
                // (`projections.md` §3).
                projection: view.projection,
                // **The view's own gate** (`views.md` §6), the roster's copy of which is on the
                // group descriptor above; `Manifest::validate_groups` refuses a bundle whose two
                // copies disagree.
                visibility: view.visibility.clone(),
            })
            .collect(),
        partitions: vec![PartitionDescriptor {
            phash: PHASH.to_string(),
            required_terms: Vec::new(),
        }],
        provenance: match batch_items_recorded {
            // The batch size is identity-bearing (I9): a rebuild must replay it. Omitted
            // entirely for a single-batch build, so pre-batching manifests stay well-defined
            // (absent key == one batch).
            Some(batch_items) => serde_json::json!({
                "generating_set_choice": "prompt-sample",
                "batch_items": batch_items,
            }),
            None => serde_json::json!({ "generating_set_choice": "prompt-sample" }),
        },
        files: manifest_files,
    };
    // Read back from the files just written, before the manifest is, so a keyword column whose
    // dictionary does not open refuses here rather than after `CURRENT` has moved. The same pass
    // runs at `tessera verify`, so the two report one set of figures.
    let keyword_cardinalities =
        unique_key::keyword_cardinalities(&prefix_dir, &manifest, &[(PHASH, &segments)])?;
    unique_key::report_keyword_cardinalities(&keyword_cardinalities, bundle_bytes);
    // Both bundle artefacts, not written here: MANIFEST.json and CURRENT are pass 5's writers
    // too (compaction §10's rule paragraph — a bundle artefact's writer lives in
    // `tessera-store`), so this build and a fold cannot serialise the same shape two different
    // ways and disagree about what a manifest digests to.
    let manifest_digest = write_manifest_json(&prefix_dir, &manifest)?;

    // Everything the bundle names is now durable; `CURRENT` is written last and by rename, so
    // a reader either sees the previous bundle or this complete one, never a half-built prefix.
    write_current(&args.out, PREFIX, &manifest_digest)?;

    Ok(BuildReport {
        prefix: PREFIX.to_string(),
        seg_id: SEG_ID.to_string(),
        views: segments_written
            .iter()
            .zip(occupancies)
            .map(|(descriptor, occupancy)| ViewReport {
                view_id: descriptor.view.clone(),
                rows: descriptor.row_count as u64,
                occupancy: *occupancy,
            })
            .collect(),
        items: n,
        terms: term_count,
        pairs: pair_count,
        bundle_bytes,
        unclustered_member_rows: published_layers.unclustered.iter().map(|u| u.rows).sum(),
        minted_artifacts: published_layers.minted.values().sum(),
        // Filled by the caller: the join happened stages ago and this function digests files.
        artifact_levels: Vec::new(),
        hierarchy_shapes: Vec::new(),
        attribute_coverage: Vec::new(),
        keyword_cardinalities,
    })
}

/// What `tessera verify` checked.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub prefix: String,
    pub partitions: usize,
    pub views: usize,
    pub segments: usize,
    pub rows: u64,
    pub entity_id_high_water: u64,
    /// Every file the manifests name, summed: the denominator an index's cost is stated against.
    pub bundle_bytes: u64,
    /// Each indexed keyword column's figures, read from the bundle exactly as the build reported
    /// them (`unique_key`).
    pub keyword_cardinalities: Vec<KeywordCardinality>,
}

/// Verify a bundle at `root`: run the read protocol (which checks every manifest digest, every
/// file's size and SHA-256, and each permutation's bijectivity onto its segment's rows), then
/// re-confirm the row space covers exactly the rows the segments claim, and re-derive every
/// row's `tessera_id` from `(identity.key, identity.shard_id, entity_id)`, failing if a single
/// row disagrees (contracts §2.6 r6: "`tessera verify` checks the whole column against" the
/// key).
pub fn verify(root: &Path) -> Result<VerifyReport> {
    verified_open(root).map(|(_, report)| report)
}

/// The pass behind [`verify`] and [`deep::verify_deep`], returning the opened bundle so the deep
/// mode does not pay a second full open (the open re-hashes every named file).
fn verified_open(root: &Path) -> Result<(tessera_store::read::Bundle, VerifyReport)> {
    let bundle = tessera_store::read::open_bundle(root)?;
    // The key is parsed here, not by `open_bundle`: `IdentityDescriptor::validate` (run at
    // open) checks `construction`/`rounds`/`idset` but never parses `key`'s hex, since
    // `tessera-store` has no need to hold a live `IdentityKey` at all — only `tessera verify`
    // and the build do.
    let identity_key = IdentityKey::from_hex(&bundle.manifest.identity.key)
        .map_err(|e| BuildError::Invalid(format!("MANIFEST identity.key: {e}")))?;
    let shard_id = bundle.manifest.identity.shard_id;

    let mut views = 0usize;
    let mut segments = 0usize;
    let mut rows = 0u64;
    for partition in bundle.partitions.values() {
        for (view_id, view) in &partition.views {
            views += 1;
            segments += view.segments.len();
            let total_rows = view.row_space.total_rows();
            rows += total_rows;
            // `open_bundle` already ran `validate_rows` on the base and `is_well_formed` on
            // every extent (no aliasing, no out-of-range row). The remaining half of
            // bijectivity is surjectivity: every row of every segment must be claimed by some
            // entity, or `columns.arrow` holds a row no entity can ever address. Swept over the
            // whole row space — base *and* extents — because a bundle that has flushed holds
            // rows above the base permutation, and a sweep of the base alone refuses every such
            // bundle as "not a bijection" (the false refusal §18 obligation 10 names). Built as
            // a row-indexed array (rather than just a count) so the identity check below can
            // reuse it instead of inverting the row space a second time.
            view.row_space
                .base()
                .validate_rows(view.row_space.base_rows())?;
            let entity_bound = view
                .row_space
                .extents()
                .last()
                .map(|extent| extent.entity_hi + 1)
                .unwrap_or_else(|| view.row_space.base().bound());
            let total_rows_usize = usize::try_from(total_rows).map_err(|_| {
                BuildError::Invalid(format!(
                    "view '{view_id}': {total_rows} rows does not fit usize"
                ))
            })?;
            let mut entity_of_row: Vec<Option<u64>> = vec![None; total_rows_usize];
            let mut claimed = 0u64;
            for entity in 0..entity_bound {
                if let Some(row) = view.row_space.row_of(tessera_types::EntityId::new(entity)) {
                    entity_of_row[row.raw() as usize] = Some(entity);
                    claimed += 1;
                }
            }
            if claimed != total_rows {
                return Err(BuildError::Invalid(format!(
                    "view '{view_id}': the row space claims {claimed} rows but the segments \
                     hold {total_rows} — not a bijection"
                )));
            }

            // The identity column, per segment **at that segment's own row offset**. A segment's
            // `columns.arrow` rows are local `0..row_count`; in the view's row space they begin
            // at the extent's `row_base` (the base segment's at 0). The offset is looked up from
            // the row space rather than accumulated in iteration order, so this cannot silently
            // depend on the segment list's ordering.
            for segment in &view.segments {
                let row_base = view
                    .row_space
                    .extents()
                    .iter()
                    .find(|extent| extent.seg_id == segment.seg_id)
                    .map(|extent| extent.row_base as usize)
                    .unwrap_or(0);
                let ids = segment.columns.tessera_id();
                for (local, id) in ids.iter().enumerate() {
                    let entity = entity_of_row
                        .get(row_base + local)
                        .copied()
                        .flatten()
                        .ok_or_else(|| {
                            BuildError::Invalid(format!(
                                "view '{view_id}' segment '{}' row {local}: no entity claims \
                                 this row",
                                segment.seg_id
                            ))
                        })?;
                    let expected = identity_key
                        .forward(shard_id, tessera_types::EntityId::new(entity))
                        .map_err(BuildError::Identity)?
                        .raw();
                    if *id != expected {
                        return Err(BuildError::Invalid(format!(
                            "view '{view_id}' segment '{}' row {local}: tessera_id {id:#x} \
                             does not match identity.key's derivation {expected:#x} for entity \
                             {entity}",
                            segment.seg_id
                        )));
                    }
                }
            }
        }
    }
    let current: CurrentPointer = {
        let bytes = fs::read(root.join("CURRENT")).map_err(|e| BuildError::io(root, e))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| BuildError::Invalid(format!("CURRENT is not valid JSON: {e}")))?
    };
    // Sorted so two runs over one bundle report the columns in one order.
    let mut partitions: Vec<(&str, &tessera_store::manifest::SegmentsManifest)> = bundle
        .partitions
        .iter()
        .map(|(phash, data)| (phash.as_str(), &data.manifest))
        .collect();
    partitions.sort_by(|a, b| a.0.cmp(b.0));
    let bundle_bytes = bundle
        .manifest
        .files
        .values()
        .chain(
            partitions
                .iter()
                .flat_map(|(_, segments)| segments.files.values()),
        )
        .map(|file| file.size)
        .sum();
    let keyword_cardinalities = unique_key::keyword_cardinalities(
        &root.join(&current.prefix),
        &bundle.manifest,
        &partitions,
    )?;
    let report = VerifyReport {
        prefix: current.prefix,
        partitions: bundle.partitions.len(),
        views,
        segments,
        rows,
        entity_id_high_water: bundle.manifest.entity_id_high_water,
        bundle_bytes,
        keyword_cardinalities,
    };
    Ok((bundle, report))
}

fn write_pairs_parquet(path: &Path, per_term: &[Vec<u32>]) -> Result<()> {
    let mut writer = PairsParquetWriter::create(path)?;
    for (t, entity_ids) in per_term.iter().enumerate() {
        writer.push_run(t as u32, entity_ids)?;
    }
    writer.finish()?;
    Ok(())
}

/// Write `external-ids-0.arrow` (R4; r6 narrows `entity_id` to `uint32`) and
/// `entities/ext-locator.u32` (r6, contracts §2.4/§2.6): the external ID here is the source
/// corpus's entity ID as 8 bytes little-endian; byte order is not numeric order, so the sort is
/// over the encoded keys. Returns the extent paths (in listed order) and the locator's path.
fn write_external_ids(
    dir: &Path,
    staged: &[StagedItem],
    entity_id_high_water: u64,
) -> Result<(Vec<PathBuf>, PathBuf)> {
    let mut rows: Vec<ExternalIdRow> = staged
        .iter()
        .enumerate()
        .map(|(position, item)| ExternalIdRow::new(item.source_id, position as u32))
        .collect();
    rows.sort_unstable_by_key(ExternalIdRow::sort_key);
    let extent_paths = write_external_id_runs(dir, &rows, EXTERNAL_ID_ROWS_PER_EXTENT)?;
    let locator_path = write_ext_locator(dir, &rows, entity_id_high_water)?;
    Ok((extent_paths, locator_path))
}

/// Write `entities/ext-locator.u32` (contracts §2.4/§2.6 r6): one raw `u32` array, no header, no
/// `<k>` suffix, length `entity_id_high_water`, `locator[entity_id] = ordinal` — that entity's
/// position in the concatenated sorted external-id extents, in listed (extent) order.
/// `0xFFFFFFFF` marks an entity with no caller external ID; in this bootstrap build every item is
/// given the source corpus's own id as its external id, so the sentinel is unused here but the
/// array is still initialised to it, since a later, incremental build can append entities this
/// build's extents never cover.
///
/// `rows` must already be in the same ascending order the extents were written in — the
/// concatenation's ordinal for `rows[i]` is exactly `i`, so a second sort or a re-read of the
/// extents is not needed to compute it.
fn write_ext_locator(
    dir: &Path,
    rows: &[ExternalIdRow],
    entity_id_high_water: u64,
) -> Result<PathBuf> {
    let path = dir.join("ext-locator.u32");
    let bound = usize::try_from(entity_id_high_water).map_err(|_| {
        BuildError::Invalid(format!(
            "entity_id_high_water {entity_id_high_water} does not fit usize"
        ))
    })?;
    let mut locator = vec![0xFFFF_FFFFu32; bound];
    for (ordinal, row) in rows.iter().enumerate() {
        let entity = row.entity_id as usize;
        // `entity` is always `< bound` here: every row's entity id came from `0..n` at staging,
        // and `entity_id_high_water` is `n`. Checked anyway — an out-of-range write here would
        // silently corrupt an unrelated entity's locator slot, and that is a disclosure.
        if entity >= locator.len() {
            return Err(BuildError::Invalid(format!(
                "ext-locator: entity id {entity} is out of bound (bound = {})",
                locator.len()
            )));
        }
        locator[entity] = ordinal as u32;
    }
    // Buffered: an unbuffered 4-bytes-per-write loop is one syscall per entity — measured as
    // the majority of the whole external-ids stage at 10⁸ (the bytes written are identical).
    let file = File::create(&path).map_err(|e| BuildError::io(&path, e))?;
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
    for slot in &locator {
        writer
            .write_all(&slot.to_le_bytes())
            .map_err(|e| BuildError::io(&path, e))?;
    }
    let file = writer
        .into_inner()
        .map_err(|e| BuildError::io(&path, e.into_error()))?;
    file.sync_all().map_err(|e| BuildError::io(&path, e))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(path)
}

/// One `(external_id, entity_id)` row awaiting the byte sort.
///
/// Twelve bytes, four-byte aligned. Deliberately **not** `(u64, u32)`: that tuple is padded to
/// sixteen, which at 10⁹ items is four gigabytes of nothing at the build's second-tightest
/// moment. The external id is held as its sort key — the source id byte-swapped, so that numeric
/// order over `(key_hi, key_lo)` is byte order over the little-endian encoding that goes on disk
/// (R4 sorts by the external id's *bytes*, and byte order is not numeric order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct ExternalIdRow {
    key_hi: u32,
    key_lo: u32,
    entity_id: u32,
}

impl ExternalIdRow {
    pub(crate) fn new(source_id: u64, entity_id: u32) -> Self {
        let key = source_id.swap_bytes();
        ExternalIdRow {
            key_hi: (key >> 32) as u32,
            key_lo: key as u32,
            entity_id,
        }
    }

    pub(crate) fn sort_key(&self) -> (u32, u32) {
        (self.key_hi, self.key_lo)
    }

    fn source_id(&self) -> u64 {
        (((self.key_hi as u64) << 32) | self.key_lo as u64).swap_bytes()
    }
}

/// The largest number of rows one `external-ids-<n>.arrow` extent may carry.
///
/// Arrow's `Binary` layout addresses its values buffer with **`i32`** offsets, so an extent of
/// 8-byte external ids saturates at `i32::MAX / 8` rows — a 10⁹-item bundle cannot be written as
/// one extent at all. Splitting well below that ceiling and listing every extent in
/// `external_id_runs` (contracts §2.1 has always made that field a list, and the engine's
/// index already loads and re-sorts across extents) is what makes the largest corpus
/// expressible; at every scale below the split point exactly one extent is written, identical to
/// what earlier builds wrote.
pub(crate) const EXTERNAL_ID_ROWS_PER_EXTENT: usize = 100_000_000;

/// Write `rows` — already in ascending external-id **byte** order — as one or more extents in
/// `dir`, at most `rows_per_extent` rows each, returning their paths in order. The extents
/// partition the global order into consecutive ranges, so each is individually sorted too.
///
/// `rows_per_extent` is a parameter rather than a direct use of
/// [`EXTERNAL_ID_ROWS_PER_EXTENT`] so the splitting boundary is testable without writing a
/// hundred million rows.
fn write_external_id_runs(
    dir: &Path,
    rows: &[ExternalIdRow],
    rows_per_extent: usize,
) -> Result<Vec<PathBuf>> {
    assert!(rows_per_extent > 0, "rows_per_extent must be positive");
    let mut paths = Vec::new();
    for chunk in rows.chunks(rows_per_extent) {
        let path = dir.join(format!("external-ids-{}.arrow", paths.len()));
        write_external_id_run(&path, chunk)?;
        paths.push(path);
    }
    // `chunks` yields nothing for an empty input, but a bundle always names at least one extent.
    if paths.is_empty() {
        let path = dir.join("external-ids-0.arrow");
        write_external_id_run(&path, &[])?;
        paths.push(path);
    }
    Ok(paths)
}

fn write_external_id_run(path: &Path, rows: &[ExternalIdRow]) -> Result<()> {
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("entity_id", DataType::UInt32, false), // r6, D8: was UInt64
    ]));
    // Built straight from `rows`: an intermediate `Vec` of keys or of widened rows would be a
    // gigabyte-scale copy of data that is already laid out correctly.
    let external: ArrayRef = std::sync::Arc::new(BinaryArray::from_iter_values(
        rows.iter().map(|row| row.source_id().to_le_bytes()),
    ));
    let entity: ArrayRef = std::sync::Arc::new(UInt32Array::from_iter_values(
        rows.iter().map(|row| row.entity_id),
    ));
    let batch = RecordBatch::try_new(schema.clone(), vec![external, entity])
        .map_err(|e| BuildError::arrow(path, e))?;

    let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
    let mut writer =
        ArrowFileWriter::try_new(file, &schema).map_err(|e| BuildError::arrow(path, e))?;
    writer
        .write(&batch)
        .map_err(|e| BuildError::arrow(path, e))?;
    writer.finish().map_err(|e| BuildError::arrow(path, e))?;
    fsync_file(path)
}

/// Write the hierarchy containment report into the bundle root's `reports/`.
///
/// **Beside the prefix, never inside it**, on the fold report's rule: a prefix is reclaimed and a
/// notice nobody has read yet would go with it. **Written even when empty**, for the same reason
/// that report is — an operator polling the directory must be able to tell *this build found
/// nothing* from *this build never looked*, and an absent file says the second.
///
/// It decides nothing. A violating edge is published exactly as a clean one is; what the report
/// buys is that the edge is named before a viewer meets its consequences.
///
/// **A build that registered no layers writes nothing at all** — not even the directory. There are
/// no edges to have checked, so an empty report there would answer a question nobody asked, and
/// creating `reports/` for it would mean every bundle carries the fold's notice directory before a
/// fold has ever run.
pub(crate) fn write_containment_report(
    root: &Path,
    published: &crate::layers::PublishedLayers,
) -> Result<()> {
    if published.layers.is_empty() {
        return Ok(());
    }
    let violations = &published.containment_violations;
    let dir = root.join("reports");
    std::fs::create_dir_all(&dir).map_err(|e| BuildError::io(&dir, e))?;
    let rows: Vec<serde_json::Value> = violations
        .iter()
        .map(|v| {
            serde_json::json!({
                "layer": v.layer,
                "level": v.level,
                "child": v.child,
                "parent": v.parent,
                "escaping_members": v.escaping_members,
            })
        })
        .collect();

    // **The splits that lose the most, and how many were not listed.** A tree has one of these per
    // internal node, which at 10⁷ artifacts is a report nobody opens; naming the worst is what an
    // operator actually reads, and saying how many were dropped is what keeps the list from
    // reading as "these are all of them" (`docs/agents/` — no silent caps).
    const LISTED: usize = 100;
    let mut splits: Vec<_> = published.split_coverage.iter().collect();
    splits.sort_by(|a, b| {
        (b.stray_members, b.members, &b.parent).cmp(&(a.stray_members, a.members, &a.parent))
    });
    let non_covering = splits.iter().filter(|s| s.stray_members > 0).count();
    let listed: Vec<serde_json::Value> = splits
        .iter()
        .take(LISTED)
        .map(|s| {
            serde_json::json!({
                "layer": s.layer,
                "level": s.level,
                "parent": s.parent,
                "children": s.children,
                "members": s.members,
                "stray_members": s.stray_members,
            })
        })
        .collect();

    let hierarchies: Vec<serde_json::Value> = published
        .hierarchy_shapes
        .iter()
        .map(|s| {
            serde_json::json!({
                "layer": s.layer,
                "level": s.level,
                "kind": s.kind,
                "artifacts": s.artifacts,
                "edges": s.edges,
                "roots": s.roots,
                "multi_parent": s.multi_parent,
                "max_parents": s.max_parents,
            })
        })
        .collect();

    write_json(
        &dir.join("containment.json"),
        &serde_json::json!({
            "hierarchies": hierarchies,
            "violations": rows,
            "splits": {
                "total": splits.len(),
                "non_covering": non_covering,
                "listed": listed.len(),
                "not_listed": splits.len().saturating_sub(listed.len()),
                "by_stray_members": listed,
            },
        }),
    )
}

pub(crate) fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| BuildError::Invalid(format!("serialising {}: {e}", path.display())))?;
    write_bytes(path, &bytes)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path).map_err(|e| BuildError::io(path, e))?;
    file.write_all(bytes).map_err(|e| BuildError::io(path, e))?;
    file.sync_all().map_err(|e| BuildError::io(path, e))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_file(path: &Path) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    file.sync_all().map_err(|e| BuildError::io(path, e))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(|e| BuildError::io(path, e))?;
    dir.sync_all().map_err(|e| BuildError::io(path, e))
}

/// How much of a file is held in memory at once while hashing it. One mebibyte is large enough
/// that the syscall cost is noise against the hashing and small enough to be irrelevant to the
/// build's peak.
const DIGEST_CHUNK_BYTES: usize = 1 << 20;

/// SHA-256 and size of `path`, read in fixed-size chunks. Never `fs::read` here: at 10⁹ items
/// `columns.arrow` alone is over 20 GB, and slurping it to hash it would reintroduce the very
/// ceiling this build was rewritten to remove.
fn digest_file(path: &Path) -> Result<FileDigest> {
    use std::io::Read;
    let mut file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; DIGEST_CHUNK_BYTES];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| BuildError::io(path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(FileDigest {
        size,
        sha256: hex_digest(hasher.finalize().as_slice()),
    })
}

pub(crate) fn hex_digest(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Manifest keys are prefix-relative with forward slashes (R1), on every platform.
fn relative_to(prefix_dir: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(prefix_dir).map_err(|_| {
        BuildError::Invalid(format!(
            "{} is not inside the bundle prefix {}",
            path.display(),
            prefix_dir.display()
        ))
    })?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The count is over *distinct* codes, and every point is counted — the two numbers the
    /// report prints are not derived from each other.
    #[test]
    fn occupancy_counts_distinct_cells_and_every_point() {
        let o = Occupancy::of_sorted_codes([7u32, 7, 7, 9, 9, 40]);
        assert_eq!((o.points, o.cells), (6, 3));
        assert!((o.points_per_cell() - 2.0).abs() < 1e-9);
        assert_eq!(Occupancy::of_sorted_codes([]).cells, 0);
    }

    /// A run count is exact only in tiler order, and out of order it *over*-reports cells — which
    /// would be a warning that quietly stops firing. Loud instead.
    #[test]
    #[should_panic(expected = "tiler order")]
    fn occupancy_refuses_codes_out_of_tiler_order() {
        Occupancy::of_sorted_codes([9u32, 7]);
    }

    /// **Ten points in ten cells is a perfectly framed corpus, not a sparse one.** The measure is
    /// a ratio for exactly this reason: a count of cells would scold every small corpus.
    #[test]
    fn a_tiny_well_framed_corpus_earns_no_warning() {
        let o = Occupancy::of_sorted_codes(0..10u32);
        assert_eq!(o.warning("s0"), None);
        assert!(o
            .report("s0")
            .contains("10 point(s) landed in 10 distinct cell(s)"));
    }

    /// The threshold is a ratio, so it fires at the same proportion at any scale — and the raw
    /// numbers are printed either side of it. Ten points per occupied cell is the same statement
    /// as a tenth of the points having a position of their own, which is how it is reported.
    #[test]
    fn the_warning_fires_on_the_ratio_not_the_size() {
        let just_under = Occupancy {
            points: 99,
            cells: 10,
        };
        let just_over = Occupancy {
            points: 100,
            cells: 10,
        };
        assert_eq!(just_under.warning("s0"), None);
        assert!(just_under
            .report("s0")
            .contains("10.1% of them have a position of their own"));
        let warning = just_over
            .warning("s0")
            .expect("10 points per cell is the threshold");
        assert!(warning.contains("RESOLUTION LOST"), "{warning}");
        assert!(
            warning.contains("stored at the same position and cannot be told apart"),
            "{warning}"
        );
        // The same ratio a thousand times larger says the same thing.
        let big = Occupancy {
            points: 100_000,
            cells: 10_000,
        };
        assert!(big.warning("s0").is_some());
    }

    /// `BuildArgs` carries the deployment key's plaintext hex beside the redacted `IdentityKey`.
    /// A derived `Debug` would undo the redaction on the first `tracing::error!("{args:?}")`.
    #[test]
    fn build_args_debug_does_not_print_the_identity_key() {
        const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
        let args = BuildArgs {
            views: vec![crate::ViewArgs {
                visibility: None,
                view_id: "s0".to_string(),
                projection: tessera_spatial::Projection::None,
                extent: Bounds {
                    x_min: 0.0,
                    x_max: 1.0,
                    y_min: 0.0,
                    y_max: 1.0,
                },
                points: PathBuf::from("points.parquet"),
                point_fields: Default::default(),
                select: None,
                access: crate::config::AccessInput::relation(PathBuf::from("pairs.parquet")),
            }],
            anchor: 0,
            groups: Vec::new(),
            scoped_attributes: Vec::new(),
            attribute_sources: Vec::new(),
            out: PathBuf::from("out"),
            limit: None,
            identity_key: tessera_types::IdentityKey::from_hex(KEY_HEX).unwrap(),
            identity_key_hex: KEY_HEX.to_string(),
            idset: 1,
            shard_id: 0,
            layers: Vec::new(),
            layer_inputs: Vec::new(),
            scoped_layers: Default::default(),
            mint_external_ids: true,
            emit_oracle_pairs: true,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema: Default::default(),
        };
        let printed = format!("{args:?}");
        assert!(
            !printed.contains(KEY_HEX),
            "Debug must not print key material, got: {printed}"
        );
        assert!(printed.contains("fp:"), "got: {printed}");
    }

    #[test]
    fn signature_key_is_sorted_deduplicated_and_order_independent() {
        let a = signature_sort_key(&[TermId::new(5), TermId::new(1), TermId::new(5)]);
        assert_eq!(a, vec![1, 5]);
        assert_eq!(signature_sort_key(&[TermId::new(1), TermId::new(5)]), a);
    }

    #[test]
    fn external_ids_split_at_the_extent_boundary() {
        use arrow::array::{Array, BinaryArray, UInt32Array};

        let temp = tempfile::TempDir::new().unwrap();
        // Source ids chosen so that byte order and numeric order disagree — the sort is over the
        // little-endian encoding (R4), so 0x0100 must come *after* 0xFF.
        let sources: Vec<u64> = vec![0x00FF, 0x0100, 0x0001, 0x0200, 0xFF00, 0x0002, 0x1234];
        let mut rows: Vec<ExternalIdRow> = sources
            .iter()
            .enumerate()
            .map(|(entity, &source)| ExternalIdRow::new(source, entity as u32))
            .collect();
        rows.sort_unstable_by_key(ExternalIdRow::sort_key);

        for rows_per_extent in [1usize, 2, 3, 6, 7, 8, 100] {
            let dir = temp.path().join(format!("split-{rows_per_extent}"));
            fs::create_dir_all(&dir).unwrap();
            let paths = write_external_id_runs(&dir, &rows, rows_per_extent).unwrap();
            assert_eq!(
                paths.len(),
                rows.len().div_ceil(rows_per_extent),
                "wrong extent count at {rows_per_extent} rows per extent"
            );
            for (idx, path) in paths.iter().enumerate() {
                assert_eq!(
                    path.file_name().unwrap(),
                    &*format!("external-ids-{idx}.arrow")
                );
            }

            // Read the extents back in order: the concatenation must be every row exactly once,
            // still in ascending external-id byte order, with each entity id beside its own id.
            let mut seen: Vec<(Vec<u8>, u64)> = Vec::new();
            for path in &paths {
                let reader =
                    arrow::ipc::reader::FileReader::try_new(File::open(path).unwrap(), None)
                        .unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let ids = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .unwrap();
                    let entities = batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .unwrap();
                    for i in 0..batch.num_rows() {
                        seen.push((ids.value(i).to_vec(), entities.value(i) as u64));
                    }
                }
            }
            assert_eq!(seen.len(), rows.len());
            assert!(
                seen.windows(2).all(|w| w[0].0 < w[1].0),
                "extents must partition one ascending byte order, got {seen:?}"
            );
            for (bytes, entity) in &seen {
                let source = u64::from_le_bytes(bytes.as_slice().try_into().unwrap());
                assert_eq!(sources[*entity as usize], source);
            }
        }
    }

    #[test]
    fn an_empty_external_id_relation_still_names_one_extent() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = write_external_id_runs(temp.path(), &[], 4).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].exists());
    }

    #[test]
    fn external_id_rows_are_twelve_bytes_and_round_trip() {
        assert_eq!(std::mem::size_of::<ExternalIdRow>(), 12);
        for source in [0u64, 1, 0xFF, 0x0100, u64::MAX, 0x0123_4567_89AB_CDEF] {
            let row = ExternalIdRow::new(source, 7);
            assert_eq!(row.source_id(), source);
            assert_eq!(row.entity_id, 7);
        }
        // The sort key must order by the id's *bytes*, which is not its numeric order: little
        // endian puts 0x0100's low byte (0x00) first, so it sorts *before* the larger-looking
        // 0x00FF (whose low byte is 0xFF).
        assert!(0x0100u64.to_le_bytes() < 0x00FFu64.to_le_bytes());
        let mut ids = [0x00FFu64, 0x0100u64];
        ids.sort_by_key(|id| ExternalIdRow::new(*id, 0).sort_key());
        assert_eq!(ids, [0x0100, 0x00FF]);
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let prefix = Path::new("/bundle/v00000");
        let path = prefix
            .join("partitions")
            .join("default")
            .join("SEGMENTS-0.json");
        assert_eq!(
            relative_to(prefix, &path).unwrap(),
            "partitions/default/SEGMENTS-0.json"
        );
    }
}
