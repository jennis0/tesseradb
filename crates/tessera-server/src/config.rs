//! Fail-closed configuration: `tessera.toml` (SA §7).
//!
//! `[disclosure]` has no defaults at all: absence of the section, or of either key inside it, is
//! a startup error naming design §7.5/§2.3. `min_visible_members` is parsed and stored even though
//! no handler reads it — ⊘ specified, not implemented — because the *startup rule* is what this
//! section enforces: a deployment must state its disclosure parameters rather than inherit them.
//! Every other section either has a documented default (`max_k = 1000`) or is required outright.
//! Credentials are never inline: `[serve]`'s `*_credential_file`/`*_credential_env` pairs are the
//! only way to supply the session/operator bearer secrets.
//!
//! ## The write-path and admission knobs
//!
//! Every one of them is a **performance knob, not a disclosure control**, so under SA §7's rule
//! ("performance knobs default; disclosure controls do not") every one of them defaults, and
//! [`tests::every_stage_2_1_knob_defaults`] pins that reading. They still refuse a **zero**, which
//! is a different thing: a zero is degenerate for every one of these (see
//! [`ConfigError::MustBeNonZero`]), and this file's discipline is to refuse rather than clamp so a
//! typo cannot silently disable a mechanism.
//!
//! Sectioning follows SA §7's own sketch: the write-path knobs sit under `[ingest]` (where SA §7
//! already puts `flush_max_items`, `flush_max_age` and `overlay_soft_limit`), the serving-side
//! ones under `[serve]` beside the other serving knobs. Units are explicit and consistent in the
//! *name* (`_ms`, `_secs`, `_bytes`) rather than in a duration string — a deviation from SA §7's
//! `"60s"` sketch, taken deliberately so a value's unit survives being read out of a log line or
//! a status payload without its key.
//!
//! **One key is inert.** ⊘ Specified, not implemented: `commit_window_max_age_ms`, because an age
//! bound has no subject in an executor whose commit window never waits (see
//! [`DEFAULT_COMMIT_WINDOW_MAX_AGE_MS`]). [`tests::the_commit_window_age_bound_is_inert`] asserts
//! that mechanically, so an operator cannot set it and believe it works without this file's doc
//! having been changed first.
//!
//! `flush_max_items` and `flush_max_age_secs` were inert too, and are not: the flush tick reads
//! both, and the assertion that used to pin their inertness was deleted in the commit that gave
//! them a consumer — which is what it asked for.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Configuration failures. Every variant here is fail-closed: the process must not start serving
/// with a guessed or defaulted value in place of a missing one.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    /// The `[disclosure]` section is absent entirely.
    MissingDisclosureSection,
    /// The `[disclosure]` section is present but missing one of its two required keys.
    MissingDisclosureKey(&'static str),
    /// Neither `*_credential_file` nor `*_credential_env` was set for this credential, or the
    /// named file/env var could not be read.
    MissingCredential(&'static str),
    BadAddr(String),
    /// Only `builtin:passthrough` is available; the wasmtime plugin host is not built.
    UnsupportedPlugin(String),
    /// `serve.k_min = 0`, which switches off §7.2's floor clause — the **I7 guarantee** that a
    /// non-empty tile always draws at least one mark. With no floor, a tile whose visible items all
    /// sit above θ serves nothing, and the sparsest principals' maps go blank exactly where I7
    /// exists to keep them populated. Refused at startup rather than clamped, so a typo cannot
    /// quietly disable an invariant.
    FloorClauseDisabled,
    /// `serve.k_min` exceeds a cap that will clamp it — either `k_max_marks` (the overplot ceiling)
    /// or `max_k` (the machine ceiling). Both clamp the floor, since the effective cap is
    /// `min(k, max_k, k_max_marks)` and the floor is `min(k_min, cap)`. Well-defined either way, but
    /// it means one of the two numbers is not doing what its author thought.
    FloorAboveCap {
        k_min: usize,
        cap_name: &'static str,
        cap: usize,
    },
    /// `serve.pin_ttl_secs` is not strictly below `drain_depth_max × flush_max_age_secs` (§1.3,
    /// §4's relation 1).
    ///
    /// **A saturated ceiling is what this refuses.** Publications landing closer together than
    /// `pin_ttl_secs / drain_depth_max` leave the drain list permanently at its ceiling: the depth
    /// alarm saturates — stopping signalling exactly when depth matters — and pins are dropped by
    /// the trim rather than by their TTL, so a client is `410`d before the lifetime it was
    /// promised. `tessera_engine::pins`' sizing obligation names this as the duty of whichever
    /// stage introduces a periodic publisher; the flush tick is that publisher.
    ///
    /// The message names all three knobs, because an operator told only "the relation fails"
    /// cannot tell which one to move.
    PinDrainRelation {
        pin_ttl_secs: u64,
        drain_depth_max: usize,
        flush_max_age_secs: u64,
    },
    /// `merge.max_merged_segment_bytes` is not strictly below the base segment's size (§4's
    /// relation 2).
    ///
    /// A merge bounded above the base segment could consume it, and a merge that consumes the base
    /// is compaction under another name — it pays a full permutation rewrite and re-emits every
    /// column, banks none of compaction's benefit, and leaves `MANIFEST.files` digesting files
    /// nothing references. See [`DEFAULT_MAX_MERGED_SEGMENT_BYTES`].
    ///
    /// Validated where the bundle is open rather than in [`load`], because the right-hand side is
    /// a property of the deployment's data and not of its configuration — which is also why there
    /// is no fixed default to violate it: an unset key is derived from the base segment
    /// ([`MAX_MERGED_SEGMENT_BASE_FRACTION`]), and only an explicitly-set one is checked here.
    MergeSizeRelation {
        max_merged_segment_bytes: u64,
        base_segment_bytes: u64,
    },
    /// `serve.max_underlay_offset` above the grid's own depth. The §5.2 grid is 2¹⁶ × 2¹⁶, so an
    /// offset beyond 16 can never be usable at any zoom — every request naming it would be refused
    /// at the depth check. It is refused at startup instead, because the value also feeds a
    /// `1 << (2 * offset)` shift and this file validates every other selection constant; leaving one
    /// shift input unbounded is an inconsistent standard rather than a considered exemption.
    UnderlayOffsetTooDeep(u8),
    /// `serve.theta_target_marks = 0`, which anchors θ at `Cut(0)` — a threshold that admits
    /// **nothing**, at every depth, because `0u64.leading_zeros() == 64` so the per-depth shift
    /// always "fits". Every non-empty tile would then draw exactly `k_min` marks at every zoom
    /// forever, with no error raised anywhere: design §7.2's density signal silently gone.
    ///
    /// This is the *same* silent failure mode `Threshold::at_depth`'s `leading_zeros` check exists to
    /// prevent, reachable through config instead of through a shift bug — so it is refused in the
    /// same spirit.
    ThetaTargetZero,
    /// `serve.compute_threads = 0`: the pool this knob sizes must fill the machine, and a
    /// zero-width pool can run nothing at all. Refused rather than clamped to 1, so a typo cannot
    /// quietly turn "one thread per core" into "one thread total".
    ComputeThreadsZero,
    /// `serve.compute_admission = 0`: the compute semaphore would have zero permits, so
    /// every gated request sheds unconditionally — indistinguishable from the server being down,
    /// but silently. Refused rather than clamped to 1 for the same reason as the floor clause.
    ComputeAdmissionZero,
    /// `serve.admission_timeout_ms = 0` would silently disable the bounded queue wait —
    /// every request either starts immediately or sheds instantly, with no queueing at all, which
    /// is what `serve.compute_queue = 0` (a legal value) already expresses explicitly. Refused so
    /// a zero here reads as a mistake rather than a second spelling of that same knob.
    AdmissionTimeoutZero,
    /// `serve.compute_admission + serve.compute_queue` overflows `usize`, or the sum exceeds
    /// `tokio::sync::Semaphore::MAX_PERMITS` (`usize::MAX >> 3`) — the two knobs together size
    /// the outer slots semaphore `AppState::new` builds
    /// (`Semaphore::new(compute_admission + compute_queue)`), and `Semaphore::new` panics past
    /// that bound. Refused here, at config parse, rather than left to panic during server
    /// startup for an absurd but syntactically valid `tessera.toml`.
    ComputeAdmissionQueueOverflow {
        compute_admission: usize,
        compute_queue: usize,
    },
    /// `serve.compute_admission` was left to default and `COMPUTE_ADMISSION_MULTIPLIER *
    /// compute_threads` overflows `usize` — an operator-supplied `compute_threads` extreme enough
    /// to overflow here would silently wrap to a small, wrong permit count in a release build
    /// (overflow checks are off), the same class of silent failure
    /// `ComputeAdmissionQueueOverflow`'s checked-add exists to prevent one step downstream.
    /// Refused here, at the multiplication itself, for the same reason.
    ComputeAdmissionDefaultOverflow {
        compute_threads: usize,
    },
    /// One of the write-path or admission knobs was set to `0`, and `0` is degenerate for
    /// every one of them — not "off", but "on and silently useless". `consequence` names the
    /// specific silent failure at the check site, because a generic "must be non-zero" tells an
    /// operator what to type and nothing about what they nearly did.
    ///
    /// Same refuse-rather-than-clamp discipline as [`ConfigError::FloorClauseDisabled`] and
    /// [`ConfigError::ComputeThreadsZero`]: clamping means a typo silently changed the
    /// configuration, passing it through means a typo silently disabled a mechanism.
    MustBeNonZero {
        key: &'static str,
        consequence: &'static str,
    },
    /// **Relation 1 (lifecycle §4's headroom rule).** The ingest queue's worst-case byte
    /// footprint, plus the bytes reserved for change records, does not sit strictly below the WAL's
    /// configured ceiling — so a full ingest queue could consume the room a deny needs, and a deny
    /// is never refused for load and has no admission control of its own to fall back on.
    ///
    /// Names **both** sides, and the third operand it is a product of, because an operator who is
    /// told only "the relation fails" cannot tell which of three knobs to move.
    WalHeadroom {
        queue_worst_case: u64,
        reserved_deny_headroom: u64,
        wal_hard_limit_bytes: u64,
        ingest_queue_bound: usize,
        ingest_max_batch_bytes: usize,
    },
    /// **Relation 1, unrepresentable.** `ingest_queue_bound × ingest_max_batch_bytes`, or
    /// that product plus the reserved deny headroom, overflows `u64`.
    ///
    /// Refused rather than wrapped, for [`crate::validate_cache_bounds`]'s reason verbatim: a wrap
    /// produces a *small* left-hand side, i.e. it silently admits exactly the collapsing
    /// configuration this relation exists to refuse.
    WalHeadroomOverflow {
        ingest_queue_bound: usize,
        ingest_max_batch_bytes: usize,
    },
    /// **Relation 2.** The serving runtime's blocking pool would have to be sized past
    /// [`SERVING_BLOCKING_THREAD_CEILING`] to cover its declared consumers — see
    /// [`serving_blocking_threads`] for what those are and why the pool is derived rather than
    /// assumed. Both operands are named because both are knobs.
    BlockingThreadCeiling {
        compute_admission: usize,
        ingest_admission: usize,
        required: usize,
    },
    /// **Relation 3.** The ingest path's worst-case *resident*
    /// bytes — queued commands **plus** admitted-but-not-yet-queued handlers, each holding a decoded
    /// batch — exceeds [`INGEST_RESIDENT_CEILING_BYTES`].
    ///
    /// Relations 1 and 2 between them bound WAL bytes and OS threads, and neither bounds heap —
    /// which is the resource that actually binds at 10⁹ (a build was OOM-killed at 46.4 GB RSS).
    /// Without this one, `compute_admission = 8, ingest_admission = 4000` satisfies both of the
    /// others while admitting four thousand concurrent handlers each holding a 16 MiB batch.
    ///
    /// **It is a machine-scale refusal, not a memory budget**, and [`INGEST_RESIDENT_CEILING_BYTES`]
    /// says what it does and does not measure.
    IngestResidentCeiling {
        ingest_queue_bound: usize,
        ingest_admission: usize,
        ingest_max_batch_bytes: usize,
        required: u64,
    },
    /// **Relation 3, unrepresentable.** `(ingest_queue_bound + ingest_admission) ×
    /// ingest_max_batch_bytes` overflows `u64`. Refused rather than wrapped, for
    /// [`ConfigError::WalHeadroomOverflow`]'s reason verbatim: a wrap produces a *small* left-hand
    /// side, i.e. it silently admits exactly the configuration the relation exists to refuse.
    IngestResidentOverflow {
        ingest_queue_bound: usize,
        ingest_admission: usize,
        ingest_max_batch_bytes: usize,
    },
    /// `ingest.ingest_max_batch_bytes` above [`INGEST_MAX_BATCH_BYTES_CEILING`].
    ///
    /// This is the only relation that bounds **one connection's** cost rather than a product over
    /// the configured bounds, and it exists because that is the term nothing else in this file can
    /// reach: an ingest body is buffered in full before the handler runs, and the number of
    /// connections doing so is bounded by neither this process nor `axum::serve`. See the constant
    /// for what that does and does not close.
    IngestBatchBytesCeiling {
        ingest_max_batch_bytes: usize,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::PinDrainRelation {
                pin_ttl_secs,
                drain_depth_max,
                flush_max_age_secs,
            } => write!(
                f,
                "serve.pin_ttl_secs = {pin_ttl_secs} is not below serve.drain_depth_max \
                 ({drain_depth_max}) x ingest.flush_max_age_secs ({flush_max_age_secs}) = {}. \
                 Geometry is published every flush_max_age_secs, so publications land closer \
                 together than pin_ttl_secs / drain_depth_max and the pin drain list sits at its \
                 ceiling permanently: the depth alarm saturates and pins are dropped by the trim \
                 rather than by their TTL. Lower pin_ttl_secs, or raise drain_depth_max (the \
                 cheaper knob -- a drain entry costs roughly one flush segment), or lengthen \
                 flush_max_age_secs and accept the visibility latency",
                *drain_depth_max as u64 * flush_max_age_secs
            ),
            ConfigError::MergeSizeRelation {
                max_merged_segment_bytes,
                base_segment_bytes,
            } => write!(
                f,
                "merge.max_merged_segment_bytes = {max_merged_segment_bytes} is not below this \
                 deployment's base segment ({base_segment_bytes} bytes), so a merge could consume \
                 the base -- which is compaction under another name: it pays a full permutation \
                 rewrite and re-emits every column, banks none of compaction's benefit, and leaves \
                 MANIFEST.files digesting files nothing references"
            ),
            ConfigError::Io(e) => write!(f, "config io error: {e}"),
            ConfigError::Toml(e) => write!(f, "config parse error: {e}"),
            ConfigError::MissingDisclosureSection => write!(
                f,
                "tessera.toml is missing its [disclosure] section — design §7.5/§2.3: \
                 disclosure parameters have no defaults, so startup refuses rather than silently \
                 choosing one"
            ),
            ConfigError::MissingDisclosureKey(key) => write!(
                f,
                "tessera.toml's [disclosure] section is missing '{key}' — design §7.5/§2.3: \
                 disclosure parameters have no defaults, so startup refuses rather than silently \
                 choosing one"
            ),
            ConfigError::MissingCredential(which) => write!(
                f,
                "no credential configured for '{which}' — set *_credential_file or \
                 *_credential_env in [serve], never inline in tessera.toml"
            ),
            ConfigError::BadAddr(raw) => write!(f, "not a valid listen address: '{raw}'"),
            ConfigError::UnsupportedPlugin(module) => write!(
                f,
                "unsupported plugin module '{module}' — this build ships only \
                 builtin:passthrough (the wasmtime plugin host is not built)"
            ),
            ConfigError::FloorClauseDisabled => write!(
                f,
                "serve.k_min = 0 switches off design §7.2's floor clause, which is the I7 \
                 guarantee that a non-empty tile always draws at least one mark; with no floor a \
                 tile whose visible items all sit above the threshold serves nothing. Startup \
                 refuses rather than clamping, so a typo cannot quietly disable an invariant"
            ),
            ConfigError::FloorAboveCap {
                k_min,
                cap_name,
                cap,
            } => write!(
                f,
                "serve.k_min ({k_min}) exceeds serve.{cap_name} ({cap}) — the floor would be \
                 clamped to that cap on every tile, so one of the two is not doing what its author \
                 intended"
            ),
            ConfigError::UnderlayOffsetTooDeep(offset) => write!(
                f,
                "serve.max_underlay_offset ({offset}) exceeds the grid's own depth of 16 (§5.2), so \
                 no zoom could ever use it"
            ),
            ConfigError::ThetaTargetZero => write!(
                f,
                "serve.theta_target_marks = 0 anchors design §7.2's threshold at a cut that admits \
                 nothing, at every depth — so every non-empty tile would draw exactly k_min marks \
                 at every zoom, with the density signal silently gone. Startup refuses rather than \
                 serving a map that looks plausible and conveys nothing"
            ),
            ConfigError::ComputeThreadsZero => write!(
                f,
                "serve.compute_threads = 0 — the compute pool must fill the machine; a \
                 zero-width pool can run nothing. Startup refuses rather than clamping to 1"
            ),
            ConfigError::ComputeAdmissionZero => write!(
                f,
                "serve.compute_admission = 0 — the compute semaphore would have zero permits, so \
                 every gated request would shed unconditionally. Startup refuses rather than \
                 clamping to 1"
            ),
            ConfigError::AdmissionTimeoutZero => write!(
                f,
                "serve.admission_timeout_ms = 0 would silently disable the bounded queue wait \
                 — use serve.compute_queue = 0 to disable queueing explicitly instead"
            ),
            ConfigError::ComputeAdmissionQueueOverflow {
                compute_admission,
                compute_queue,
            } => write!(
                f,
                "serve.compute_admission ({compute_admission}) + serve.compute_queue \
                 ({compute_queue}) overflows usize or exceeds tokio::sync::Semaphore::MAX_PERMITS \
                 ({}) — the outer slots semaphore cannot be built at this size; lower one or both \
                 knobs",
                tokio::sync::Semaphore::MAX_PERMITS
            ),
            ConfigError::ComputeAdmissionDefaultOverflow { compute_threads } => write!(
                f,
                "serve.compute_threads ({compute_threads}) is too large: the default \
                 serve.compute_admission = {COMPUTE_ADMISSION_MULTIPLIER} * compute_threads \
                 overflows usize — set serve.compute_admission explicitly to a sane value instead"
            ),
            ConfigError::MustBeNonZero { key, consequence } => write!(
                f,
                "{key} = 0 is degenerate, not 'off': {consequence}. Startup refuses rather than \
                 clamping, so a typo cannot quietly disable a mechanism"
            ),
            ConfigError::WalHeadroom {
                queue_worst_case,
                reserved_deny_headroom,
                wal_hard_limit_bytes,
                ingest_queue_bound,
                ingest_max_batch_bytes,
            } => write!(
                f,
                "the ingest queue's worst case ({ingest_queue_bound} × {ingest_max_batch_bytes} B \
                 = {queue_worst_case} B) plus the {reserved_deny_headroom} B reserved for change \
                 records is {} B, which does not sit strictly below ingest.wal_hard_limit_bytes = \
                 {wal_hard_limit_bytes} B. Refusing to start: a full ingest queue could then \
                 consume the WAL room a deletion or suppression needs, and denies are NEVER \
                 refused for load (contracts §3.1), so they have no admission control of their own \
                 to fall back on — the failure would be an append error on a security operation, \
                 not a 429 on an ingest. Lower ingest.ingest_queue_bound or \
                 ingest.ingest_max_batch_bytes, or raise ingest.wal_hard_limit_bytes. Resident heap \
                 is a SEPARATE and LARGER relation — queued commands hold their rows in memory, and \
                 so does every admitted-but-not-yet-queued handler — and it is checked against \
                 ingest.ingest_admission too; see the ingest-resident refusal",
                queue_worst_case.saturating_add(*reserved_deny_headroom)
            ),
            ConfigError::WalHeadroomOverflow {
                ingest_queue_bound,
                ingest_max_batch_bytes,
            } => write!(
                f,
                "ingest.ingest_queue_bound ({ingest_queue_bound}) × ingest.ingest_max_batch_bytes \
                 ({ingest_max_batch_bytes} B) overflows u64, so no WAL ceiling could satisfy the \
                 headroom relation. Refusing to start rather than wrapping: a wrap produces a \
                 SMALL worst case, i.e. it would silently admit exactly the configuration this \
                 check exists to refuse"
            ),
            ConfigError::BlockingThreadCeiling {
                compute_admission,
                ingest_admission,
                required,
            } => write!(
                f,
                "serve.compute_admission ({compute_admission}) + ingest.ingest_admission \
                 ({ingest_admission}) + the reserve needs a blocking pool of {required} threads, \
                 above the {SERVING_BLOCKING_THREAD_CEILING}-thread ceiling. Refusing to start: \
                 a blocking thread's stack is 2 MiB of address space, so this configuration \
                 reserves up to {} GiB of it for stacks — a worst case, not a prediction, since \
                 tokio spawns blocking threads lazily and reaps them idle and Linux commits stack \
                 pages only as they are touched. Lower serve.compute_admission or \
                 ingest.ingest_admission. (The pool is SIZED from these two knobs rather than \
                 assumed — an admitted request can never find no thread — so this refusal is about \
                 the machine, not about the relation between them.)",
                (*required as u64 * 2) / 1024
            ),
            ConfigError::IngestResidentCeiling {
                ingest_queue_bound,
                ingest_admission,
                ingest_max_batch_bytes,
                required,
            } => write!(
                f,
                "the ingest path's worst-case resident bytes — ({ingest_queue_bound} queued + \
                 {ingest_admission} admitted) × {ingest_max_batch_bytes} B = {required} B — exceed \
                 the {INGEST_RESIDENT_CEILING_BYTES} B ingest-resident ceiling. Refusing to start: \
                 a queued command holds its rows in memory, and so does every admitted handler that \
                 has decoded its batch and not yet submitted (or is blocked on its receipt), so \
                 BOTH knobs multiply the byte cap. The real figure is several times this one — a \
                 decoded Vec<IngestItem> is larger than the wire bytes it came from, by roughly an \
                 order of magnitude for the narrowest rows — which is why the ceiling sits well \
                 below any machine's RAM rather than at it. Lower ingest.ingest_admission, \
                 ingest.ingest_queue_bound or ingest.ingest_max_batch_bytes"
            ),
            ConfigError::IngestResidentOverflow {
                ingest_queue_bound,
                ingest_admission,
                ingest_max_batch_bytes,
            } => write!(
                f,
                "(ingest.ingest_queue_bound ({ingest_queue_bound}) + ingest.ingest_admission \
                 ({ingest_admission})) × ingest.ingest_max_batch_bytes ({ingest_max_batch_bytes} B) \
                 overflows u64. Refusing to start rather than wrapping: a wrap produces a SMALL \
                 worst case, i.e. it would silently admit exactly the configuration this check \
                 exists to refuse"
            ),
            ConfigError::IngestBatchBytesCeiling {
                ingest_max_batch_bytes,
            } => write!(
                f,
                "ingest.ingest_max_batch_bytes = {ingest_max_batch_bytes} B exceeds the \
                 {INGEST_MAX_BATCH_BYTES_CEILING} B per-connection ceiling. Refusing to start: an \
                 ingest body is buffered in full before any handler runs, so this key is what ONE \
                 credentialed connection costs, and nothing bounds how many there are — \
                 axum::serve applies no connection cap. The resident relation over \
                 ingest_queue_bound and ingest_admission bounds the admitted window, not the \
                 arrivals in front of it, so without this ceiling a configuration with small \
                 bounds and a gigabyte batch cap passes every other check and dies on the second \
                 concurrent upload. This ceiling does NOT bound the total: N connections still \
                 cost N times this number, and a deployment exposing the control plane beyond a \
                 trusted admin network needs a reverse proxy to bound N"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Toml(e)
    }
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// **`deny_unknown_fields` on every raw section.** Serde's default is to ignore what it does not
/// recognise, which for this file means a typo'd section header (`[ingestion]`) or key
/// (`wal_hard_limit`) parses clean and silently defaults — against this module's own stated
/// discipline that every check here refuses rather than clamps, so a typo cannot silently disable
/// an invariant. An operator who sets a key and gets the default has no signal at all that they
/// did.
///
/// The cost is that a `tessera.toml` carrying a key from a *newer* build is refused rather than
/// ignored. That is the right direction for a fail-closed config: a downgrade that silently drops
/// half an operator's tuning is the worse outcome.
///
/// `[disclosure]` is exempt in practice — it is parsed as a `toml::Value` and validated by hand
/// below, since its rule is "present, with both keys" rather than a shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    bundle: RawBundle,
    plugin: RawPlugin,
    #[serde(default)]
    disclosure: Option<toml::Value>,
    serve: RawServe,
    /// SA §7's `[ingest]` section — every key optional, so the whole section may be absent. These
    /// are the write path's performance knobs; none of them is a disclosure control.
    #[serde(default)]
    ingest: RawIngest,
}

/// SA §7's `[ingest]` section. Every field is `Option` and the struct is `Default`, so
/// a `tessera.toml` with no `[ingest]` section at all parses to "every key defaulted".
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawIngest {
    #[serde(default)]
    commit_window_max_items: Option<usize>,
    #[serde(default)]
    commit_window_max_age_ms: Option<u64>,
    #[serde(default)]
    ingest_queue_bound: Option<usize>,
    #[serde(default)]
    ingest_admission: Option<usize>,
    #[serde(default)]
    ingest_max_batch_rows: Option<usize>,
    #[serde(default)]
    ingest_max_batch_bytes: Option<usize>,
    #[serde(default)]
    wal_hard_limit_bytes: Option<u64>,
    #[serde(default)]
    overlay_soft_limit: Option<usize>,
    #[serde(default)]
    flush_max_items: Option<usize>,
    #[serde(default)]
    flush_max_age_secs: Option<u64>,
    #[serde(default)]
    ingest_buffer_max_items: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundle {
    path: PathBuf,
    cache: PathBuf,
    wal: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    module: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServe {
    viewer: String,
    session: String,
    control: String,
    #[serde(default)]
    max_k: Option<usize>,
    /// Emit the `x-tessera-stage-ns` breakdown header. Defaults to **false**, and has no effect
    /// at all unless the binary was also built with the `bench-timing` feature.
    #[serde(default)]
    stage_timing: Option<bool>,
    #[serde(default)]
    k_min: Option<usize>,
    #[serde(default)]
    k_max_marks: Option<usize>,
    #[serde(default)]
    theta_target_marks: Option<u64>,
    #[serde(default)]
    max_underlay_offset: Option<u8>,
    #[serde(default)]
    max_underlay_cells: Option<usize>,
    #[serde(default)]
    max_tiles_per_request: Option<usize>,
    #[serde(default)]
    session_credential_file: Option<PathBuf>,
    #[serde(default)]
    session_credential_env: Option<String>,
    #[serde(default)]
    operator_credential_file: Option<PathBuf>,
    #[serde(default)]
    operator_credential_env: Option<String>,
    #[serde(default)]
    compute_threads: Option<usize>,
    #[serde(default)]
    compute_admission: Option<usize>,
    #[serde(default)]
    compute_queue: Option<usize>,
    #[serde(default)]
    admission_timeout_ms: Option<u64>,
    #[serde(default)]
    row_projection_cache_bytes: Option<u64>,
    #[serde(default)]
    fragment_cache_bytes: Option<u64>,
    #[serde(default)]
    expected_concurrent_sessions: Option<usize>,
    #[serde(default)]
    pin_ttl_secs: Option<u64>,
    #[serde(default)]
    drain_depth_max: Option<usize>,
    #[serde(default)]
    segment_floor_bytes: Option<u64>,
    #[serde(default)]
    max_merged_segment_bytes: Option<u64>,
    #[serde(default)]
    tier_width: Option<usize>,
    #[serde(default)]
    pins_per_session_max: Option<usize>,
    /// Browser origins permitted to call the viewer and session planes.
    ///
    /// **Absent means no CORS layer at all**, which is the only sensible default for a key whose
    /// effect is to let a page from another origin present a session token and the session
    /// credential. There is deliberately no environment variable and no wildcard: this is a
    /// development affordance, and the enumerated list is what keeps it from becoming an
    /// integration pattern. See [`crate::cors`].
    #[serde(default)]
    dev_cors_origins: Option<Vec<String>>,
}

/// The control plane's listen target: a real unix socket, or (tests, and the documented Windows
/// shape — SA §4.2) a loopback TCP address, since `reqwest` does not speak unix sockets.
#[derive(Debug, Clone)]
pub enum ControlListen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bundle_path: PathBuf,
    pub cache_dir: PathBuf,
    pub wal_path: PathBuf,
    /// The disclosure floor. Parsed and stored because design §7.5/§2.3 makes a missing
    /// `[disclosure]` section a refusal to start.
    /// ⊘ Specified, not implemented: no handler reads it, so nothing is suppressed for being below
    /// the floor.
    pub min_visible_members: u64,
    pub token_max_lifetime_secs: u64,
    pub viewer_addr: SocketAddr,
    pub session_addr: SocketAddr,
    pub control_listen: ControlListen,
    pub max_k: usize,
    /// §7.2's floor clause. See `EngineConfig::k_min`.
    pub k_min: usize,
    /// §7.2's cap clause — the *overplot* ceiling, distinct from `max_k`'s machine ceiling. See
    /// `EngineConfig::k_max_marks`.
    pub k_max_marks: usize,
    /// θ's anchor target. See `EngineConfig::theta_target_marks`.
    pub theta_target_marks: u64,
    pub max_underlay_offset: u8,
    pub max_underlay_cells: usize,
    /// Availability bound on the base viewport path. See `EngineConfig::max_tiles_per_request`.
    pub max_tiles_per_request: usize,
    /// Emit `x-tessera-stage-ns` on viewport responses. **Fails closed**: absent means false, and
    /// even true does nothing in a binary built without the `bench-timing` feature. The header
    /// carries only durations and row counts — no identifier, no per-principal label (SA §9) —
    /// but it quantifies the C4 timing channel, so it stays off unless a measurement asked for it.
    pub stage_timing: bool,
    /// `serve.dev_cors_origins`. Empty is the default and means no CORS layer is mounted at all —
    /// not a layer that allows nothing. See [`crate::cors`] for why this is a development
    /// affordance rather than an integration feature.
    pub dev_cors_origins: Vec<String>,
    pub session_credential: String,
    pub operator_credential: String,
    /// Sizes `Engine::open`'s shared rayon pool, which should fill the machine.
    pub compute_threads: usize,
    /// The compute semaphore's permit count — a bound on in-flight *requests*, admitted for the
    /// viewer/session planes only (never the control plane), not a bound on runnable CPU:
    /// `compute_threads` (the rayon pool) still bounds the parallel-sweep CPU each admitted
    /// request may fan out across, and this gate deliberately lets the serialise phase
    /// oversubscribe up to `compute_admission` because small requests are latency-bound on
    /// scheduling, not CPU. Defaults to [`COMPUTE_ADMISSION_MULTIPLIER`]`× compute_threads` — see
    /// that constant's doc for the measurement behind the multiplier.
    pub compute_admission: usize,
    /// The outer slots semaphore's *additional* permits beyond `compute_admission` — the
    /// bounded queue. Legally `0` (shed the instant every compute permit is busy). Defaults to
    /// `2 × compute_admission`, now effectively `2 × COMPUTE_ADMISSION_MULTIPLIER = 8×` cores.
    pub compute_queue: usize,
    /// How long a request may wait for a compute permit before it is shed with 429
    /// `backpressure` and `Retry-After: 1`.
    pub admission_timeout_ms: u64,

    // ---- The write-path and admission knobs. The argument for each default is on its
    // `DEFAULT_*` constant.
    /// The **row** count at which a commit window closes (an *item* is a row; entries are admitted
    /// whole, so a window closes at or just past this).
    /// See [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`]. `1` is the honest way to disable group commit.
    pub commit_window_max_items: usize,
    /// **INERT** — parsed, validated, stored, and read by nothing, because a commit window never
    /// waits and so has no age to bound; see [`DEFAULT_COMMIT_WINDOW_MAX_AGE_MS`] for the argument
    /// and [`tests::the_commit_window_age_bound_is_inert`] for the mechanical assertion.
    pub commit_window_max_age_ms: u64,
    /// The bounded work queue's depth; full is a 429 with `retry_after_s`.
    /// See [`DEFAULT_INGEST_QUEUE_BOUND`]. The deny queue is unbounded and this never bounds it.
    pub ingest_queue_bound: usize,
    /// Concurrent `/control/ingest` handlers admitted; over is 429. See
    /// [`DEFAULT_INGEST_ADMISSION`]. Distinct from [`Config::ingest_queue_bound`], which bounds
    /// *queued commands*: the two bound different resources and move independently.
    pub ingest_admission: usize,
    /// Per-request row cap on `/control/ingest`; over is 422 (contracts §3.1).
    /// See [`DEFAULT_INGEST_MAX_BATCH_ROWS`].
    pub ingest_max_batch_rows: usize,
    /// Per-request body-byte cap on `/control/ingest`; over is 422. This is the operand of the
    /// headroom assertion (see [`DEFAULT_WAL_HARD_LIMIT_BYTES`]), because a queue bounded in
    /// *entries* bounds nothing without it. See [`DEFAULT_INGEST_MAX_BATCH_BYTES`].
    pub ingest_max_batch_bytes: usize,
    /// The WAL's byte ceiling *as a startup relation between config values*, and the right-hand
    /// side of the headroom assertion. **Not a runtime ceiling: appends do not stop here** — `Wal`
    /// has no length accessor, so nothing compares the live log against this number. See
    /// [`DEFAULT_WAL_HARD_LIMIT_BYTES`] for what would be needed to make the name true.
    pub wal_hard_limit_bytes: u64,
    /// Overlay depth at which an alarm is raised. See [`DEFAULT_OVERLAY_SOFT_LIMIT`].
    /// **Alarms only** — ⊘ no compaction fold exists, so crossing it gets an operator a signal,
    /// never relief.
    pub overlay_soft_limit: usize,
    /// Buffer occupancy at which a flush becomes **ready** — not at which it publishes. See
    /// [`DEFAULT_FLUSH_MAX_ITEMS`] and [`Config::flush_max_age_secs`].
    pub flush_max_items: usize,
    /// The flush tick: the period at which geometry is published, and therefore the bound on how
    /// stale an acknowledged item's absence may be. See [`DEFAULT_FLUSH_MAX_AGE_SECS`], and
    /// [`ConfigError::PinDrainRelation`] for the relation that bounds it from below.
    pub flush_max_age_secs: u64,
    /// Buffer occupancy at which `/control/ingest` is refused with a 429 (§1.3).
    ///
    /// A **distinct knob** from [`Config::ingest_queue_bound`], which bounds queued *commands*:
    /// the executor drains a job into the buffer in milliseconds, so no ingest rate produces a 429
    /// by buffer size through that one. See [`DEFAULT_INGEST_BUFFER_MAX_ITEMS`].
    pub ingest_buffer_max_items: usize,
    /// Superseded generations retained on the pin drain list (lifecycle §2.2). See
    /// [`DEFAULT_DRAIN_DEPTH_MAX`].
    pub drain_depth_max: usize,
    /// Below this, segments compare equal for merge selection, so a tail of tiny ones does not
    /// dominate it. See [`DEFAULT_SEGMENT_FLOOR_BYTES`].
    pub segment_floor_bytes: u64,
    /// Cap on any single merge — and the bound that keeps a merge from swallowing the base
    /// segment, which would be compaction under another name (§5.3).
    ///
    /// **`None` means "derive it from the base segment", and there is deliberately no fixed
    /// default.** §4's relation 2 puts the base segment's size on the right-hand side, and that is
    /// a property of the deployment's data rather than of its configuration: any constant large
    /// enough for a 10⁹-row deployment exceeds a small one's whole base segment, so shipping one
    /// would give a server that refuses to start on its own defaults for every bundle below that
    /// size — the failure `DEFAULT_FLUSH_MAX_AGE_SECS` had against relation 1.
    ///
    /// **⊘ The derivation lands with merge selection**, which is the only thing that will read it;
    /// until then an unset key means "no merge policy is configured", which is accurate because
    /// there is no merge. An explicitly-set value *is* checked — see
    /// [`ConfigError::MergeSizeRelation`] — so an operator cannot configure the fail-open early.
    pub max_merged_segment_bytes: Option<u64>,
    /// Segments in a tier before a merge is selected. See [`DEFAULT_TIER_WIDTH`].
    pub tier_width: usize,
    /// Byte bound on the row-projection cache. See [`DEFAULT_ROW_PROJECTION_CACHE_BYTES`], and
    /// [`MEASURED_PROJECTION_BYTES_AT_1E9`] for the per-entry size the startup validation weighs it
    /// against.
    pub row_projection_cache_bytes: u64,
    /// Byte bound on the *in-memory* fragment tier. See [`DEFAULT_FRAGMENT_CACHE_BYTES`]. The
    /// `.frag` sidecar tier is untouched by it.
    pub fragment_cache_bytes: u64,
    /// The concurrency the projection cache must not collapse at; the startup validation is that
    /// [`Config::row_projection_cache_bytes`] admits at least this many entries. See
    /// [`DEFAULT_EXPECTED_CONCURRENT_SESSIONS`].
    pub expected_concurrent_sessions: usize,
    /// Pin TTL (lifecycle §2.2). See [`DEFAULT_PIN_TTL_SECS`].
    pub pin_ttl_secs: u64,
    /// Per-session pin cap (lifecycle §2.2). See [`DEFAULT_PINS_PER_SESSION_MAX`].
    pub pins_per_session_max: usize,
}

/// The machine ceiling on a viewport's `k` — GPU, transport, handle table.
///
/// **A working value, not a calibration.** The drawn-mark budget spec's probes P1–P3 are what
/// would calibrate this number and they have not run — a `k` measured against the current bundle
/// would be measured against the wrong identity width. So this is a sane working default, and it is
/// labelled as one rather than presented as a measurement.
///
/// Deliberately **above** [`DEFAULT_K_MAX_MARKS`]: the machine ceiling should not be the binding one
/// — the overplot ceiling should be — so that raising what a screen can legibly show does not also
/// require re-reasoning about transport.
const DEFAULT_MAX_K: usize = 1_000;

/// §7.2's floor clause: the fewest marks a non-empty tile draws. Provisional (density memo §4),
/// pending that memo's §0 visual experiments.
const DEFAULT_K_MIN: usize = 2;

/// §7.2's cap clause: the most marks any one tile draws. **The overplot ceiling** — sized by what a
/// screen can legibly show, not by machine limits.
///
/// Density memo §4 argues 128 from ink coverage at ~80x80 px per tile, on the premise that a
/// viewport draws a few hundred tiles and that mark count stops reading as density somewhere around
/// 50–100 marks per tile. That premise is **untested** — the memo says so itself, and it is exactly
/// what the drawn-mark budget's P1 probe exists to settle — and it sits against the standing
/// position that the drawn-mark budget should be the largest a client can render. 500 takes the
/// latter side pending the probe ([decision 0007](../../../docs/decisions/0007-k-max-marks-500.md)).
///
/// It is the number that actually binds: with `k` defaulting to the same value, the effective cap is
/// this, and §7.2's proportional window `cap / k_min` is 250.
const DEFAULT_K_MAX_MARKS: usize = 500;

/// The MACHINE ceiling must sit at or above the OVERPLOT ceiling, so the overplot one is what binds.
/// Inverted, raising what a screen can legibly show would silently do nothing until someone also
/// raised a transport limit — a confusing failure, and exactly the conflation these two constants
/// were split apart to prevent. Checked at compile time rather than in a test: it is a property of
/// the two literals, so it should fail the build.
const _: () = assert!(DEFAULT_MAX_K >= DEFAULT_K_MAX_MARKS);

/// θ's anchor target: marks the mean occupied tile should draw at any depth. Provisional.
const DEFAULT_THETA_TARGET_MARKS: u64 = 16;

/// The largest `underlay_offset` a request may ask for (§3.3): sub-cell depth is `zoom + offset`.
const DEFAULT_MAX_UNDERLAY_OFFSET: u8 = 4;

/// The ceiling on sub-cells in one response. `tiles_for_bbox` is itself uncapped and the underlay
/// multiplies its output by `4^offset`, so without this one request can demand ~77k
/// `count_range` calls and blow the 10 ms p99 latency gate.
const DEFAULT_MAX_UNDERLAY_CELLS: usize = 8192;

/// The most tiles one viewport request may span.
///
/// **Sized against the threat, which is unbounded allocation — not against a latency SLO.** Without
/// a bound, zoom 16 over the full extent is 65536² = 4.29e9 tiles at 16 B each, ~69 GB in one `Vec`:
/// an out-of-memory abort from a single authenticated request. At this limit the tile vector is at
/// most 4 MB and the per-tile range arithmetic is bounded with it.
///
/// It is deliberately *not* tightened to the few hundred tiles a real viewport draws. A wide bbox at
/// a deep zoom is an unusual but legitimate query — the differential suite issues them — and its
/// latency is the caller's own and now bounded. Refusing it would trade an availability fix for a
/// functionality regression.
const DEFAULT_MAX_TILES_PER_REQUEST: usize = 262_144;

/// The compute pool should fill the machine. `available_parallelism` fails only when the OS
/// genuinely cannot answer the question (SA has no fallback story for that host); treated as 1
/// rather than propagated, since a single-threaded fallback still starts the server, and the
/// `ComputeThreadsZero` refusal exists for the case an operator's *explicit* `0` needs catching,
/// not this one.
fn default_compute_threads() -> usize {
    std::thread::available_parallelism().map_or_else(
        |e| {
            // Rare (the OS genuinely could not answer, e.g. an exotic sandboxing setup) but
            // silently sizing the compute pool/admission gate at 1 thread instead of the
            // machine's real core count is a startup-time surprise worth a log line, not a
            // silently-degraded deployment — an operator staring at low throughput later has no
            // other signal that this fallback, rather than an explicit `compute_threads = 1`,
            // is why.
            tracing::warn!(
                error = %e,
                "available_parallelism() failed; falling back to compute_threads = 1 — set \
                 serve.compute_threads explicitly to size the compute pool/admission gate for \
                 this host"
            );
            1
        },
        std::num::NonZeroUsize::get,
    )
}

/// 25× the 10 ms p99 target — a request that cannot even *start* in 250 ms is better shed
/// with `Retry-After: 1` than served at the measured 1.04 s worst case.
const DEFAULT_ADMISSION_TIMEOUT_MS: u64 = 250;

/// `compute_admission`'s default multiplier over `compute_threads`.
///
/// **Not "one CPU-bound request per core".** The calibrated-viewport measurement is that requests at
/// this corpus scale are ~0.3 ms and mostly memory-bound: `compute_admission`
/// bounds in-flight *requests*, not runnable CPU — the rayon pool (`compute_threads`) still bounds
/// the parallel-sweep CPU each admitted request may fan out across, and the serialise phase
/// deliberately oversubscribes up to `compute_admission`, because small requests are latency-bound
/// on scheduling, not CPU, and a 1× gate left cores idle waiting on the next request rather than
/// running the one already queued.
///
/// Measured on a 12-core WSL2 box over a 2.42 M-row fixture: at 1× (`compute_admission=12`)
/// closed-loop throughput was
/// c=5 11,771 / c=100 31,248 / c=1000 28,929 rps against a pre-gate baseline of 15,277 / 48,588 /
/// 49,475 rps, with shed% at c=100/c=1000 around 40%. At 4×, c=100 improves to 35,448 rps
/// (shed% collapses to ~0.1%) and c=1000 to 32,645 rps (shed% to ~1.9%) — a real but partial
/// recovery, not a full one; c=5 is essentially unchanged (11,515 rps) because that cell is
/// latency-/generator-bound, not gate-bound, so a wider gate has nothing to admit that wasn't
/// already getting in. p99 grew 1.02–1.47× over the 1× run across every cell measured, well inside
/// the ~2× bound treated as the regression limit — the trade this constant makes is real (some tail
/// risk under sustained oversubscription, since the queue is `2 × compute_admission` = 8× cores) but
/// it stayed bounded at this measurement.
const COMPUTE_ADMISSION_MULTIPLIER: usize = 4;

// ---------------------------------------------------------------------------------------------
// The write-path and admission knobs, and the argument for each default.
//
// The set is chosen as a *consistent* set, not as independent numbers: two of the startup
// assertions are relations between them (the WAL headroom relation, and the cache-admission one),
// and a default configuration that could not start is not a default.
// `defaults_satisfy_task_6s_headroom_relation` and `defaults_satisfy_task_5s_cache_relation` pin
// both, so an edit to one constant cannot silently make the shipped default config refuse to start.
// ---------------------------------------------------------------------------------------------

/// The **row** count at which a commit window closes.
///
/// **Rows, not submissions, and the arithmetic below is why.** Every quantity this number is sized
/// from — the run length, the heap the window holds, the latency it
/// costs — scales in rows. Read as submissions it would admit 10 000 × `ingest_max_batch_rows` =
/// 10⁸ rows in one window, four orders of magnitude past the resident ceiling
/// [`INGEST_RESIDENT_CEILING_BYTES`] allows, and the run-length figure below would be 2 000 000
/// rather than ~200. Entries are admitted whole (a submission is never split across two windows),
/// so a window closes at or just past this count.
///
/// **Sized from the compression arithmetic, which is the window's whole purpose — but do not size a
/// deployment from the headline.** The probes' 8.9–36.7× posting compression was measured under a
/// *full-corpus* signature sort. `run ≈ commit_window_max_items × term_density` — 10 k rows at the
/// corpus's 2% giving ~200 — is an **upper bound**, not an expectation:
/// entity ids are sorted by an item's whole term *signature*, so a term's ids run contiguously only
/// where that term is effectively the signature. The measured corpus is not shaped that way (probes
/// results §3: 54,791 signatures over 2.42 M items, mean group 44, rank-100 group 4,213 and
/// rank-1,000 group 158), so a 10 k window holds ~17 rows of the hundredth-largest group and under
/// one of the thousandth — **runs of order 10¹, one to two orders of magnitude below the 200**. Size
/// from that, and re-measure before trusting either figure. The probes' headline is
/// policy-dependent, and the per-window re-permutation that would settle it was not run.
///
/// **The instrument that settles it is on `/control/status`.** `fragmentation.run_ratio`
/// (contracts §3.4) is measured at every window close, over the windows a deployment actually
/// produced, so an operator can compare what this value collects in production against the `run ≈
/// rows × term_density` bound above rather than against a corpus nobody ran. Read it with
/// `fragmentation.windows` and `wal_appends / wal_fsyncs` beside it: a run ratio near `1.0` with a
/// fsync ratio near `1.0` means the windows are closing with one entry in them and this key is not
/// the thing to change. Read its own caveats first — it is within-window sort quality against a
/// within-window baseline, not a stream-scope figure.
///
/// Raising this buys run length **sub-linearly** — the extra rows come from progressively smaller
/// signature groups — and costs window latency, the heap the held rows occupy, and sort work:
/// `assign_sorted` is `n log n` in the window's rows, so 100 → 10 000 is twice the comparison work
/// per row. It is not a free dial in the compression direction.
///
/// **Which half of the win that is, stated because the multiplier alone hides it.** Design §11.1's
/// container model gives the benefit available to a term of density `p` at sort scope `B` as
/// `max(1, 2¹⁶/(p·B))`, and `p·B < 2¹⁶` for every `p ≤ 1` once `B ≲ 6·10⁴`. So at this value — and
/// at any value the resident ceiling permits — a window collects the **posting-storage**
/// (run-encoding) win and **none of the container-count** win, and container count is what a union
/// costs.
///
/// **The window adds no residency term, and the argument is worth writing down because it reads the
/// other way round.** Relation 3 bounds resident ingest bytes by `ingest_admission ×
/// ingest_max_batch_rows`, and a window looks like a fourth term outside it. It is not: every entry
/// in an open window has a handler blocked on its
/// receipt, and `control.rs`'s ingest path takes the `IngestAdmission` permit **before**
/// `spawn_blocking` and moves it *into* the closure, so the permit is held until the ack. In-window
/// entries are therefore ≤ `ingest_admission`, and their bytes are the ones relation 3 already
/// counts. This key is the **tighter** bound in the common case, not the load-bearing one; the
/// worst-case window is `commit_window_max_items − 1 + ingest_max_batch_rows` ≈ 20 000 rows, which is
/// what "at or just past this count" above means.
///
/// **It equals [`DEFAULT_INGEST_MAX_BATCH_ROWS`] deliberately** (that constant's own doc makes the
/// same point from the other side): one maximal batch is one maximal window, so no client can
/// define the window's size by picking a chunk size. The consequence to know: at the defaults a
/// maximal batch commits alone and gains nothing from grouping — it is the *small* batches the
/// window collects — and an operator who raises `ingest_max_batch_rows` without raising this gets
/// one-entry windows.
///
/// **`1` is how you disable group commit**, and an A/B between one large window and a hundred small
/// ones needs that spelling to exist. `0` is refused: a window that closes at zero rows
/// is not "off", it is group commit silently doing nothing while the code that implements it
/// still runs.
const DEFAULT_COMMIT_WINDOW_MAX_ITEMS: usize = 10_000;

/// **INERT.** The age at which a commit window would close, if a commit window ever waited. It does
/// not, so **nothing reads this value and an operator who sets it changes nothing at all**
/// (`tests::the_commit_window_age_bound_is_inert` fails the moment anything outside this module
/// uses the key).
///
/// **Three plausible claims about it are all false of the code**, and are worth naming because each
/// would be a reason to wire the key up: denies do *not* share the window (it is ingest-only, and
/// denies ride the never-shed lane); the worst-case deny starvation is *not* `2 ×` this value (that
/// arithmetic needs both an age bound and a deny in the window, and has neither); and "both queues
/// empty" is *not* the close trigger (the trigger is the **work** queue observed empty —
/// `run_work_pass` never looks at the deny lane).
///
/// **What an age bound would buy: nothing.** It is the safety cap on a *linger* — "having drained
/// the queue empty, wait for company" — and the executor has no linger. A `CommitWindow` is a local
/// of `Executor::run_work_pass` that every exit disposes of; no window survives the executor's one
/// blocking point. So the interval a timer would end does not exist, and the only place such a
/// check could fire is inside the drain, where it is a less predictable spelling of
/// [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`] — that loop does hashing, not IO, and the rows it can gather
/// are bounded by the row bound (worst case `commit_window_max_items - 1 + ingest_max_batch_rows`,
/// ≈ 20 000) rather than by a clock. The full argument, including why the interval where the queue
/// momentarily empties while more work is imminent is real but is a window closing *too early* —
/// the wrong sign for an age bound — is at `Executor::run_work_pass`.
///
/// **The key is kept rather than deleted** because every raw config section is
/// `deny_unknown_fields`: removing it would turn any existing `tessera.toml` that sets
/// `ingest.commit_window_max_age_ms` into a start-up refusal. That is the same trade the two
/// `flush_*` knobs make — an inert key that says so beats a compatibility break. Anything that
/// builds a linger and gives this key a consumer must delete this paragraph, the `Config` field's
/// INERT note and `the_commit_window_age_bound_is_inert` in the same commit.
const DEFAULT_COMMIT_WINDOW_MAX_AGE_MS: u64 = 200;

/// The bounded work queue's depth. Full is a 429 with `retry_after_s`; the deny queue is
/// separate and unbounded, and this never bounds it (lifecycle §1.3's deny priority lane).
///
/// **Bounded by heap, not by taste.** A queued `Command` holds its rows in memory, so the queue's
/// worst case is `ingest_queue_bound × ingest_max_batch_bytes` = 32 × 16 MiB = **512 MiB**. Memory
/// is already the binding constraint at 10⁹ (a build was OOM-killed at 46.4 GB RSS), so
/// this is deliberately a small number: backpressure that arrives early is a working queue,
/// backpressure that arrives at the OOM killer is not. It is only *half* the ingest path's resident
/// worst case — the other half is [`DEFAULT_INGEST_ADMISSION`]'s, and
/// [`INGEST_RESIDENT_CEILING_BYTES`] is the relation over both.
///
/// # Why it is strictly below [`DEFAULT_INGEST_ADMISSION`], and which 429 that makes live
///
/// Set equal to the admission bound, **the queue-full 429 is unreachable in any operator-legal
/// configuration**. `Engine::accept_ingest` blocks on its receipt, so an admitted handler holds at
/// most one work-queue entry at a time and outstanding entries are bounded by admitted handlers:
/// with `A = Q = 64` the queue peaks at 63 (the executor holds one in flight) and `try_send` can
/// never observe `Full`. `SubmitError::QueueFull`, `estimate_retry_after_s(depth, ..)` for any
/// `depth > 0`, and the whole `retry_after_s` wire surface would be dead code at the shipped
/// defaults, reachable only through `start_write_executor(0)` — a spelling `non_zero_usize` refuses
/// to an operator.
///
/// So `Q < A`, and the consequence is worth stating rather than leaving to be re-derived:
///
/// - the **queue-full** 429 is the shed path a burst normally meets. It fires the moment more than
///   32 batches are simultaneously submitted, which is the honest "this node is behind on writes"
///   signal, and its `retry_after_s` is derived from the queue's own depth and drain;
/// - the **admission** 429 bounds *blocking threads*, and at these defaults it fires only when
///   handlers accumulate somewhere that holds no queue slot — a slow `terms_of_label`, slow sidecar
///   IO, or a long-running executor item keeping 64 handlers parked on receipts. It is not a
///   backstop for the queue; the two bound different resources, which is the whole reason both keys
///   exist.
const DEFAULT_INGEST_QUEUE_BOUND: usize = 32;

/// Concurrent `/control/ingest` handlers admitted at once. Over is a 429 with its own
/// derived `retry_after_s`; the refusal costs no blocking thread, no queue slot and no WAL byte.
///
/// **It bounds blocking-pool threads. It does not bound queued commands.**
/// [`DEFAULT_INGEST_QUEUE_BOUND`] does that. The distinction is the whole reason this is a separate
/// key.
///
/// **It is, however, a heap operand, and [`INGEST_RESIDENT_CEILING_BYTES`] is the relation that
/// says so.** An admitted handler holds a fully decoded batch from the Arrow decode until its
/// receipt returns — the window this constant's own argument below enumerates — so
/// `ingest_admission × ingest_max_batch_bytes` is resident *beside* the queue's own worst case, not
/// inside it. Bounding only threads and WAL bytes leaves that term out entirely, which is how
/// `compute_admission = 8, ingest_admission = 4000` passes those two relations while admitting four
/// thousand concurrent 16 MiB batches. The resident relation is a machine-scale refusal, not a
/// memory budget.
///
/// # Why this is not derived from `ingest_queue_bound`
///
/// The tempting derivation is "admitting more concurrent handlers than the queue can hold buys
/// nothing, since the surplus can only 429 anyway". That is false, and the refutation is what makes
/// this a key rather than an expression. An ingest handler holds a blocking thread across:
///
/// - Arrow decode, the plugin's `terms_of_label` loop, `resolve_terms` and the external-ID sidecar
///   lookup — during which it holds **no queue slot at all**, because it has not submitted yet; and
/// - its whole blocking wait on the executor's receipt — during which it *also* holds no queue slot,
///   because the executor has dequeued the command and is running it.
///
/// So the useful concurrency is the queue's depth **plus** the handlers doing pre-submit work, and
/// setting this equal to the queue bound truncates that second term to zero — which kills the
/// queue-full 429 outright. [`DEFAULT_INGEST_QUEUE_BOUND`]'s doc has that argument and says which
/// 429 is live at these defaults. Tying the two would also mean an operator raising
/// `ingest_queue_bound` for burst
/// tolerance — a *heap* decision — silently raising blocking-thread demand and moving
/// [`serving_blocking_threads`]'s arithmetic under their feet.
///
/// 64 against the default pool leaves the viewer plane its whole `compute_admission`, by
/// construction rather than by luck — see [`serving_blocking_threads`].
const DEFAULT_INGEST_ADMISSION: usize = 64;

/// The ceiling on the ingest path's worst-case **resident** bytes, and the right-hand side of the
/// third startup relation.
///
/// `(ingest_queue_bound + ingest_admission) × ingest_max_batch_bytes`. Both terms, because both are
/// real and the second is the easy one to miss: a queued `Command` holds its rows, and so does every
/// admitted handler between its Arrow decode and its receipt.
///
/// **What it is not.** It is not a memory budget, it does not measure heap, and nothing observes
/// RSS. It is the same shape as [`SERVING_BLOCKING_THREAD_CEILING`] — a refusal of the configuration
/// that cannot work on any machine this system targets, not of the one that is merely large. Two
/// specific honesty caveats, so no reader takes the number for a measurement:
///
/// 1. **The left-hand side under-states the truth.** It counts *wire* bytes. A decoded
///    `Vec<IngestItem>` — a `Vec<u8>` external id, a `Vec<u8>` access label and a `Vec<WalScalar>`
///    per row — is several times the Arrow body it came from, by roughly an order of magnitude for
///    the narrowest (x/y-only) rows. 16 GiB of *this* arithmetic is therefore a good deal more than
///    16 GiB of RSS. The ceiling is set well under a target machine's RAM for that reason rather
///    than by folding in a multiplier nobody has measured.
/// 2. **It bounds the configuration, not the process.** It weighs the *admitted* window — queued
///    commands and admitted handlers. Requests that have not been admitted yet are in front of it
///    and are not counted: an ingest body is buffered in full before the handler runs, so every
///    credentialed connection costs `ingest_max_batch_bytes` whether or not it ever gets a permit,
///    and `axum::serve` applies no connection cap. That per-connection term is bounded separately
///    by [`INGEST_MAX_BATCH_BYTES_CEILING`]; the *count* of connections is bounded by neither, and
///    that constant says what stands in its place.
///
/// It is also what closes `ingest_queue_bound = 1, ingest_max_batch_bytes = 1 GiB`, which every
/// other relation admitted: `(1 + 64) × 1 GiB = 65 GiB` is refused here. At the default bounds the
/// effective cap this relation puts on `ingest_max_batch_bytes` is `16 GiB / 96` ≈ 170 MiB — but
/// that is a *derived* cap that moves with the other two knobs, which is why the per-connection
/// ceiling is stated independently rather than left to fall out of this one.
const INGEST_RESIDENT_CEILING_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// The ceiling on `ingest.ingest_max_batch_bytes`, and the only startup relation about **one
/// connection** rather than about a product over the configured bounds.
///
/// # The window it bounds, and the half of it that stays open
///
/// `/control/ingest` is buffer-the-whole-body shaped: `DefaultBodyLimit` is set to
/// `ingest_max_batch_bytes` and the `Bytes` extractor holds the whole request before `ingest` runs.
/// The router's credential layer runs outside the extractors, so an *unauthenticated* caller is
/// refused with the body still an unconsumed stream and nothing is buffered on their behalf. A
/// caller holding the operator credential is a different matter: each of its in-flight requests
/// pins up to this many bytes, before `ingest_admission` is consulted and therefore outside every
/// bound that gate provides.
///
/// **What the count is bounded by is a deployment property, not a mechanism in this process.** The
/// control plane is a unix socket by default (SA §4.2) and is meant to be reachable only by admin
/// systems; where it is exposed more widely, the connection bound is the reverse proxy's, and SA §8
/// says so. Two in-process mechanisms were assessed and declined, and neither should be revisited
/// without new information:
///
/// - a `tower` concurrency limit **queues rather than sheds**, so on the ingest route it swallows
///   the prompt 429 the admission bound exists to produce, and on the whole control router it puts
///   `/control/changes` behind an in-flight bound shared with receipt-blocking ingest handlers —
///   lifecycle §1.3's forbidden shape, reintroduced at the router;
/// - a **listener-level connection cap** fails the same test one layer lower, and worse. Declining
///   to accept does not refuse a caller; it leaves them in the kernel's accept backlog with no
///   status code at all. Trading a 429 for a stall is the defect that disqualified the first
///   option, spelled without an error path.
///
/// So the honest close for the *count* is deployment posture, and what this constant closes is the
/// *multiplier*: without it, `ingest_admission = 1, ingest_queue_bound = 1, ingest_max_batch_bytes
/// = 8 GiB` satisfies every other relation — the admitted window is `2 × 8 GiB = 16 GiB`, exactly
/// at the resident ceiling — and then dies on the *second* concurrent upload, before either of
/// those bounds has anything to say. A configuration whose per-connection cost is unbounded makes
/// an unbounded count catastrophic rather than merely unbounded.
///
/// # The number
///
/// 64 MiB is four times [`DEFAULT_INGEST_MAX_BATCH_BYTES`], so an operator with unusually wide rows
/// has room to raise it without meeting this, and the shipped default is nowhere near it. At the
/// ceiling
/// a hundred concurrent uploads is 6.4 GB of buffered bodies — survivable on a machine this system
/// targets, and observable long before it is fatal; at the 8 GiB the other relations admit, one is
/// fatal. **It is a machine-scale refusal, not a memory budget**, the same shape as
/// [`INGEST_RESIDENT_CEILING_BYTES`] and [`SERVING_BLOCKING_THREAD_CEILING`], and it under-states
/// the truth for the same reason: it counts wire bytes, and the decoded `Vec<IngestItem>` the
/// handler goes on to hold is several times larger.
///
/// **Streaming the upload would close this properly and is deliberately not attempted here.** The
/// per-connection cost is a consequence of the endpoint's shape — the whole Arrow body is decoded
/// at once — and making it incremental is a change to the write path that belongs with flush, not
/// a bound bolted onto configuration.
const INGEST_MAX_BATCH_BYTES_CEILING: usize = 64 * 1024 * 1024;

/// The ceiling is **headroom over the shipped default**, not a value in its own right: a default
/// configuration that could not start is not a default, and at exactly the default an operator would
/// have no room to raise the cap for unusually wide rows. Checked at compile time rather than in a
/// test, on [`DEFAULT_MAX_K`]'s reasoning: it is a property of two literals, so it should fail the
/// build.
const _: () = assert!(INGEST_MAX_BATCH_BYTES_CEILING == 4 * DEFAULT_INGEST_MAX_BATCH_BYTES);

/// The WAL bytes reserved for change records above the ingest queue's own worst case.
///
/// **Argued from the record, not chosen for roundness.** A change record is an op, an entity id and
/// a caller-supplied external id capped at 64 bytes (`control.rs`'s `EXTERNAL_ID_MAX_LEN`) plus its
/// descriptors — order 200 B — so 1 GiB is room for roughly five million deny appends *above a
/// completely full ingest queue*. Denies are never refused for load (contracts §3.1) and so have no
/// admission control of their own to fall back on, which is why this is a term in the startup
/// relation and not a comment beside it (lifecycle §4's headroom rule).
const RESERVED_DENY_HEADROOM_BYTES: u64 = 1024 * 1024 * 1024;

/// Blocking threads held back for work that is not one of the two admission-bounded
/// consumers [`serving_blocking_threads`] enumerates.
///
/// The one such consumer *in this workspace* is [`crate::control`]'s `spawn_on_deny_lane`
/// **fallback**: if the deny runtime is unavailable it alarms and runs the suppression on the shared
/// pool, because running there beats refusing a security operation. That is a last resort, so the
/// right size for it is a reserve, not a mechanism.
///
/// It also covers the consumers this workspace does not own — tokio itself dispatches blocking work
/// onto this pool, DNS resolution most visibly. [`serving_blocking_threads`]'s enumeration is
/// scoped to the workspace precisely because this reserve is what stands behind the rest.
const BLOCKING_THREAD_RESERVE: usize = 32;

/// The ceiling on the serving runtime's blocking pool, and the right-hand side of the second
/// startup relation.
///
/// **This is not a bound the arithmetic has to fit under by luck** — [`serving_blocking_threads`]
/// *derives* the pool from its declared consumers, so there is no configuration in which an
/// admitted request finds no thread. What this ceiling refuses is a configuration that would size
/// an OS-thread pool past what the machine can carry: tokio's blocking threads take a 2 MiB stack
/// each, so 4096 of them is 8 GiB of stacks. Without it, `serve.compute_admission` is an operator
/// number with nothing between it and the address space.
///
/// It governs the **serving** pool. The deny lane has its own runtime with its own pool
/// (`control::DENY_MAX_BLOCKING_THREADS`), and this number does not cover it — deliberately, since
/// the whole point of that lane is that it is not sized against ingest.
pub const SERVING_BLOCKING_THREAD_CEILING: usize = 4096;

/// The serving runtime's `max_blocking_threads`, derived from the config's declared consumers.
///
/// **Every consumer *in this workspace* is one of these, and that list is an enumeration rather than
/// an estimate.** It is deliberately not a claim about the process:
/// tokio dispatches its own work onto the same pool, DNS resolution being the one this crate's own
/// test fixtures already rely on. [`BLOCKING_THREAD_RESERVE`] is what covers everything outside the
/// enumeration, which is why the pool is `consumers + reserve` and not `consumers`:
///
/// - the viewer/session closures (`/v1/viewport`, `/v1/items`, `/session/authorise`), each of which
///   awaits `ComputeGate::admit` **before** `spawn_blocking`, so at most `compute_admission` of them
///   hold a thread. `compute_queue` is deliberately **not** a term: a queued request is parked on a
///   semaphore and holds no thread;
/// - `/control/ingest`, at most `ingest_admission`;
/// - `/control/changes`, which contributes **zero** because it runs on its own runtime with its own
///   pool (`control::DENY_RUNTIME`) — except on that lane's alarmed fallback, which
///   [`BLOCKING_THREAD_RESERVE`] covers.
///
/// **Called by `tessera-cli`'s runtime builder, which is the only reason this is `pub`.** The
/// alternative is tokio's undeclared default, which a tokio upgrade or an embedder's own builder
/// invalidates in silence. Deriving the pool from its consumers is also stronger than asserting a
/// constant covers them — an operator who raises
/// `compute_admission` gets a pool that fits, rather than a refusal telling them to lower it again.
///
/// Embedders and every integration test build their own runtime and get none of this. What they
/// keep is the *mechanism* — the ingest admission semaphore is an `AppState` field, so every server
/// however constructed has one — and what they lose is only the sizing.
pub fn serving_blocking_threads(config: &Config) -> usize {
    config
        .compute_admission
        .saturating_add(config.ingest_admission)
        .saturating_add(BLOCKING_THREAD_RESERVE)
}

/// Per-request row cap on `/control/ingest`; over is 422 (contracts §3.1).
///
/// Matched to [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`] deliberately — one maximal batch is one maximal
/// window's worth of rows, so a single client cannot define the window's size by picking a chunk
/// size, which is exactly the property design §11.1 wants when it puts the sort scope on the
/// server. The `rows` cap is the one that binds for ordinary point data; the byte cap
/// below catches unusually wide rows.
const DEFAULT_INGEST_MAX_BATCH_ROWS: usize = 10_000;

/// Per-request body-byte cap on `/control/ingest`; over is 422.
///
/// 16 MiB is ~1.6 KB per row at the row cap above — comfortable headroom over the ~1 KB/row the
/// queue arithmetic assumes, so the row cap is what a normal caller meets and this one only catches
/// pathological rows. **Without a byte cap the queue bound bounds nothing**: a queue bounded in
/// entries lets one ten-million-row batch walk straight past it, which is why this key exists at all
/// rather than the row cap alone.
const DEFAULT_INGEST_MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// The WAL's byte ceiling, and the right-hand side of the startup headroom assertion
/// (`ingest_queue_bound × ingest_max_batch_bytes` + reserved deny headroom must sit strictly below
/// it). Without it nothing bounds the WAL at all: it grows until the filesystem says no, at which
/// point every append fails and, per `WalError::Poisoned`, the handle is dead.
///
/// 8 GiB leaves 7 GiB of headroom above the queue's 1 GiB worst case, so denies — which are never
/// refused for load and therefore have no admission control of their own to fall back on — have
/// somewhere to go even with the ingest queue completely full. It is also small enough to fit the
/// NVMe cache directory SA §7 describes without an operator thinking about it.
///
/// **It bounds a startup relation; it does not stop appends** — in the spirit of
/// [`DEFAULT_OVERLAY_SOFT_LIMIT`]'s "it alarms; it does not act". ⊘ The name reads as a runtime
/// ceiling and is not one: `Wal` exposes no length accessor, so nothing can compare the live WAL
/// against this number. It is consumed in exactly one place — the startup assertion that the
/// queue's worst case plus reserved deny headroom sits strictly below it — and past that point the
/// WAL grows until the filesystem refuses, at which point `WalError::Poisoned` makes the handle
/// dead. Runtime enforcement would need a `Wal::len()` and a ruling on what "at the limit" should do
/// (refusing ingest is straightforward; refusing a *deny* is fail-open), and neither exists.
const DEFAULT_WAL_HARD_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Overlay depth at which an alarm is raised. **SA §7's own default**, carried across unchanged.
///
/// **It alarms; it does not act.** ⊘ Specified, not implemented: no compaction fold exists, so an
/// operator who sets this gets a signal that the overlay is deep, not a mechanism that makes it
/// shallower. The
/// depth matters beyond memory: every deny acceptance clones the overlay inside the WAL critical
/// section, so overlay depth is a term in the deny-ack latency that
/// [`DEFAULT_COMMIT_WINDOW_MAX_AGE_MS`] budgets against.
const DEFAULT_OVERLAY_SOFT_LIMIT: usize = 500_000;

/// **INERT.** The flush trigger by buffered-item count. **SA §7's own default**, carried across
/// unchanged.
///
/// **It marks the buffer flush-*ready*; it does not publish** (§1.3). Publication waits for the
/// next tick, because publishing on trip would move the real period below the one
/// [`ConfigError::PinDrainRelation`] validated — and §2.2's depth trim would then drop pins before
/// their TTL while the depth alarm saturates.
const DEFAULT_FLUSH_MAX_ITEMS: usize = 100_000;

/// **The flush tick**, and therefore the bound on how stale an acknowledged item's absence may be:
/// a buffered item contributes to no viewport, count or density until a flush gives it a row. A
/// visibility-latency control, not merely a segment-count one.
///
/// **90, not SA §7's 60, and the change came with the validation that makes it necessary.** The
/// floor the shipped `pin_ttl_secs` (300) and `drain_depth_max` (4) permit is 75 s
/// ([`ConfigError::PinDrainRelation`]); 60 violates it, so landing the relation without moving this
/// number would give a server that refuses to start on its own defaults. 90 sits above the floor
/// with margin.
///
/// An admin who wants lower visibility latency raises `drain_depth_max` rather than shortening
/// `pin_ttl_secs`: under §1.2's incremental generation construction the marginal cost of a drain
/// entry is roughly one flush segment rather than a whole bundle, which is what makes it the
/// cheaper knob. *Modelled, not measured* — the figure to take before leaning on it is resident
/// bytes per drain entry under sustained flush.
const DEFAULT_FLUSH_MAX_AGE_SECS: u64 = 90;

/// Buffer occupancy at which `/control/ingest` is refused (§1.3).
///
/// **`ingest_queue_bound` does not bound this.** That one bounds the *command queue* — 32 jobs by
/// default — and the executor drains a job into the buffer in milliseconds, so nothing in the
/// system compared buffer occupancy to anything and no ingest rate produced a 429 by buffer size.
/// Deferring `flush_max_items` to the tick needs such a bound to exist, because between ticks the
/// buffer is what grows.
///
/// Sized an order above `flush_max_items` (100 000): a flush-ready buffer must not be a refusing
/// one, or a deployment that trips the item bound between ticks sheds ingest it was about to
/// publish. What this bounds is the pathological case — repeated flush failure — where the buffer
/// grows without a flush to drain it, and 429 is the intended backpressure (§10).
const DEFAULT_INGEST_BUFFER_MAX_ITEMS: usize = 1_000_000;

/// Superseded generations retained on the pin drain list (lifecycle §2.2).
///
/// **Admin-configurable, reversing `tessera_engine::pins`' recorded choice** that it be a
/// compile-time constant. §1.4's cost model is what changes the answer: the constant was chosen
/// when a drain entry meant a whole distinct bundle, and under §1.2's incremental construction
/// consecutive generations share their base geometry by `Arc`, so a drain entry costs roughly one
/// flush segment. That makes this the cheaper of the two knobs an admin has for visibility
/// latency, and a knob is no use compiled in.
const DEFAULT_DRAIN_DEPTH_MAX: usize = 4;

/// Below this, segments compare equal for merge selection (§5.1), so a tail of tiny segments does
/// not dominate it.
const DEFAULT_SEGMENT_FLOOR_BYTES: u64 = 16 * 1024 * 1024;

/// Segments in a tier before a merge is selected (§5.1).
const DEFAULT_TIER_WIDTH: usize = 4;

/// The **measured** serialised size of one row projection at the 10⁹ operating point: every
/// mask at ≥25% coverage serialises to a 125.12 MB dense bound (design Appendix A quotes the
/// same 125 MB unsharded figure). Exposed rather than private because it is the operand of
/// [`crate::validate_cache_bounds`]'s startup validation, not a documentation flourish.
///
/// Three caveats belong wherever this number is used.
///
/// 1. It is the *serialised* size. `get_serialized_size_in_bytes` can underestimate the in-memory
///    footprint — but **not at this operating point**. The ~2× gap is an **array-container**
///    property:
///    a `Vec<u16>` of values carries capacity slack that the serialised form does not. A mask that
///    serialises to the 125.12 MB dense bound is by construction dominated by **bitmap**
///    containers, whose in-memory size *is* their serialised size (8 KB, a fixed 2¹⁶-bit block) —
///    ratio ≈ 1.0. So at the figure this constant describes, the factor does not apply; it applies
///    to sparse, array-container-dominated masks, which are small in absolute terms anyway.
/// 2. It is the 10⁹ figure, so a smaller corpus leaves the bounds below over-provisioned rather
///    than wrong.
/// 3. It is **per (session, slice, segments_version) entry**, not per session — the cache key's
///    three components (see `tessera_engine`'s `RowProjectionKey`). "Eight sessions, eight
///    entries" holds only while one partition emits one slice, which is true today and silently
///    false the moment a build emits two: the same eight sessions then occupy sixteen entries.
pub const MEASURED_PROJECTION_BYTES_AT_1E9: u64 = 125_120_000;

/// Byte bound on the row-projection cache.
///
/// **Sized so the cache cannot collapse at the expected concurrency, because plain LRU does not
/// degrade in this regime — it collapses.** A projection miss is
/// `RowProjection::new`, measured in *seconds* (the 10⁹ warm-up viewport is 10.7 s), so the
/// miss/hit cost ratio is 10⁵–10⁷; and because the single-flight miss path returns
/// `ProjectionBuilding` to every racer, a working set that does not fit presents as a permanent
/// 429 storm with a core set pegged on rebuilds, not as a gently lower hit rate.
///
/// 2 GiB is `2 ×` [`DEFAULT_EXPECTED_CONCURRENT_SESSIONS`] × [`MEASURED_PROJECTION_BYTES_AT_1E9`]
/// (8 × 125 MB ≈ 1 GiB working set).
///
/// **The factor of two is entry-count headroom, and specifically *not* a correction for the
/// serialised-size accounting.** That ~2× underestimate is an array-container property and does not
/// apply at the dense bound this cache is sized against, where the mask is bitmap-container
/// dominated and in-memory size equals serialised size — so anyone reaching for that justification
/// is reaching for the wrong one. What the margin buys is the two ways the entry count exceeds the
/// session count: **more than one slice per session** (the key is
/// `(token_id, slice, segments_version)`, so a two-slice bundle doubles the entries at unchanged
/// concurrency), and **a generation swap**, during which a pinned request's old-`segments_version`
/// entry coexists with the new one until `prune_generation` runs at drain-list reclaim. Both are
/// entry-count effects, and at this bound either one alone still fits.
const DEFAULT_ROW_PROJECTION_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Byte bound on the **in-memory** fragment tier. The digest-verified `.frag` sidecar
/// tier is untouched by it.
///
/// Deliberately half [`DEFAULT_ROW_PROJECTION_CACHE_BYTES`], and the asymmetry is the point: a
/// fragment miss costs a sidecar re-open plus SHA-256 over ~125 MB (~60–80 ms estimated), where
/// a projection miss costs a rebuild measured in seconds. Fragments are also keyed by canonical
/// grant set rather than by session, so principals with equal grants share one entry and the
/// working set grows with *policy* cardinality, not with concurrency.
///
/// **The margin its sibling carries is dropped here deliberately.** Both
/// bounds hold the same-shaped Roaring object at the same measured per-entry size, so `1 ×` here
/// against `2 ×` there is a real difference and needs its reason stated rather than inferred: this
/// cache's entry count is bounded by **distinct grant sets in flight**, not by sessions, so the
/// row-projection margin's two justifications (a slice multiplier per session, and a generation
/// swap's transient duplicate) do not apply — a slice does not appear in this key at all, and a
/// fragment outlives a bundle swap. Eight *entries* here is therefore eight distinct policies.
///
/// A deployment whose principals genuinely span more than eight distinct grant sets should raise
/// this, and [`crate::validate_cache_bounds`] covers this cache with the same relation it applies to
/// the projection cache, so the failure is a refusal to start rather than a thrash.
const DEFAULT_FRAGMENT_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// The session concurrency the projection cache must not collapse at — the startup
/// validation is that `row_projection_cache_bytes` admits at least this many entries at
/// [`MEASURED_PROJECTION_BYTES_AT_1E9`].
///
/// Eight is chosen as the smallest number that is honestly a *deployment* rather than a demo,
/// and it is a floor on the cache bound rather than a limit on sessions: exceeding it is not
/// refused anywhere, it simply means the cache starts evicting. An operator who runs more
/// concurrent sessions than this should raise **both** this and the cache bound together, which
/// is why the validation reads as a relation between them rather than as two independent
/// numbers.
const DEFAULT_EXPECTED_CONCURRENT_SESSIONS: usize = 8;

/// How long a session pin stays resolvable (lifecycle §2.2).
///
/// **A pin is expensive in a way its holder cannot see.** A drained generation is held alive by
/// its pin, and at the measured 47.02 GB bundle with a 22.5 GB viewport-hot set, one slow client
/// holding a pin across two compactions contests essentially all of a 47 GB box's page cache and
/// holds up to ~94 GB of *invisible* disk through deleted-but-mapped files (`df` ≠ `du`). The
/// failure is a warm-to-cold cliff — a measured 4.0–4.5 ns/visible-row warm against a modelled
/// 50–100 µs/page-miss — so it arrives as a step, not a slope.
///
/// Five minutes is sized to a human interaction with a frozen view (pan and zoom over one
/// geometry), not to a session: expiry is a `410` and re-pinning is one round trip, so the cost
/// of this being too short is a retry, while the cost of it being too long is the cliff above.
const DEFAULT_PIN_TTL_SECS: u64 = 300;

/// The most pins one session may hold at once (lifecycle §2.2).
///
/// Four is one pin per concurrently-open frozen view, with room to spare.
///
/// **This knob does NOT bound retention**, however much it looks as though it should —
/// `tessera_engine::pins`' module doc refutes that reading and asks that it not be reintroduced. A
/// drain entry holds its `Arc<Bundle>` from retirement until the TTL or the depth ceiling releases
/// it,
/// **whether or not any session ever presents a pin naming it** — the cap is consulted only when
/// one is presented. And a client that wants N superseded geometries simply opens N sessions:
/// `Engine::authorise` mints a fresh `token_id` per call against a cached fragment, so the
/// rotation costs it nothing. What actually bounds retention is [`DEFAULT_PIN_TTL_SECS`] above and
/// the engine's own `DRAIN_DEPTH_MAX`; see `tessera_engine::pins` for the page-cache argument
/// that sizes them.
///
/// What this knob *does* bound is one session's claim on the resolve path — enough to keep a
/// single client from pinning without limit, not enough to be a memory bound. Exceeding it is
/// refused rather than silently ignored, because a session that believes it holds a pin it does
/// not hold would compose against geometry it did not ask for.
const DEFAULT_PINS_PER_SESSION_MAX: usize = 4;

/// Refuses a zero for one of the write-path knobs, naming the silent failure zero would cause.
/// See [`ConfigError::MustBeNonZero`] for why these refuse rather than clamp.
fn non_zero_usize(key: &'static str, value: usize, consequence: &'static str) -> Result<usize> {
    if value == 0 {
        return Err(ConfigError::MustBeNonZero { key, consequence });
    }
    Ok(value)
}

/// [`non_zero_usize`] for the `u64`-typed knobs (byte bounds and durations).
fn non_zero_u64(key: &'static str, value: u64, consequence: &'static str) -> Result<u64> {
    if value == 0 {
        return Err(ConfigError::MustBeNonZero { key, consequence });
    }
    Ok(value)
}

pub fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)?;
    parse(&text)
}

fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;

    if raw.plugin.module != "builtin:passthrough" {
        return Err(ConfigError::UnsupportedPlugin(raw.plugin.module));
    }

    let disclosure_value = raw
        .disclosure
        .ok_or(ConfigError::MissingDisclosureSection)?;
    let table = disclosure_value
        .as_table()
        .ok_or(ConfigError::MissingDisclosureSection)?;
    let min_visible_members = table
        .get("min_visible_members")
        .and_then(toml::Value::as_integer)
        .ok_or(ConfigError::MissingDisclosureKey("min_visible_members"))?
        as u64;
    let token_max_lifetime_secs = table
        .get("token_max_lifetime")
        .and_then(toml::Value::as_integer)
        .ok_or(ConfigError::MissingDisclosureKey("token_max_lifetime"))?
        as u64;

    let viewer_addr: SocketAddr = raw
        .serve
        .viewer
        .parse()
        .map_err(|_| ConfigError::BadAddr(raw.serve.viewer.clone()))?;
    let session_addr: SocketAddr = raw
        .serve
        .session
        .parse()
        .map_err(|_| ConfigError::BadAddr(raw.serve.session.clone()))?;
    let control_listen = parse_control_listen(&raw.serve.control)?;

    let session_credential = load_credential(
        "session",
        raw.serve.session_credential_file.as_deref(),
        raw.serve.session_credential_env.as_deref(),
    )?;
    let operator_credential = load_credential(
        "operator",
        raw.serve.operator_credential_file.as_deref(),
        raw.serve.operator_credential_env.as_deref(),
    )?;

    // §7.2's clause parameters. Every check here refuses rather than clamps: a typo must not
    // silently disable an invariant (the floor) or silently blank the density signal (theta).
    let k_min = raw.serve.k_min.unwrap_or(DEFAULT_K_MIN);
    let k_max_marks = raw.serve.k_max_marks.unwrap_or(DEFAULT_K_MAX_MARKS);
    let max_k = raw.serve.max_k.unwrap_or(DEFAULT_MAX_K);
    let theta_target_marks = raw
        .serve
        .theta_target_marks
        .unwrap_or(DEFAULT_THETA_TARGET_MARKS);
    if k_min == 0 {
        return Err(ConfigError::FloorClauseDisabled);
    }
    // BOTH caps clamp the floor, because the effective cap is `min(k, max_k, k_max_marks)` and the
    // floor is `min(k_min, cap)`. Checking only the overplot ceiling was a gap.
    if k_min > k_max_marks {
        return Err(ConfigError::FloorAboveCap {
            k_min,
            cap_name: "k_max_marks",
            cap: k_max_marks,
        });
    }
    if k_min > max_k {
        return Err(ConfigError::FloorAboveCap {
            k_min,
            cap_name: "max_k",
            cap: max_k,
        });
    }
    if theta_target_marks == 0 {
        return Err(ConfigError::ThetaTargetZero);
    }
    let max_underlay_offset = raw
        .serve
        .max_underlay_offset
        .unwrap_or(DEFAULT_MAX_UNDERLAY_OFFSET);
    if max_underlay_offset > 16 {
        return Err(ConfigError::UnderlayOffsetTooDeep(max_underlay_offset));
    }

    // The admission knobs. Same refuse-not-clamp discipline as the selection clause above: each of
    // `compute_threads`/`compute_admission`/`admission_timeout_ms` at 0 has a distinct
    // silent-failure mode (an empty pool, a gate that sheds everything, a queue wait that never
    // actually waits) and a typo must not quietly produce any of them. `compute_queue = 0` is legal
    // — it means "shed the instant every compute permit is busy" — so it alone is never checked.
    let compute_threads = raw
        .serve
        .compute_threads
        .unwrap_or_else(default_compute_threads);
    if compute_threads == 0 {
        return Err(ConfigError::ComputeThreadsZero);
    }
    // Parsed here rather than beside its `[ingest]` siblings below, because it is an operand of the
    // clamp `compute_admission`'s default takes immediately after.
    let ingest_admission = non_zero_usize(
        "ingest.ingest_admission",
        raw.ingest
            .ingest_admission
            .unwrap_or(DEFAULT_INGEST_ADMISSION),
        "zero concurrent ingest handlers admits no ingest at all — every batch is shed with 429 \
         before it is even decoded, which is indistinguishable from the write executor being down",
    )?;
    let compute_admission = match raw.serve.compute_admission {
        Some(v) => v,
        // `checked_mul`, not `*`: release builds have overflow checks off, so an unchecked
        // multiply here would silently wrap to a small, wrong permit count for an
        // operator-supplied `compute_threads` extreme enough to overflow — refused instead, the
        // same discipline the `compute_admission + compute_queue` checked-add below already
        // applies one step downstream.
        //
        // **Clamped, not refused, and only on this arm.** The default is `4 × available
        // parallelism`, so on a box past ~1000 cores `4n + ingest_admission + reserve` exceeded
        // [`SERVING_BLOCKING_THREAD_CEILING`] and a *default* `tessera.toml` refused to start on a
        // machine where it previously started fine. A default configuration that cannot start is a
        // defect however rare the box, and the ceiling's own subject is the blocking pool, not the
        // gate: clamping gives that machine the largest admission the pool can carry. An **explicit**
        // `serve.compute_admission` is still refused by relation 2 below — an operator who names a
        // number gets told it does not fit, rather than silently getting a different one.
        None => compute_threads
            .checked_mul(COMPUTE_ADMISSION_MULTIPLIER)
            .ok_or(ConfigError::ComputeAdmissionDefaultOverflow { compute_threads })?
            .min(
                SERVING_BLOCKING_THREAD_CEILING
                    .saturating_sub(ingest_admission)
                    .saturating_sub(BLOCKING_THREAD_RESERVE)
                    // `max(1)`: an `ingest_admission` large enough to fill the ceiling on its own
                    // must not clamp the gate to zero permits (which sheds every viewer request).
                    // Relation 2 refuses that configuration a few lines below; this only keeps the
                    // clamp from manufacturing a second, worse failure on the way there.
                    .max(1),
            ),
    };
    if compute_admission == 0 {
        return Err(ConfigError::ComputeAdmissionZero);
    }
    let compute_queue = raw.serve.compute_queue.unwrap_or(2 * compute_admission);
    // The outer slots semaphore is sized `compute_admission + compute_queue` (`AppState::new`);
    // `Semaphore::new` panics past `MAX_PERMITS`, and a naive `+` panics on overflow first at
    // absurd (but syntactically valid) configured values. Refuse here instead, at parse, so the
    // failure is a typed config error rather than a startup panic.
    match compute_admission.checked_add(compute_queue) {
        Some(total) if total <= tokio::sync::Semaphore::MAX_PERMITS => {}
        _ => {
            return Err(ConfigError::ComputeAdmissionQueueOverflow {
                compute_admission,
                compute_queue,
            })
        }
    }
    let admission_timeout_ms = raw
        .serve
        .admission_timeout_ms
        .unwrap_or(DEFAULT_ADMISSION_TIMEOUT_MS);
    if admission_timeout_ms == 0 {
        return Err(ConfigError::AdmissionTimeoutZero);
    }

    // The write-path knobs. All of them default (SA §7: performance knobs default, disclosure
    // controls do not — none of these is a disclosure control); all of them refuse a zero, each with
    // its own silent failure named. Two of them (`flush_*`) are read by nothing.
    let commit_window_max_items = non_zero_usize(
        "ingest.commit_window_max_items",
        raw.ingest
            .commit_window_max_items
            .unwrap_or(DEFAULT_COMMIT_WINDOW_MAX_ITEMS),
        "a window that closes at zero rows is not group commit switched off, it is group \
         commit silently doing nothing — set it to 1 to disable batching honestly",
    )?;
    let commit_window_max_age_ms = non_zero_u64(
        "ingest.commit_window_max_age_ms",
        raw.ingest
            .commit_window_max_age_ms
            .unwrap_or(DEFAULT_COMMIT_WINDOW_MAX_AGE_MS),
        // The key is INERT — no window is ever aged out — so this refusal guards a future consumer
        // rather than a live mechanism, and says so rather than describing a behaviour the build
        // does not have.
        "this key is inert (nothing reads it; see DEFAULT_COMMIT_WINDOW_MAX_AGE_MS), and zero is \
         still refused so that no configuration reaches a future consumer already meaning \
         \"close before anything can join\" — set ingest.commit_window_max_items = 1 to disable \
         batching honestly",
    )?;
    let ingest_queue_bound = non_zero_usize(
        "ingest.ingest_queue_bound",
        raw.ingest
            .ingest_queue_bound
            .unwrap_or(DEFAULT_INGEST_QUEUE_BOUND),
        // The plausible reading — that a zero-depth queue makes submissions block until the
        // executor picks them up — is the opposite of what happens. `LifecycleHandle::submit` uses
        // `try_send` and `Executor::run` takes work with `try_recv`, never blocking in
        // `work.recv()`, so a rendezvous channel has no waiting receiver ever and EVERY ingest is
        // refused. That is the worse failure, and it is what the message tells an operator.
        "a zero-depth work queue is a rendezvous with nobody waiting at it: the executor takes \
         work with try_recv, so a zero bound refuses EVERY ingest with 429 while denies continue \
         normally — indistinguishable from ingest being switched off, but silently",
    )?;
    // `ingest_admission` is parsed **above**, before `compute_admission`, because it is an operand
    // of that key's defaulted-and-clamped value. Not repeated here.
    let ingest_max_batch_rows = non_zero_usize(
        "ingest.ingest_max_batch_rows",
        raw.ingest
            .ingest_max_batch_rows
            .unwrap_or(DEFAULT_INGEST_MAX_BATCH_ROWS),
        "every ingest batch would be refused with 422, closing the ingest plane while every \
         health surface still reports the server up",
    )?;
    let ingest_max_batch_bytes = non_zero_usize(
        "ingest.ingest_max_batch_bytes",
        raw.ingest
            .ingest_max_batch_bytes
            .unwrap_or(DEFAULT_INGEST_MAX_BATCH_BYTES),
        "every ingest batch would be refused with 422, and the queue's headroom arithmetic would \
         conclude that an arbitrarily deep queue fits in any WAL",
    )?;
    // The per-connection ceiling, checked here beside the key rather than with the three relations
    // below, because it is not a relation: it is a bound on this one value, and it is the only
    // check in this file that concerns a request the admission gate has not seen yet. See
    // [`INGEST_MAX_BATCH_BYTES_CEILING`] for what it closes and what it explicitly does not.
    if ingest_max_batch_bytes > INGEST_MAX_BATCH_BYTES_CEILING {
        return Err(ConfigError::IngestBatchBytesCeiling {
            ingest_max_batch_bytes,
        });
    }
    let wal_hard_limit_bytes = non_zero_u64(
        "ingest.wal_hard_limit_bytes",
        raw.ingest
            .wal_hard_limit_bytes
            .unwrap_or(DEFAULT_WAL_HARD_LIMIT_BYTES),
        "a zero WAL ceiling cannot be satisfied by any queue depth, so the startup headroom \
         assertion could only ever refuse to start",
    )?;
    let overlay_soft_limit = non_zero_usize(
        "ingest.overlay_soft_limit",
        raw.ingest
            .overlay_soft_limit
            .unwrap_or(DEFAULT_OVERLAY_SOFT_LIMIT),
        "the alarm would fire on the very first deny and never stop; a permanently-firing alarm \
         is indistinguishable from no alarm at all",
    )?;
    let flush_max_items = non_zero_usize(
        "ingest.flush_max_items",
        raw.ingest
            .flush_max_items
            .unwrap_or(DEFAULT_FLUSH_MAX_ITEMS),
        "a zero item trigger asks flush to run before there is anything to flush (INERT — nothing \
         reads this key; validated so that whatever builds flush inherits the guard)",
    )?;
    let flush_max_age_secs = non_zero_u64(
        "ingest.flush_max_age_secs",
        raw.ingest
            .flush_max_age_secs
            .unwrap_or(DEFAULT_FLUSH_MAX_AGE_SECS),
        "a zero tick asks the executor to publish geometry continuously, which is a publication \
         per loop iteration and a drain entry per publication",
    )?;
    let ingest_buffer_max_items = non_zero_usize(
        "ingest.ingest_buffer_max_items",
        raw.ingest
            .ingest_buffer_max_items
            .unwrap_or(DEFAULT_INGEST_BUFFER_MAX_ITEMS),
        "a zero buffer bound refuses EVERY ingest with 429 while denies continue normally — \
         indistinguishable from ingest being switched off, but silently",
    )?;
    let drain_depth_max = non_zero_usize(
        "serve.drain_depth_max",
        raw.serve.drain_depth_max.unwrap_or(DEFAULT_DRAIN_DEPTH_MAX),
        "a zero drain depth retires every superseded geometry immediately, so a pin taken one \
         instant before a flush is 410 the next — I11's frozen view would last less than a tick",
    )?;
    let segment_floor_bytes = raw
        .serve
        .segment_floor_bytes
        .unwrap_or(DEFAULT_SEGMENT_FLOOR_BYTES);
    let max_merged_segment_bytes = match raw.serve.max_merged_segment_bytes {
        Some(bytes) => Some(non_zero_u64(
            "serve.max_merged_segment_bytes",
            bytes,
            "a zero merge cap admits no merge at all, so the segment and delta-tier counts grow \
             without limit under flush — §0's serving cliff on both axes",
        )?),
        None => None,
    };
    let tier_width = non_zero_usize(
        "serve.tier_width",
        raw.serve.tier_width.unwrap_or(DEFAULT_TIER_WIDTH),
        "a zero tier width selects a merge over no segments at all",
    )?;
    let row_projection_cache_bytes = non_zero_u64(
        "serve.row_projection_cache_bytes",
        raw.serve
            .row_projection_cache_bytes
            .unwrap_or(DEFAULT_ROW_PROJECTION_CACHE_BYTES),
        "a cache that admits nothing is a 100% miss rate, and because the single-flight miss path \
         returns ProjectionBuilding to every racer that presents as a permanent 429 storm with \
         cores pegged on rebuilds, not as a slower server",
    )?;
    let fragment_cache_bytes = non_zero_u64(
        "serve.fragment_cache_bytes",
        raw.serve
            .fragment_cache_bytes
            .unwrap_or(DEFAULT_FRAGMENT_CACHE_BYTES),
        "the in-memory fragment tier would admit nothing, so every authorisation would re-open \
         and re-verify its sidecar",
    )?;
    let expected_concurrent_sessions = non_zero_usize(
        "serve.expected_concurrent_sessions",
        raw.serve
            .expected_concurrent_sessions
            .unwrap_or(DEFAULT_EXPECTED_CONCURRENT_SESSIONS),
        "the cache-admission validation would be vacuous — any cache bound, however small, \
         'admits' zero sessions",
    )?;
    let pin_ttl_secs = non_zero_u64(
        "serve.pin_ttl_secs",
        raw.serve.pin_ttl_secs.unwrap_or(DEFAULT_PIN_TTL_SECS),
        "every pin would expire the instant it was issued, so a client presenting a pin it was \
         just given gets 410 with nothing to distinguish that from a drained generation",
    )?;
    // **§4's relation 1**, checked here because all three operands are now known and because a
    // violating configuration must be refused rather than clamped: clamping would silently give an
    // operator a different pin lifetime, or a different visibility latency, than the one they
    // configured. The flush tick is the periodic publisher `tessera_engine::pins`' sizing
    // obligation names, and this is that obligation discharged.
    if pin_ttl_secs >= drain_depth_max as u64 * flush_max_age_secs {
        return Err(ConfigError::PinDrainRelation {
            pin_ttl_secs,
            drain_depth_max,
            flush_max_age_secs,
        });
    }
    let pins_per_session_max = non_zero_usize(
        "serve.pins_per_session_max",
        raw.serve
            .pins_per_session_max
            .unwrap_or(DEFAULT_PINS_PER_SESSION_MAX),
        "no session could pin at all, silently disabling I11's pinned-geometry guarantee rather \
         than bounding it",
    )?;

    // ---------------------------------------------------------------------------------------
    // The three startup relations. Each refuses naming both of its sides.
    //
    // They live here rather than in `prepare` for the reason the `compute_admission + \
    // compute_queue` check above does: cross-key validation is this function's job, and a refusal
    // expressible at `parse()` is testable without a bundle, a WAL or a listener.
    // ---------------------------------------------------------------------------------------

    // Relation 1 — WAL bytes (lifecycle §4's headroom rule). `checked_*` throughout, for
    // `validate_cache_bounds`' reason verbatim: a wrap produces a *small* left-hand side, i.e. it
    // silently admits exactly the collapsing configuration this refuses.
    let queue_worst_case = (ingest_queue_bound as u64)
        .checked_mul(ingest_max_batch_bytes as u64)
        .ok_or(ConfigError::WalHeadroomOverflow {
            ingest_queue_bound,
            ingest_max_batch_bytes,
        })?;
    let required_wal = queue_worst_case
        .checked_add(RESERVED_DENY_HEADROOM_BYTES)
        .ok_or(ConfigError::WalHeadroomOverflow {
            ingest_queue_bound,
            ingest_max_batch_bytes,
        })?;
    // **Strictly** below: at equality a full ingest queue plus the reserve exactly consumes the
    // ceiling, leaving the next deny nothing.
    if required_wal >= wal_hard_limit_bytes {
        return Err(ConfigError::WalHeadroom {
            queue_worst_case,
            reserved_deny_headroom: RESERVED_DENY_HEADROOM_BYTES,
            wal_hard_limit_bytes,
            ingest_queue_bound,
            ingest_max_batch_bytes,
        });
    }

    // Relation 2 — blocking threads. The pool is *derived* from its two admission-bounded
    // consumers (see `serving_blocking_threads`), so what is refused here is a pool the machine
    // cannot carry, not an imbalance between the knobs.
    let required_threads = compute_admission
        .saturating_add(ingest_admission)
        .saturating_add(BLOCKING_THREAD_RESERVE);
    if required_threads > SERVING_BLOCKING_THREAD_CEILING {
        return Err(ConfigError::BlockingThreadCeiling {
            compute_admission,
            ingest_admission,
            required: required_threads,
        });
    }

    // Relation 3 — resident bytes. Relations 1 and 2 bound WAL bytes and OS threads; heap is the
    // resource that actually binds at 10⁹, and neither of them weighs it. **Both** admission and
    // the queue bound multiply the byte cap, because a decoded batch is resident from the Arrow
    // decode until the receipt returns, and only part of that window holds a queue slot.
    // `checked_*` for `WalHeadroomOverflow`'s reason verbatim: a wrap produces a *small* left-hand
    // side. See [`INGEST_RESIDENT_CEILING_BYTES`] for what this does and does not claim.
    let resident_worst_case = (ingest_queue_bound as u64)
        .checked_add(ingest_admission as u64)
        .and_then(|batches| batches.checked_mul(ingest_max_batch_bytes as u64))
        .ok_or(ConfigError::IngestResidentOverflow {
            ingest_queue_bound,
            ingest_admission,
            ingest_max_batch_bytes,
        })?;
    if resident_worst_case > INGEST_RESIDENT_CEILING_BYTES {
        return Err(ConfigError::IngestResidentCeiling {
            ingest_queue_bound,
            ingest_admission,
            ingest_max_batch_bytes,
            required: resident_worst_case,
        });
    }

    Ok(Config {
        ingest_buffer_max_items,
        drain_depth_max,
        segment_floor_bytes,
        max_merged_segment_bytes,
        tier_width,
        bundle_path: raw.bundle.path,
        cache_dir: raw.bundle.cache,
        wal_path: raw.bundle.wal,
        min_visible_members,
        token_max_lifetime_secs,
        viewer_addr,
        session_addr,
        control_listen,
        max_k,
        k_min,
        k_max_marks,
        theta_target_marks,
        max_underlay_offset,
        max_underlay_cells: raw
            .serve
            .max_underlay_cells
            .unwrap_or(DEFAULT_MAX_UNDERLAY_CELLS),
        max_tiles_per_request: raw
            .serve
            .max_tiles_per_request
            .unwrap_or(DEFAULT_MAX_TILES_PER_REQUEST),
        stage_timing: raw.serve.stage_timing.unwrap_or(false),
        dev_cors_origins: raw.serve.dev_cors_origins.unwrap_or_default(),
        session_credential,
        operator_credential,
        compute_threads,
        compute_admission,
        compute_queue,
        admission_timeout_ms,
        commit_window_max_items,
        commit_window_max_age_ms,
        ingest_queue_bound,
        ingest_admission,
        ingest_max_batch_rows,
        ingest_max_batch_bytes,
        wal_hard_limit_bytes,
        overlay_soft_limit,
        flush_max_items,
        flush_max_age_secs,
        row_projection_cache_bytes,
        fragment_cache_bytes,
        expected_concurrent_sessions,
        pin_ttl_secs,
        pins_per_session_max,
    })
}

fn parse_control_listen(raw: &str) -> Result<ControlListen> {
    if let Some(path) = raw.strip_prefix("unix:") {
        return Ok(ControlListen::Unix(PathBuf::from(path)));
    }
    raw.parse()
        .map(ControlListen::Tcp)
        .map_err(|_| ConfigError::BadAddr(raw.to_string()))
}

fn load_credential(name: &'static str, file: Option<&Path>, env: Option<&str>) -> Result<String> {
    if let Some(path) = file {
        return Ok(fs::read_to_string(path)?.trim().to_string());
    }
    if let Some(var) = env {
        return std::env::var(var).map_err(|_| ConfigError::MissingCredential(name));
    }
    Err(ConfigError::MissingCredential(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_disclosure_section_refuses_to_start() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_SESSION_CRED"
            operator_credential_env = "TESSERA_TEST_OPERATOR_CRED"
        "#;
        let err = parse(toml).unwrap_err();
        assert!(matches!(err, ConfigError::MissingDisclosureSection));
    }

    #[test]
    fn missing_disclosure_key_refuses_to_start() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            min_visible_members = 10
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_SESSION_CRED"
            operator_credential_env = "TESSERA_TEST_OPERATOR_CRED"
        "#;
        let err = parse(toml).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::MissingDisclosureKey("token_max_lifetime")
        ));
    }

    /// A complete, valid config, with `[serve]` extras interpolated — used by the selection-clause
    /// tests below so each one differs from a working config in exactly one key.
    fn valid_toml(serve_extra: &str) -> String {
        valid_toml_with(serve_extra, "")
    }

    /// [`valid_toml`] with an `[ingest]` section as well. Note the section is emitted **only**
    /// when non-empty, so `valid_toml("")` is genuinely a config with no `[ingest]` section at
    /// all — which is what `every_stage_2_1_knob_defaults` needs to be testing.
    fn valid_toml_with(serve_extra: &str, ingest_extra: &str) -> String {
        let ingest_section = if ingest_extra.is_empty() {
            String::new()
        } else {
            format!("[ingest]\n{ingest_extra}\n")
        };
        format!(
            r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            min_visible_members = 10
            token_max_lifetime = 3600
            {ingest_section}
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_SESSION_CRED"
            operator_credential_env = "TESSERA_TEST_OPERATOR_CRED"
            {serve_extra}
        "#
        )
    }

    /// **§4's relation 1.** A violating configuration is refused, and the message names all three
    /// knobs, because an operator told only "the relation fails" cannot tell which one to move.
    ///
    /// The fixture is what SA §7 shipped: `pin_ttl_secs` 300 against a 60 s tick, which is a drain
    /// list permanently at its ceiling — the depth alarm saturated and pins dropped by the trim
    /// rather than by their TTL.
    #[test]
    fn a_pin_ttl_the_publication_cadence_cannot_cover_is_refused() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml_with(
            "pin_ttl_secs = 300",
            "flush_max_age_secs = 60",
        ))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::PinDrainRelation { .. }),
            "got {err:?}"
        );
        let text = err.to_string();
        for knob in ["pin_ttl_secs", "drain_depth_max", "flush_max_age_secs"] {
            assert!(text.contains(knob), "the message must name {knob}: {text}");
        }
    }

    /// **The shipped defaults satisfy it**, which is not automatic: SA §7's 60 s tick against the
    /// 300 s pin TTL and a depth of 4 violates the relation, so landing the validation without
    /// moving `DEFAULT_FLUSH_MAX_AGE_SECS` would have given a server that refuses to start on its
    /// own configuration. The two changed together.
    #[test]
    fn the_shipped_defaults_satisfy_relation_one() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("the defaults must load");
        assert!(
            config.pin_ttl_secs < config.drain_depth_max as u64 * config.flush_max_age_secs,
            "pin_ttl_secs {} >= {} x {}",
            config.pin_ttl_secs,
            config.drain_depth_max,
            config.flush_max_age_secs
        );
    }

    /// Raising `drain_depth_max` is the way to a shorter tick, and §1.4 is why: under incremental
    /// generation construction a drain entry costs roughly one flush segment rather than a whole
    /// bundle, so it is the cheaper knob than shortening `pin_ttl_secs`. This is that path being
    /// open rather than merely described.
    #[test]
    fn a_deeper_drain_list_admits_a_shorter_tick() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        assert!(parse(&valid_toml_with(
            "pin_ttl_secs = 300\n            drain_depth_max = 16",
            "flush_max_age_secs = 30",
        ))
        .is_ok());
    }

    /// The new knobs default rather than requiring an operator to name them, and
    /// `max_merged_segment_bytes` defaults to **unset** — there is no fixed value that can satisfy
    /// §4's relation 2 across deployment sizes.
    #[test]
    fn the_flush_and_merge_knobs_have_working_defaults() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("defaults must load");
        assert_eq!(config.flush_max_age_secs, DEFAULT_FLUSH_MAX_AGE_SECS);
        assert_eq!(config.flush_max_items, DEFAULT_FLUSH_MAX_ITEMS);
        assert_eq!(
            config.ingest_buffer_max_items,
            DEFAULT_INGEST_BUFFER_MAX_ITEMS
        );
        assert_eq!(config.drain_depth_max, DEFAULT_DRAIN_DEPTH_MAX);
        assert_eq!(config.segment_floor_bytes, DEFAULT_SEGMENT_FLOOR_BYTES);
        assert_eq!(config.tier_width, DEFAULT_TIER_WIDTH);
        assert_eq!(
            config.max_merged_segment_bytes, None,
            "no fixed default can satisfy relation 2 — it is derived from the base segment"
        );
    }

    #[test]
    fn the_selection_clauses_have_working_defaults() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("defaults must load");
        assert_eq!(config.k_min, DEFAULT_K_MIN);
        assert_eq!(config.k_max_marks, DEFAULT_K_MAX_MARKS);
        assert_eq!(config.theta_target_marks, DEFAULT_THETA_TARGET_MARKS);
        assert_eq!(config.max_underlay_offset, DEFAULT_MAX_UNDERLAY_OFFSET);
        assert_eq!(config.max_underlay_cells, DEFAULT_MAX_UNDERLAY_CELLS);
        assert_eq!(config.max_k, DEFAULT_MAX_K);
        assert_eq!(config.compute_threads, default_compute_threads());
        assert_eq!(
            config.compute_admission,
            COMPUTE_ADMISSION_MULTIPLIER * config.compute_threads
        );
        assert_eq!(config.compute_queue, 2 * config.compute_admission);
        assert_eq!(config.admission_timeout_ms, DEFAULT_ADMISSION_TIMEOUT_MS);
    }

    /// `compute_admission` defaults to `COMPUTE_ADMISSION_MULTIPLIER × compute_threads`, not
    /// to a separate constant — an explicit `compute_threads` must change the default admission
    /// bound too, or the measured small-request throughput argument silently stops holding.
    #[test]
    fn compute_admission_defaults_to_compute_threads() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("compute_threads = 7")).expect("must load");
        assert_eq!(config.compute_threads, 7);
        assert_eq!(config.compute_admission, 28);
        assert_eq!(config.compute_queue, 56);
    }

    /// `compute_queue = 0` is explicitly legal — it means "shed the instant every compute
    /// permit is busy" — so it must load, not refuse.
    #[test]
    fn a_zero_compute_queue_is_legal() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("compute_queue = 0")).expect("compute_queue = 0 must load");
        assert_eq!(config.compute_queue, 0);
    }

    /// `compute_threads = 0` refuses to start rather than silently running a zero-width
    /// pool — the same refuse-not-clamp discipline as the selection clause's `k_min = 0`.
    #[test]
    fn a_zero_compute_threads_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("compute_threads = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ComputeThreadsZero), "{err}");
    }

    /// an explicit `compute_threads` large enough that `COMPUTE_ADMISSION_MULTIPLIER *
    /// compute_threads` overflows `usize` refuses to start rather than silently wrapping to a
    /// small, wrong `compute_admission` — release builds have overflow checks off, so an
    /// unchecked multiply would produce a bogus-but-plausible value with no error raised anywhere.
    /// `compute_threads` here is `i64::MAX` (fits TOML's integer range) so `4 * compute_threads`
    /// overflows `u64`/`usize` without overflowing on the way in from TOML itself.
    #[test]
    fn a_compute_threads_that_overflows_the_default_admission_multiply_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("compute_threads = 9223372036854775807")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::ComputeAdmissionDefaultOverflow {
                    compute_threads: 9223372036854775807
                }
            ),
            "{err}"
        );
    }

    /// `compute_admission = 0` refuses to start — a zero-permit compute semaphore sheds
    /// every gated request unconditionally, indistinguishable from the server being down.
    #[test]
    fn a_zero_compute_admission_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("compute_admission = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ComputeAdmissionZero), "{err}");
    }

    /// `admission_timeout_ms = 0` refuses to start — that would silently disable the bounded
    /// queue wait; `compute_queue = 0` is the correct, explicit way to disable queueing.
    #[test]
    fn a_zero_admission_timeout_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("admission_timeout_ms = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::AdmissionTimeoutZero), "{err}");
    }

    /// `compute_admission + compute_queue` at an absurd (but syntactically valid) size
    /// refuses to start rather than panicking inside `Semaphore::new` during server startup —
    /// these two knobs together size the outer slots semaphore, and `Semaphore::new` panics past
    /// `MAX_PERMITS`. Both values here fit comfortably in TOML's i64 range but their sum exceeds
    /// `tokio::sync::Semaphore::MAX_PERMITS` (`usize::MAX >> 3`), without overflowing `usize`
    /// itself — the "exceeds the bound" branch, distinct from the defensive `checked_add`
    /// overflow branch that TOML's i64 ceiling makes unreachable from config alone.
    #[test]
    fn an_absurd_admission_plus_queue_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml(
            "compute_admission = 2000000000000000000\ncompute_queue = 2000000000000000000",
        ))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::ComputeAdmissionQueueOverflow { .. }),
            "{err}"
        );
    }

    /// `k_min = 0` disables §7.2's floor clause, which is the I7 guarantee. Startup must refuse
    /// rather than clamp — a clamp would mean a typo silently changed the configuration, and a
    /// pass-through would mean a typo silently disabled an invariant.
    #[test]
    fn a_zero_floor_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("k_min = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::FloorClauseDisabled), "{err}");
    }

    #[test]
    fn a_floor_above_either_cap_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        // Above the overplot ceiling.
        let err = parse(&valid_toml("k_min = 200\nk_max_marks = 128")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::FloorAboveCap {
                    k_min: 200,
                    cap_name: "k_max_marks",
                    cap: 128
                }
            ),
            "{err}"
        );

        // Above the MACHINE ceiling, which also clamps the floor — the gap the first check missed.
        let err = parse(&valid_toml("max_k = 1\nk_min = 2")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::FloorAboveCap {
                    k_min: 2,
                    cap_name: "max_k",
                    cap: 1
                }
            ),
            "{err}"
        );
    }

    /// `theta_target_marks = 0` anchors θ at a cut admitting nothing, so every tile would draw
    /// exactly `k_min` at every zoom with no error — the identical silent failure that
    /// `Threshold::at_depth`'s `leading_zeros` guard prevents, reached through config instead.
    #[test]
    fn an_underlay_offset_deeper_than_the_grid_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("max_underlay_offset = 17")).unwrap_err();
        assert!(
            matches!(err, ConfigError::UnderlayOffsetTooDeep(17)),
            "{err}"
        );
    }

    #[test]
    fn a_zero_theta_target_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("theta_target_marks = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ThetaTargetZero), "{err}");
    }

    /// The dev-only browser seam is off unless typed.
    ///
    /// This is the one knob in this file that is **not** a performance knob, so SA §7's "knobs
    /// default, disclosure controls do not" would ordinarily require it to be stated. It defaults
    /// anyway — to *empty*, which is the fail-closed value: absent means the layer is never
    /// mounted. Making it required would force every existing `tessera.toml` to name a
    /// development-only key.
    #[test]
    fn dev_cors_origins_defaults_to_empty() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("a config naming no CORS origins must load");
        assert!(
            config.dev_cors_origins.is_empty(),
            "absent serve.dev_cors_origins must mean no CORS at all — a non-empty default would \
             let a dev affordance ride into a deployment"
        );
    }

    #[test]
    fn dev_cors_origins_round_trips_when_named() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let toml = valid_toml("dev_cors_origins = [\"http://localhost:5173\"]");
        let config = parse(&toml).expect("a config naming a CORS origin must load");
        assert_eq!(config.dev_cors_origins, vec!["http://localhost:5173"]);
    }

    // ---- The write-path and admission knobs --------------------------------------------------

    /// **SA §7's rule, pinned as a test.** "Performance knobs default; disclosure controls do
    /// not" — and not one of the write-path knobs is a disclosure control: they size queues,
    /// windows, caches and pin lifetimes, and none of them changes what any principal may see. So a
    /// `tessera.toml` that mentions **none** of them — no `[ingest]` section at all — must load,
    /// with the documented defaults.
    ///
    /// The reading matters because the opposite reading is also plausible and is wrong: several of
    /// these knobs *bound* memory, and classing "bounds memory" as "must be stated explicitly"
    /// would make every deployment carry fifteen lines of boilerplate that SA §7 exists to prevent.
    /// Anything that decides one of these really is a disclosure control must fail this test on the
    /// way to moving it — which is the point.
    #[test]
    fn every_stage_2_1_knob_defaults() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let toml = valid_toml("");
        assert!(
            !toml.contains("[ingest]"),
            "this test is only meaningful against a config with no [ingest] section"
        );
        let config = parse(&toml).expect("a config naming none of the write-path knobs must load");

        assert_eq!(
            config.commit_window_max_items,
            DEFAULT_COMMIT_WINDOW_MAX_ITEMS
        );
        assert_eq!(
            config.commit_window_max_age_ms,
            DEFAULT_COMMIT_WINDOW_MAX_AGE_MS
        );
        assert_eq!(config.ingest_queue_bound, DEFAULT_INGEST_QUEUE_BOUND);
        assert_eq!(config.ingest_admission, DEFAULT_INGEST_ADMISSION);
        assert_eq!(config.ingest_max_batch_rows, DEFAULT_INGEST_MAX_BATCH_ROWS);
        assert_eq!(
            config.ingest_max_batch_bytes,
            DEFAULT_INGEST_MAX_BATCH_BYTES
        );
        assert_eq!(config.wal_hard_limit_bytes, DEFAULT_WAL_HARD_LIMIT_BYTES);
        assert_eq!(config.overlay_soft_limit, DEFAULT_OVERLAY_SOFT_LIMIT);
        assert_eq!(config.flush_max_items, DEFAULT_FLUSH_MAX_ITEMS);
        assert_eq!(config.flush_max_age_secs, DEFAULT_FLUSH_MAX_AGE_SECS);
        assert_eq!(
            config.row_projection_cache_bytes,
            DEFAULT_ROW_PROJECTION_CACHE_BYTES
        );
        assert_eq!(config.fragment_cache_bytes, DEFAULT_FRAGMENT_CACHE_BYTES);
        assert_eq!(
            config.expected_concurrent_sessions,
            DEFAULT_EXPECTED_CONCURRENT_SESSIONS
        );
        assert_eq!(config.pin_ttl_secs, DEFAULT_PIN_TTL_SECS);
        assert_eq!(config.pins_per_session_max, DEFAULT_PINS_PER_SESSION_MAX);
    }

    /// And the keys are actually wired to their fields — a defaults test alone would pass just as
    /// happily against fifteen constants nothing parses. One key per section, plus the byte- and
    /// duration-typed shapes, so a mis-sectioned or mis-typed key is caught here.
    #[test]
    fn the_stage_2_1_knobs_are_read_from_their_sections() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml_with(
            "pin_ttl_secs = 42\nrow_projection_cache_bytes = 777000000",
            // 3 GiB + a bit. This key is an operand of the WAL headroom relation, so a value
            // chosen only for legibility would make the whole config refuse to start; it is kept
            // above `queue worst case + reserved deny headroom` at the defaults.
            "commit_window_max_items = 7\nwal_hard_limit_bytes = 3000000000",
        ))
        .expect("must load");
        assert_eq!(config.pin_ttl_secs, 42);
        assert_eq!(config.row_projection_cache_bytes, 777_000_000);
        assert_eq!(config.commit_window_max_items, 7);
        assert_eq!(config.wal_hard_limit_bytes, 3_000_000_000);
    }

    /// Every write-path knob refuses a zero, and refuses it by *name*. Zero is degenerate for all
    /// fifteen — never "off" — and the failure modes are silent ones: a window that batches
    /// nothing, a queue that blocks instead of shedding, an alarm that never stops firing, a
    /// cache that turns every request into a 429. Same discipline as `k_min = 0`.
    ///
    /// Table-driven deliberately: a knob added to `Config` without a zero check will not show up
    /// here as a compile error, so the list is also the checklist a reviewer reads against the
    /// struct.
    #[test]
    fn a_zero_stage_2_1_knob_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        let ingest_keys = [
            "commit_window_max_items",
            "commit_window_max_age_ms",
            "ingest_queue_bound",
            "ingest_admission",
            "ingest_max_batch_rows",
            "ingest_max_batch_bytes",
            "wal_hard_limit_bytes",
            "overlay_soft_limit",
            "flush_max_items",
            "flush_max_age_secs",
        ];
        let serve_keys = [
            "row_projection_cache_bytes",
            "fragment_cache_bytes",
            "expected_concurrent_sessions",
            "pin_ttl_secs",
            "pins_per_session_max",
        ];
        assert_eq!(
            ingest_keys.len() + serve_keys.len(),
            15,
            "there are fifteen write-path and admission knobs; this table must cover all of them"
        );

        for key in ingest_keys {
            let err = parse(&valid_toml_with("", &format!("{key} = 0"))).unwrap_err();
            let ConfigError::MustBeNonZero { key: named, .. } = err else {
                panic!("ingest.{key} = 0 must be refused as MustBeNonZero, got {err}");
            };
            assert_eq!(named, format!("ingest.{key}"));
        }
        for key in serve_keys {
            let err = parse(&valid_toml(&format!("{key} = 0"))).unwrap_err();
            let ConfigError::MustBeNonZero { key: named, .. } = err else {
                panic!("serve.{key} = 0 must be refused as MustBeNonZero, got {err}");
            };
            assert_eq!(named, format!("serve.{key}"));
        }
    }

    /// A misspelt key or section is **refused**, not defaulted.
    ///
    /// The three cases are the three an operator actually hits: a key typo'd inside a real section
    /// (`wal_hard_limit` for `wal_hard_limit_bytes`), a key put in the *wrong* section (a `serve`
    /// key under `[ingest]` — the two-section split makes this the easy mistake), and a typo'd
    /// section header (`[ingestion]`).
    /// Without `deny_unknown_fields` all three parse clean and silently default, so an operator
    /// tuning the write path gets the shipped behaviour and no signal at all.
    #[test]
    fn a_misspelt_key_or_section_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        for toml in [
            valid_toml_with("", "wal_hard_limit = 999000000"),
            valid_toml_with("", "pin_ttl_secs = 42"),
            valid_toml("commit_window_max_items = 7"),
            valid_toml("").replace("[serve]", "[serv]\n[serve]"),
        ] {
            let err = parse(&toml).unwrap_err();
            assert!(
                matches!(err, ConfigError::Toml(_)),
                "an unrecognised key or section must be refused, got {err}"
            );
        }
    }

    /// Every `.rs` file under `crates/` — other than this module, the one legitimate mention —
    /// that **uses** any of `keys` after comments are stripped.
    ///
    /// **Comments are stripped before the scan**, and that is not an incidental detail: a scan over
    /// raw text matches prose too, which forces every doc comment in the tree that explains why a
    /// key does nothing to avoid naming it — a test making documentation worse to keep itself green.
    /// A mention is not a consumer; only a *use* is, and after comment-stripping any surviving
    /// occurrence is one.
    ///
    /// Shared by the two inertness tests rather than inlined into either, so each can carry its own
    /// failure message: they are instructions to different readers.
    fn files_using(keys: &[&str]) -> Vec<String> {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/<crate> always has two ancestors")
            .to_path_buf();
        let crates = workspace.join("crates");
        assert!(
            crates.is_dir(),
            "cannot scan for consumers: {} is not a directory",
            crates.display()
        );
        let this_file = workspace.join("crates/tessera-server/src/config.rs");
        assert!(this_file.is_file(), "{} moved", this_file.display());

        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![crates];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("readable crates tree") {
                let path = entry.expect("readable dir entry").path();
                if path.is_dir() {
                    // `target/` can appear inside a crate dir; nothing generated is a consumer.
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") && path != this_file {
                    let text = fs::read_to_string(&path).expect("readable source file");
                    let code = strip_comments(&text);
                    if keys.iter().any(|k| code.contains(k)) {
                        offenders.push(path.display().to_string());
                    }
                }
            }
        }
        offenders
    }

    /// **The second inertness assertion.** `commit_window_max_age_ms` is parsed, validated and
    /// stored, and *nothing reads it*: the consumer it would have — a third window-close trigger —
    /// has no subject, because a `CommitWindow` is a local of `Executor::run_work_pass` that every
    /// exit disposes of, so no window ever waits and there is no interval for an age bound to end.
    /// The full argument is at [`DEFAULT_COMMIT_WINDOW_MAX_AGE_MS`] and at
    /// `Executor::run_work_pass`.
    ///
    /// It is a **separate test** from [`the_flush_knobs_are_inert`] rather than a third key added to
    /// it, because that test's failure message is a specific instruction to whoever wires flush and
    /// this key's is a different instruction to a different reader. They share only the scan.
    ///
    /// **This test is what makes "there is no timer" honest**, rather than a decoration: the claim
    /// only holds if an operator cannot set the key and believe one exists. Comments are stripped
    /// before the scan, so the prose above — and every doc block in the tree that names this key to
    /// explain why it does nothing — is deliberately fine.
    #[test]
    fn the_commit_window_age_bound_is_inert() {
        let offenders = files_using(&["commit_window_max_age_ms"]);
        assert!(
            offenders.is_empty(),
            "commit_window_max_age_ms is documented as INERT (the commit window never waits, so \
             an age bound has no subject), but is USED (outside a comment) in: \
             {offenders:?}. Naming the key in prose is fine — comments are stripped before this \
             scan. If something now consumes it, that something is a *linger* and it needs the \
             argument at DEFAULT_COMMIT_WINDOW_MAX_AGE_MS answered first; then delete this test AND \
             the INERT paragraphs on that constant, on the Config field and in this module's own \
             doc, in the same commit"
        );
    }

    /// Line and block comments removed; string literals are left alone.
    ///
    /// Deliberately crude — it is scanning for a handful of identifiers, not parsing Rust. The one
    /// way it can be wrong is a `//` inside a string literal on a line that also *uses* one of the
    /// keys, which would hide a real consumer; there is no such line, and the failure direction
    /// would be a missed offender in a test whose job is to notice a whole new consumer appearing.
    fn strip_comments(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_block = false;
        for line in text.lines() {
            let mut rest = line;
            loop {
                if in_block {
                    match rest.find("*/") {
                        Some(end) => {
                            in_block = false;
                            rest = &rest[end + 2..];
                        }
                        None => {
                            rest = "";
                            break;
                        }
                    }
                } else {
                    let line_at = rest.find("//");
                    let block_at = rest.find("/*");
                    match (line_at, block_at) {
                        (Some(l), b) if b.is_none_or(|b| l < b) => {
                            out.push_str(&rest[..l]);
                            rest = "";
                            break;
                        }
                        (_, Some(b)) => {
                            out.push_str(&rest[..b]);
                            in_block = true;
                            rest = &rest[b + 2..];
                        }
                        _ => break,
                    }
                }
            }
            out.push_str(rest);
            out.push('\n');
        }
        out
    }

    /// The defaults are a *consistent set*, not fifteen independent numbers. `parse` refuses to
    /// start unless `ingest_queue_bound × ingest_max_batch_bytes`, plus
    /// [`RESERVED_DENY_HEADROOM_BYTES`], sits strictly below `wal_hard_limit_bytes` — so if the
    /// defaults did not satisfy it, the *default* configuration would refuse to start, which is not
    /// a default.
    ///
    /// Asserted against **the constant the check uses**, not against a proxy for it: a test that
    /// asserted `queue_worst_case × 2 < ceiling` — the same shape, a different operand — would go on
    /// passing if the reserve ever changed.
    #[test]
    fn defaults_satisfy_task_6s_headroom_relation() {
        let queue_worst_case =
            DEFAULT_INGEST_QUEUE_BOUND as u64 * DEFAULT_INGEST_MAX_BATCH_BYTES as u64;
        assert!(
            queue_worst_case + RESERVED_DENY_HEADROOM_BYTES < DEFAULT_WAL_HARD_LIMIT_BYTES,
            "queue worst case {queue_worst_case} B plus {RESERVED_DENY_HEADROOM_BYTES} B of \
             reserved deny headroom does not fit strictly under the WAL ceiling \
             {DEFAULT_WAL_HARD_LIMIT_BYTES} B"
        );
    }

    /// **A default configuration must start on every machine, and it did not.**
    ///
    /// The earlier version of this test *documented the bite* instead of removing it: it asserted
    /// only that the ceiling admits ≥ 256 cores. It does — and at ~1001 cores `4n +
    /// ingest_admission + reserve` crossed [`SERVING_BLOCKING_THREAD_CEILING`] and a `tessera.toml`
    /// that named neither knob refused to start on a box where it had previously started fine.
    ///
    /// The derived default is clamped now, so the property under test is the one that matters:
    /// **a config with no `serve.compute_admission` and no `ingest.ingest_admission` parses at any
    /// `compute_threads`.** `compute_threads` is set explicitly because the true default reads this
    /// box's `available_parallelism`, which cannot be made large.
    ///
    /// **Mutation:** drop the `.min(...)` clamp from the `None` arm and the first leg becomes a
    /// `BlockingThreadCeiling` error.
    #[test]
    fn a_defaulted_config_starts_on_a_machine_of_any_size() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        let config = parse(&valid_toml_with("compute_threads = 100000", ""))
            .expect("a defaulted configuration must start on a machine of any size");
        assert_eq!(
            config.compute_admission,
            SERVING_BLOCKING_THREAD_CEILING - DEFAULT_INGEST_ADMISSION - BLOCKING_THREAD_RESERVE,
            "the derived default must clamp to the largest admission the blocking pool can carry"
        );
        assert!(serving_blocking_threads(&config) <= SERVING_BLOCKING_THREAD_CEILING);

        // And a modest box is untouched by the clamp — otherwise this would pass with a constant.
        let config = parse(&valid_toml_with("compute_threads = 8", "")).unwrap();
        assert_eq!(config.compute_admission, 8 * COMPUTE_ADMISSION_MULTIPLIER);

        // **The clamp is only on the defaulted arm.** An operator who names a number that does not
        // fit is still refused, rather than silently given a different one.
        let err = parse(&valid_toml_with(
            &format!("compute_admission = {SERVING_BLOCKING_THREAD_CEILING}"),
            "",
        ))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::BlockingThreadCeiling { .. }),
            "an explicit compute_admission must still refuse, not clamp: {err}"
        );
    }

    /// **The queue-full 429 must be reachable at the shipped defaults.** With
    /// `ingest_admission == ingest_queue_bound` an admitted handler holds at most one queue entry
    /// (`Engine::accept_ingest` blocks on its receipt), so outstanding entries are bounded by
    /// admitted handlers and `try_send` can never observe `Full`. The whole queue-backpressure wire
    /// surface — `SubmitError::QueueFull`, `estimate_retry_after_s` at any depth above zero, the
    /// derived `retry_after_s` — would then be dead at the defaults, reachable only through a
    /// `start_write_executor(0)` spelling `non_zero_usize` refuses to operators.
    ///
    /// **Mutation:** set `DEFAULT_INGEST_QUEUE_BOUND` back to 64 and this goes red.
    #[test]
    fn the_default_admission_bound_exceeds_the_default_queue_bound() {
        // The peak the queue can reach: every admitted handler holding one entry, less the one the
        // executor has already taken. `Full` is observable only if that exceeds the queue's depth.
        let peak_outstanding = DEFAULT_INGEST_ADMISSION.saturating_sub(1);
        assert!(
            peak_outstanding > DEFAULT_INGEST_QUEUE_BOUND,
            "with {DEFAULT_INGEST_ADMISSION} admitted handlers against a queue of \
             {DEFAULT_INGEST_QUEUE_BOUND} the work queue peaks at {peak_outstanding} and can never \
             fill, because a handler holds its receipt open across its own queue slot — the \
             queue-full 429 would be unreachable in every operator-legal configuration"
        );
    }

    /// **The per-connection ceiling refuses at its own boundary, and admits below it.**
    ///
    /// Both legs, because a ceiling tested only from above is satisfied just as well by a check that
    /// refuses everything — and this key's default is the one every deployment uses.
    #[test]
    fn the_per_connection_batch_ceiling_is_enforced_at_startup() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        let at_ceiling = parse(&valid_toml_with(
            "",
            &format!("ingest_max_batch_bytes = {INGEST_MAX_BATCH_BYTES_CEILING}"),
        ))
        .expect("exactly at the ceiling must start");
        assert_eq!(
            at_ceiling.ingest_max_batch_bytes,
            INGEST_MAX_BATCH_BYTES_CEILING
        );

        let over = parse(&valid_toml_with(
            "",
            &format!(
                "ingest_max_batch_bytes = {}",
                INGEST_MAX_BATCH_BYTES_CEILING + 1
            ),
        ))
        .unwrap_err();
        let ConfigError::IngestBatchBytesCeiling {
            ingest_max_batch_bytes,
        } = over
        else {
            panic!("one byte over the per-connection ceiling must refuse to start: {over}");
        };
        assert_eq!(
            ingest_max_batch_bytes,
            INGEST_MAX_BATCH_BYTES_CEILING + 1,
            "the refusal must carry the value it refused, so the message can name it"
        );

        // The message has to carry the two facts an operator cannot get from the key's name: that
        // the cost is per connection, and that nothing in this process bounds how many there are.
        let text = over.to_string();
        for phrase in ["buffered in full", "no connection cap", "reverse proxy"] {
            assert!(
                text.contains(phrase),
                "the refusal must say why a per-connection ceiling exists ({phrase:?} missing): \
                 {text}"
            );
        }
    }

    /// The third relation's defaults, for the same reason as the other two: a default configuration
    /// that could not start is not a default.
    #[test]
    fn defaults_satisfy_task_6s_ingest_resident_relation() {
        let resident = (DEFAULT_INGEST_QUEUE_BOUND + DEFAULT_INGEST_ADMISSION) as u64
            * DEFAULT_INGEST_MAX_BATCH_BYTES as u64;
        assert!(
            resident <= INGEST_RESIDENT_CEILING_BYTES,
            "the defaults ask for {resident} B resident, above the \
             {INGEST_RESIDENT_CEILING_BYTES} B ceiling"
        );
    }

    /// **The startup arithmetic bounded threads and WAL bytes; heap is what binds.**
    ///
    /// `compute_admission = 8, ingest_admission = 4000` satisfied both of the earlier relations —
    /// 8 + 4000 + 32 = 4040 threads is under the ceiling, and the WAL relation does not mention
    /// `ingest_admission` at all — while admitting four thousand concurrent handlers each holding a
    /// decoded 16 MiB batch. Relation 3 is what refuses it, and it must name every operand, since
    /// three different knobs can fix it.
    ///
    /// The second leg is the configuration every other relation admitted:
    /// `ingest_queue_bound = 1, ingest_max_batch_bytes = 1 GiB` started fine, because the WAL
    /// relation's product is small when the queue is shallow and nothing else looked at the byte cap
    /// at all. It is now refused **one check earlier**, by
    /// [`INGEST_MAX_BATCH_BYTES_CEILING`] — and the leg asserts that rather than the resident
    /// ceiling, because the narrower refusal is the true one: a gigabyte batch cap is a
    /// per-*connection* cost, and a caller does not need to be admitted to pay it. The consequence
    /// worth knowing is that with a per-connection ceiling in place, relation 3 is reachable only
    /// through the two count knobs — no legal `ingest_max_batch_bytes` can trip it against a shallow
    /// queue any more.
    ///
    /// **Mutation:** drop `ingest_admission` from `resident_worst_case` and leg 1 goes green — the
    /// queue term alone (32 × 16 MiB) is nowhere near the ceiling, which is precisely the hole.
    #[test]
    fn an_admission_bound_that_ignores_heap_is_refused_at_startup() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        let err = parse(&valid_toml_with(
            "compute_admission = 8",
            "ingest_admission = 4000",
        ))
        .unwrap_err();
        let ConfigError::IngestResidentCeiling {
            ingest_admission,
            ingest_max_batch_bytes,
            required,
            ..
        } = err
        else {
            panic!("4000 concurrent 16 MiB batches must refuse to start: {err}");
        };
        assert_eq!(ingest_admission, 4000);
        assert_eq!(
            required,
            (DEFAULT_INGEST_QUEUE_BOUND + 4000) as u64 * ingest_max_batch_bytes as u64
        );
        let text = err.to_string();
        for operand in [
            DEFAULT_INGEST_QUEUE_BOUND.to_string(),
            4000.to_string(),
            ingest_max_batch_bytes.to_string(),
            required.to_string(),
        ] {
            assert!(text.contains(&operand), "{operand} missing from: {text}");
        }

        // Leg 2 — the shallow-queue, enormous-batch configuration every other relation admitted.
        let err = parse(&valid_toml_with(
            "",
            "ingest_queue_bound = 1\ningest_max_batch_bytes = 1073741824\n\
             wal_hard_limit_bytes = 8589934592",
        ))
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::IngestBatchBytesCeiling {
                    ingest_max_batch_bytes: 1073741824
                }
            ),
            "a 1 GiB batch cap is a per-connection cost paid before admission; the WAL relation \
             sees only the 1-deep queue and admits it: {err}"
        );
    }

    /// Both relations refuse, and each refusal names both of its operands.
    ///
    /// Two legs in one test deliberately: the deliverable is "*the* startup headroom assertion",
    /// and a reader checking it should see both halves and their two different shapes — one is a
    /// relation between operator values, the other is a ceiling on a derived resource.
    #[test]
    fn headroom_arithmetic_is_checked_at_startup() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        // Leg 1 — WAL bytes. A queue deep enough that its worst case plus the reserve does not fit
        // under the configured ceiling.
        let err = parse(&valid_toml_with(
            "",
            "ingest_queue_bound = 1024\ningest_max_batch_bytes = 16777216\n\
             wal_hard_limit_bytes = 8589934592",
        ))
        .unwrap_err();
        let ConfigError::WalHeadroom {
            queue_worst_case,
            wal_hard_limit_bytes,
            ..
        } = err
        else {
            panic!("a queue whose worst case exceeds the WAL ceiling must refuse to start: {err}");
        };
        assert_eq!(queue_worst_case, 1024 * 16 * 1024 * 1024);
        assert_eq!(wal_hard_limit_bytes, 8 * 1024 * 1024 * 1024);
        // Both sides in the message, so an operator can act without reading this file.
        let text = ConfigError::WalHeadroom {
            queue_worst_case,
            reserved_deny_headroom: RESERVED_DENY_HEADROOM_BYTES,
            wal_hard_limit_bytes,
            ingest_queue_bound: 1024,
            ingest_max_batch_bytes: 16 * 1024 * 1024,
        }
        .to_string();
        assert!(text.contains(&queue_worst_case.to_string()), "{text}");
        assert!(text.contains(&wal_hard_limit_bytes.to_string()), "{text}");
        assert!(
            text.contains(&RESERVED_DENY_HEADROOM_BYTES.to_string()),
            "the reserved deny headroom is the term that makes this relation more than \
             'the queue fits'; it must be named: {text}"
        );

        // **Leg 1b — the reserved deny headroom is load-bearing, and the leg above does not show
        // it.** Planting "drop the `RESERVED_DENY_HEADROOM_BYTES` term" left the leg above GREEN:
        // its queue worst case (16 GiB) exceeds the ceiling on its own, so the reserve never
        // decided anything. This leg is the one where the reserve is the *only* reason the
        // configuration is refused — the queue fits, and it fits with nothing left over for the
        // denies that are never shed for load. That is precisely lifecycle §4's headroom rule, and
        // without this leg the term could be deleted with every test still passing.
        let snug = 16 * 1024 * 1024 + RESERVED_DENY_HEADROOM_BYTES / 2;
        let err = parse(&valid_toml_with(
            "",
            &format!(
                "ingest_queue_bound = 1\ningest_max_batch_bytes = 16777216\n\
                 wal_hard_limit_bytes = {snug}"
            ),
        ))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::WalHeadroom { .. }),
            "the queue alone fits under this ceiling; it is the reserved deny headroom that must \
             not, and dropping that term from the relation must not go unnoticed: {err}"
        );

        // The same configuration one byte of ceiling above the requirement loads, which is what
        // makes the legs above a statement about the relation rather than about the numbers. The
        // queue bound here is 64 rather than the 1024 above because relation 3 (resident bytes) is
        // evaluated after this one and a 1024-deep queue of 16 MiB batches exceeds *it* — which is
        // the point of relation 3, not a workaround for it.
        let ok_ceiling = 64u64 * 16 * 1024 * 1024 + RESERVED_DENY_HEADROOM_BYTES + 1;
        let config = parse(&valid_toml_with(
            "",
            &format!(
                "ingest_queue_bound = 64\ningest_max_batch_bytes = 16777216\n\
                 wal_hard_limit_bytes = {ok_ceiling}"
            ),
        ))
        .expect("strictly below the ceiling must load");
        assert_eq!(config.wal_hard_limit_bytes, ok_ceiling);

        // Leg 2 — blocking threads.
        let err = parse(&valid_toml_with(
            &format!("compute_admission = {SERVING_BLOCKING_THREAD_CEILING}"),
            "",
        ))
        .unwrap_err();
        let ConfigError::BlockingThreadCeiling {
            compute_admission,
            ingest_admission,
            required,
        } = err
        else {
            panic!("a blocking pool above the ceiling must refuse to start: {err}");
        };
        assert_eq!(compute_admission, SERVING_BLOCKING_THREAD_CEILING);
        assert_eq!(ingest_admission, DEFAULT_INGEST_ADMISSION);
        assert_eq!(
            required,
            SERVING_BLOCKING_THREAD_CEILING + DEFAULT_INGEST_ADMISSION + BLOCKING_THREAD_RESERVE
        );
        let text = ConfigError::BlockingThreadCeiling {
            compute_admission,
            ingest_admission,
            required,
        }
        .to_string();
        assert!(text.contains(&compute_admission.to_string()), "{text}");
        assert!(text.contains(&ingest_admission.to_string()), "{text}");
    }

    /// The pool is **derived from its consumers**, not a constant they must fit under — which is
    /// the whole reason a 128-core box does not refuse to start. Mutating
    /// [`serving_blocking_threads`] to return a constant makes this red.
    #[test]
    fn the_serving_blocking_pool_covers_its_declared_consumers() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        // 128 cores' worth of compute admission: comfortably past tokio's undeclared 512 default,
        // which is the figure this task replaced.
        let config = parse(&valid_toml_with(
            "compute_admission = 512",
            "ingest_admission = 64",
        ))
        .expect("a large but serviceable machine must start");
        let pool = serving_blocking_threads(&config);
        assert!(
            pool >= config.compute_admission + config.ingest_admission,
            "the pool ({pool}) must cover both admission bounds, or an admitted request can find \
             no blocking thread"
        );
        assert!(
            pool > 512,
            "and it must not be pinned to tokio's old default"
        );
    }

    /// The other cross-knob relation, for the same reason. [`crate::validate_cache_bounds`] refuses
    /// to start unless each cache bound admits at least `expected_concurrent_sessions` entries at
    /// the measured per-entry size — so the defaults must admit that many. **Both** caches, not just
    /// the projection one: they hold the same-shaped Roaring object at the same measured size, and a
    /// validation covering one would leave the other free to be set to a value that collapses.
    #[test]
    fn defaults_satisfy_task_5s_cache_relation() {
        let working_set =
            DEFAULT_EXPECTED_CONCURRENT_SESSIONS as u64 * MEASURED_PROJECTION_BYTES_AT_1E9;
        assert!(
            DEFAULT_ROW_PROJECTION_CACHE_BYTES >= working_set,
            "{DEFAULT_ROW_PROJECTION_CACHE_BYTES} B admits fewer than \
             {DEFAULT_EXPECTED_CONCURRENT_SESSIONS} projections of \
             {MEASURED_PROJECTION_BYTES_AT_1E9} B"
        );
        assert!(
            DEFAULT_FRAGMENT_CACHE_BYTES >= working_set,
            "{DEFAULT_FRAGMENT_CACHE_BYTES} B admits fewer than \
             {DEFAULT_EXPECTED_CONCURRENT_SESSIONS} fragments of \
             {MEASURED_PROJECTION_BYTES_AT_1E9} B — this cache is keyed by grant set rather than \
             by session, so eight entries is eight distinct policies, but the per-entry size and \
             the collapse mode are the projection cache's"
        );
        // The projection cache carries a further 2× (the fragment cache deliberately does not —
        // see both constants). It is entry-count headroom, NOT a correction for a serialised-size
        // underestimate: that underestimate is an array-container property and does not apply at
        // the bitmap-dominated dense bound this figure describes.
        assert!(
            DEFAULT_ROW_PROJECTION_CACHE_BYTES >= 2 * working_set,
            "the projection bound must carry its 2× entry-count headroom: the key is \
             (token_id, slice, segments_version), so a second slice or a generation swap doubles \
             the entries at unchanged session concurrency"
        );
    }
}
