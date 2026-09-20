//! Fail-closed configuration: `tessera.toml` (SA §7, `configuration.md` §3).
//!
//! **One file, read by both verbs.** `tessera.toml` says what this *deployment* is — where the
//! bundle lives, where its cache and WAL go, what to listen on, how to tune it, where the corpus
//! declaration is, and which environment variable carries the identity key. `schema.toml` says
//! what the *corpus* is. The secret is in neither: it is in the environment, or in a `.env` beside
//! this file, and never in git.
//!
//! **The build's output path and the server's `bundle_path` are one value seen from two sides**,
//! so they are declared once — [`Config::bundle_path`] — and `tessera build --out` overrides it.
//! Declaring them apart is how a server comes up against a directory the last build did not write.
//!
//! **Found by walking up from the working directory**, as `Cargo.toml` is ([`discover`]), so
//! `tessera build` and `tessera serve` take no required flag anywhere under the project root.
//! **A missing file is a refusal naming what to create** ([`ConfigError::NoDeploymentConfig`]),
//! never a silent set of defaults: a deployment that came up on guessed paths would serve an empty
//! bundle out of a directory nobody chose.
//!
//! **Every path in it resolves against its own directory**, not the working directory — the same
//! rule a `source` in `schema.toml` follows. A relative `bundle.path` that moved with the shell's
//! cwd would make `cd crates && tessera serve` open a different bundle from the one `tessera
//! build` had just written.
//!
//! `[disclosure]` has no defaults at all: absence of the section, or of the key inside it, is a
//! startup refusal naming what to write. The section holds `token_max_lifetime` alone, and a key
//! it does not know is refused like any other section's. The membership requirement an artifact
//! layer gates on is declared per layer, in the corpus declaration, and has no deployment-wide
//! form.
//! Every other section either has a documented default (`max_k = 1000`) or is required outright.
//! Credentials are never inline: `[serve]`'s `*_credential_file`/`*_credential_env` pairs are the
//! only way to supply the session/operator bearer secrets, and the secret itself is read at
//! [`crate::prepare`] rather than here — `tessera build` reads this same file, and a build has no
//! business requiring a serving secret to be exported before it will write a bundle. A plane still
//! cannot come up without one ([`Credential::resolve`]).
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
//! **No key is inert, by rule** (decision 0045): a key exists only while something reads it.
//! Two were deleted under that rule. One is back: **`ingest.flush_max_items` was restored on
//! 2026-09-04 with a consumer** — the flush tick now comes due on buffered rows as well as on age
//! (see [`DEFAULT_FLUSH_MAX_ITEMS`]), which is a different mechanism from the "flush-ready" mark
//! 0045 deleted and is read rather than merely stored. The other,
//! `ingest.commit_window_max_age_ms`, stays deleted (an age bound has no subject in an executor
//! whose window never lingers — decision 0034, whose keep-parsed clause 0045 supersedes), and a
//! `tessera.toml` naming it is refused with an error naming it, which is louder than the silent
//! no-op the key used to buy.

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
    /// No `tessera.toml` anywhere from the working directory up to the filesystem root, and no
    /// `--deployment` naming one.
    ///
    /// **A refusal naming what to create, never a set of defaults.** Every path this file carries
    /// is a decision — which bundle, which cache, which WAL, which declaration — and a guessed one
    /// is a build writing where nobody asked or a server opening a bundle nobody built.
    NoDeploymentConfig {
        from: PathBuf,
    },
    /// `[identity]` carries the key itself rather than the name of the variable holding it.
    ///
    /// Refused with its own message rather than as an unknown field, because the mistake is
    /// *reasonable* — the `--identity-file` format does spell it `key` — and the consequence is a
    /// secret in a file that is meant to be committed.
    IdentityKeyInline,
    /// A key inside `[identity]` that is not `env`.
    UnknownIdentityKey(String),
    /// `[identity]` is present but is not a table.
    IdentityNotATable,
    /// The `[disclosure]` section is absent entirely.
    MissingDisclosureSection,
    /// The `[disclosure]` section is present but missing one of its required keys.
    MissingDisclosureKey(&'static str),
    /// Neither `*_credential_file` nor `*_credential_env` was set for this credential, or the
    /// named environment variable was not set.
    MissingCredential(&'static str),
    /// A `*_credential_file` was named and could not be read. Separate from [`ConfigError::Io`]
    /// so the refusal carries the path it tried, which a bare `No such file or directory` does
    /// not, and the path is the resolved one.
    CredentialFileUnreadable {
        which: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    BadAddr(String),
    /// Only `builtin:passthrough` is available; the wasmtime plugin host is not built.
    UnsupportedPlugin(String),
    /// A CORS origin list carries `*`.
    ///
    /// Refused rather than dropped, and refused for both lists. The reason is decision 0102's and
    /// not the layer's: an origin list is a deployment's statement about which pages may present
    /// its tokens, and a wildcard says every page, which is the one thing an enumerated list is
    /// for not saying. (`tower_http`'s `AllowOrigin::list` also panics on one, so an unchecked
    /// wildcard would be a process that dies at router construction rather than at parse.)
    CorsWildcard {
        key: &'static str,
    },
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
    /// `serve.tier_width` or `serve.coalesce_width` below 2 — a width that can select nothing.
    ///
    /// Selection needs a window of at least two entries on every axis (`MergePolicy::select`
    /// returns `None` below 2, and the coalesce's dispatch declines the same way), so 1 does not
    /// mean "merge eagerly": it means the pass never runs, segments (or tiers, runs and dictionary
    /// extents) accumulate for the life of the deployment, and the failure looks like a policy
    /// that is simply never triggered. Refused at startup rather than discovered at the first
    /// tick, and refused rather than read as "off": silently disabling a maintenance pass is the
    /// failure this file exists to prevent, and these keys deliberately have no "off" spelling —
    /// a deployment that wants a pass to stay out of the way sets the width above what its write
    /// pattern can accumulate.
    SelectionWidthBelowTwo {
        key: &'static str,
        width: usize,
    },
    /// `ingest.compaction_window_start` is neither `HH:MM` nor `"off"`.
    ///
    /// Refused rather than defaulted, because the two failure directions are both bad and neither
    /// is visible: a value read as "off" silently retires the deployment's only fold schedule, and
    /// one read as midnight silently starts hours of IO at an hour the operator was trying to
    /// avoid.
    CompactionWindowNotATime(String),
    /// `ingest.compaction_after_deletions` or `ingest.compaction_max_segments` is a string other
    /// than `"off"`. See [`RawThreshold`].
    CompactionThresholdNotANumberOrOff(String),
    /// A ratio key — `ingest.compaction_dead_rows_fraction` or `ingest.compaction_dead_bytes_ratio`
    /// — is a string other than `"off"`. See [`RawRatio`].
    CompactionRatioNotANumberOrOff {
        key: &'static str,
        value: String,
    },
    /// A ratio key is zero, negative, or not finite: a route that can never decline. See
    /// [`ratio_or_off`].
    CompactionRatioNotPositive {
        key: &'static str,
        value: f64,
    },
    /// `ingest.compaction_max_segments` is at or below `ingest.compaction_window_min_segments`,
    /// with the window armed.
    ///
    /// The two are a floor and a ceiling over one gauge — "worth folding tonight" and "cannot wait
    /// for tonight" — so a ceiling at or below the floor makes the window **unreachable**: every
    /// count that would have opened it has already fired the any-hour route. Refused rather than
    /// shipped, because the result is three keys that parse, validate and can never fire, which is
    /// the inert key decision 0045 forbids.
    CompactionSegmentThresholdsInverted {
        window_min_segments: usize,
        max_segments: usize,
    },
    /// `ingest.compaction_window_secs` is at or past a whole day, which makes the "window" every
    /// hour of every day — i.e. the ungated timer compaction §9 declines, reached by setting a
    /// width rather than by asking for one.
    CompactionWindowNotAWindow(u32),
    /// `serve.max_underlay_offset` above the grid's own depth. The §5.2 grid is 2¹⁶ × 2¹⁶, so an
    /// offset beyond 16 can never be usable at any zoom — every request naming it would be refused
    /// at the depth check. It is refused at startup instead, because the value also feeds a
    /// `1 << (2 * offset)` shift and this file validates every other selection constant; leaving one
    /// shift input unbounded is an inconsistent standard rather than a considered exemption.
    UnderlayOffsetTooDeep(u8),
    /// `serve.theta_target_marks = 0`, which anchors θ at `Cut(0)` — a threshold that admits
    /// **nothing**, at every depth. `θ_d = m_target · N_occ(d) / V_total` is zero for every
    /// occupancy when `m_target` is, so no depth and no corpus shape can lift it. Every non-empty
    /// tile would draw exactly `k_min` marks at every zoom for ever, with no error raised anywhere:
    /// design §7.2's density signal gone.
    ///
    /// This is the *same* failure mode `Threshold::at_depth`'s saturation test exists to prevent —
    /// a cut of zero reached without an error — arriving through config instead of through
    /// arithmetic, so it is refused in the same spirit.
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
    /// `serve.single_flight_wait_ms = 0` would reinstate the immediate refusal decision 0058
    /// removed: a racer would find a build in flight, park for no time at all, and be shed with
    /// the 429 whose `Retry-After` is shorter than the build it is waiting for. Refused rather
    /// than accepted as "disable waiting", because that is a behaviour the decision rules out
    /// rather than a knob position.
    SingleFlightWaitZero,
    /// `serve.stream_flush_bytes = 0`: every gathered tile would flush alone, so the frame
    /// overhead is paid per tile and the emit loop sends thousands of tiny frames — a knob
    /// position with no use, refused so it reads as the mistake it is. There is no "disable
    /// chunking" spelling because chunk boundaries are not contract; a huge value approximates
    /// one flush per response honestly (the emit pass still splits at its own 1 GiB frame cap,
    /// far below the wire's u32 length bound — `tessera_engine`'s `MAX_POINTS_FRAME_BYTES`).
    StreamFlushBytesZero,
    /// `serve.stream_write_stall_ms = 0`: the very first send of every streamed response would
    /// exceed its stall budget before the client could read a byte — every viewport aborts,
    /// silently, as if the corpus were empty. Refused rather than read as "no deadline".
    StreamWriteStallZero,
    /// `serve.stream_deadline_ms = 0`: same shape — every stream exceeds a zero whole-stream
    /// budget immediately. An operator who wants effectively-unbounded streams sets it large and
    /// owns the slot-occupancy arithmetic (`streamed-serving.md` §5); zero is never that intent.
    StreamDeadlineZero,
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
            ConfigError::NoDeploymentConfig { from } => write!(
                f,
                "no tessera.toml found, searching upward from {}. It is what says where this \
                 deployment's bundle, cache and WAL live, where the corpus declaration is, and \
                 which environment variable carries the identity key -- none of which has a \
                 default worth guessing. Create one beside the corpus declaration:\n\n\
                 \x20   [bundle]\n\
                 \x20   path  = \"bundles/corpus\"   # tessera build --out writes here; \
                 tessera serve opens it\n\
                 \x20   cache = \".tessera/cache\"\n\
                 \x20   wal   = \".tessera/wal.log\"\n\n\
                 \x20   [build]\n\
                 \x20   schema = \"schema.toml\"     # the corpus declaration; this is the \
                 default\n\n\
                 \x20   [identity]\n\
                 \x20   env = \"TESSERA_IDENTITY_KEY\"   # the variable carrying the key; \
                 this is the default\n\n\
                 \x20   [plugin]\n\
                 \x20   module = \"builtin:passthrough\"\n\n\
                 \x20   [disclosure]\n\
                 \x20   token_max_lifetime = 3600\n\n\
                 \x20   [serve]\n\
                 \x20   viewer  = \"127.0.0.1:37585\"\n\
                 \x20   session = \"127.0.0.1:49303\"\n\
                 \x20   control = \"127.0.0.1:45721\"\n\n\
                 Or name one outright with --deployment <path>",
                from.display()
            ),
            ConfigError::IdentityKeyInline => write!(
                f,
                "tessera.toml: [identity] carries `key`. The identity key never appears in this \
                 file -- it is sixteen bytes keying the bijection every tessera_id a client holds \
                 is derived through, and this file belongs in git. Name the variable that carries \
                 it instead: `[identity] env = \"TESSERA_IDENTITY_KEY\"` (that is also the \
                 default, so the whole section may be omitted). The value goes in the environment, \
                 in a .env beside this file, or in a 0600 file named by --identity-file"
            ),
            ConfigError::UnknownIdentityKey(key) => write!(
                f,
                "tessera.toml: unknown key '{key}' in [identity]. The section takes `env` alone -- \
                 the name of the environment variable carrying the identity key, defaulting to \
                 TESSERA_IDENTITY_KEY. The key itself is never written here"
            ),
            ConfigError::IdentityNotATable => write!(
                f,
                "tessera.toml: [identity] must be a table: `[identity]` with \
                 `env = \"TESSERA_IDENTITY_KEY\"` under it"
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
            ConfigError::SelectionWidthBelowTwo { key, width } => write!(
                f,
                "{key} = {width} can select nothing: merge and coalesce selection need a window \
                 of at least two entries, so a width below 2 does not merge eagerly — it never \
                 merges at all, silently, and the artefact counts grow for the life of the \
                 deployment. Set 2 or more; to keep a pass out of the way, set the width above \
                 what the write pattern can accumulate"
            ),
            ConfigError::Io(e) => write!(f, "config io error: {e}"),
            ConfigError::Toml(e) => write!(f, "config parse error: {e}"),
            ConfigError::MissingDisclosureSection => write!(
                f,
                "tessera.toml is missing its [disclosure] section, which has no defaults to fall \
                 back on. Add `[disclosure]` with `token_max_lifetime = 3600` under it, in seconds"
            ),
            ConfigError::MissingDisclosureKey(key) => write!(
                f,
                "tessera.toml's [disclosure] section is missing '{key}', which has no default to \
                 fall back on. Write `{key} = 3600` under `[disclosure]`, in seconds"
            ),
            ConfigError::CredentialFileUnreadable {
                which,
                path,
                source,
            } => write!(
                f,
                "cannot read the '{which}' credential file '{}': {source}. The path named in \
                 [serve] resolves against tessera.toml's own directory, not the working directory",
                path.display()
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
            ConfigError::CompactionWindowNotATime(value) => write!(
                f,
                "ingest.compaction_window_start = '{value}' is neither a UTC time of day \
                 (HH:MM, 24-hour) nor 'off'. It is refused rather than defaulted: read as 'off' \
                 it silently retires the deployment's fold schedule, and read as midnight it \
                 silently starts hours of IO at the hour the operator was avoiding"
            ),
            ConfigError::CompactionRatioNotANumberOrOff { key, value } => write!(
                f,
                "{key} = \"{value}\" is neither a ratio nor \"off\". A maintenance route that \
                 silently does not run is indistinguishable from one with nothing to do, so a \
                 spelling this loader does not recognise is refused rather than read as absent"
            ),
            ConfigError::CompactionRatioNotPositive { key, value } => write!(
                f,
                "{key} = {value} is not a positive, finite ratio. Zero or negative is satisfied by \
                 every possible measurement, so the route would dispatch a fold at every tick that \
                 clears the interval floor — the ungated timer compaction §9 declines, reached by \
                 configuration. To switch the route off, write \"off\""
            ),
            ConfigError::CompactionThresholdNotANumberOrOff(value) => write!(
                f,
                "a compaction threshold is set to '{value}', which is neither a count nor 'off' \
                 (ingest.compaction_after_deletions, ingest.compaction_max_segments)"
            ),
            ConfigError::CompactionSegmentThresholdsInverted {
                window_min_segments,
                max_segments,
            } => write!(
                f,
                "ingest.compaction_max_segments = {max_segments} is not above \
                 ingest.compaction_window_min_segments = {window_min_segments}. The two are a \
                 floor and a ceiling over one gauge — 'worth folding tonight' and 'cannot wait for \
                 tonight' — so a ceiling at or below the floor makes the window unreachable and \
                 its three keys inert"
            ),
            ConfigError::CompactionWindowNotAWindow(secs) => write!(
                f,
                "ingest.compaction_window_secs = {secs} is a whole day or more, which makes the \
                 window every hour of every day — the ungated timer compaction §9 declines, \
                 reached by setting a width. Set 'compaction_window_start = \"off\"' if the \
                 intent is to fold whenever there is work, and let compaction_max_segments say \
                 how much work"
            ),
            ConfigError::CorsWildcard { key } => write!(
                f,
                "serve.{key} contains \"*\". A CORS origin list is enumerated or it is absent: it \
                 is this deployment's statement about which pages may present its session tokens \
                 in a browser, and a wildcard says every page there has ever been. Name the \
                 origins, or remove the key and let the layer be absent"
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
            ConfigError::SingleFlightWaitZero => write!(
                f,
                "serve.single_flight_wait_ms = 0 would refuse a request the instant it finds \
                 another request already building its row projection — the behaviour decision \
                 0058 removed. Set a budget that outlasts a cold build."
            ),
            ConfigError::StreamFlushBytesZero => write!(
                f,
                "serve.stream_flush_bytes = 0 would flush every gathered tile as its own frame; \
                 set the flush threshold the client should decode per view (default 1 MiB)"
            ),
            ConfigError::StreamWriteStallZero => write!(
                f,
                "serve.stream_write_stall_ms = 0 would abort every streamed response at its \
                 first send; set how long one send may wait on a non-reading client"
            ),
            ConfigError::StreamDeadlineZero => write!(
                f,
                "serve.stream_deadline_ms = 0 would abort every streamed response immediately; \
                 set the whole-stream budget that bounds a slot's occupancy"
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    bundle: RawBundle,
    plugin: RawPlugin,
    #[serde(default)]
    disclosure: Option<RawDisclosure>,
    /// `[build]` — what `tessera build` needs and `tessera serve` ignores. Absent means the whole
    /// section defaults, which is the ordinary case: the declaration is `schema.toml` beside this
    /// file.
    #[serde(default)]
    build: RawBuild,
    /// `[identity]` — the **name** of the environment variable carrying the identity key, never
    /// the key. Parsed as a `toml::Value` and validated by hand, so `key = "…"` gets the refusal
    /// it deserves rather than serde's unknown-field text.
    #[serde(default)]
    identity: Option<toml::Value>,
    #[serde(default)]
    serve: RawServe,
    /// SA §7's `[ingest]` section — every key optional, so the whole section may be absent. These
    /// are the write path's performance knobs; none of them is a disclosure control.
    #[serde(default)]
    ingest: RawIngest,
}

/// `[disclosure]`. The section, and the key in it, are both required; the `Option`s carry
/// "absent" as far as the refusal that names what to write.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisclosure {
    #[serde(default)]
    token_max_lifetime: Option<u64>,
}

/// SA §7's `[ingest]` section. Every field is `Option` and the struct is `Default`, so
/// a `tessera.toml` with no `[ingest]` section at all parses to "every key defaulted".
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawIngest {
    #[serde(default)]
    commit_window_max_items: Option<usize>,
    #[serde(default)]
    ingest_queue_bound: Option<usize>,
    #[serde(default)]
    ingest_admission: Option<usize>,
    #[serde(default)]
    ingest_max_batch_rows: Option<usize>,
    #[serde(default)]
    ingest_max_batch_bytes: Option<usize>,
    #[serde(default)]
    publish_max_body_bytes: Option<usize>,
    #[serde(default)]
    max_artifacts_per_request: Option<usize>,
    #[serde(default)]
    max_members_per_request: Option<usize>,
    #[serde(default)]
    max_excluded_per_request: Option<usize>,
    #[serde(default)]
    wal_hard_limit_bytes: Option<u64>,
    #[serde(default)]
    overlay_soft_limit: Option<usize>,
    #[serde(default)]
    flush_max_age_secs: Option<u64>,
    #[serde(default)]
    flush_max_items: Option<usize>,
    #[serde(default)]
    ingest_buffer_max_items: Option<usize>,
    #[serde(default)]
    compaction_min_interval_secs: Option<u64>,
    #[serde(default)]
    compaction_window_start: Option<String>,
    #[serde(default)]
    compaction_window_secs: Option<u32>,
    #[serde(default)]
    compaction_window_min_segments: Option<usize>,
    #[serde(default)]
    compaction_max_segments: Option<RawThreshold>,
    #[serde(default)]
    compaction_after_deletions: Option<RawThreshold>,
    #[serde(default)]
    compaction_dead_rows_fraction: Option<RawRatio>,
    #[serde(default)]
    compaction_dead_bytes_ratio: Option<RawRatio>,
}

/// A count, or the literal `"off"` — compaction §9's spelling for a route a deployment does not
/// want.
///
/// **Untagged, and the string arm is not a free-text field**: anything other than `"off"` is
/// refused at startup ([`ConfigError::CompactionThresholdNotANumberOrOff`]) rather than read as
/// zero, because a typo that disables a maintenance route silently is the failure this whole file
/// is built to avoid.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum RawThreshold {
    Count(u64),
    Word(String),
}

/// A ratio, or the literal `"off"` — [`RawThreshold`]'s shape for the two gauges that are
/// fractions rather than counts.
///
/// A separate type rather than widening `RawThreshold`, because the two are refused for different
/// reasons and an operator reading the error should be told which: a count that is negative is a
/// typo, and a ratio that is negative is a route that could never decline.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum RawRatio {
    Ratio(f64),
    Word(String),
}

/// SA §7's `[build]` section: the corpus declaration this deployment builds from.
///
/// **The output path is not here** — it is `bundle.path`, which the server also reads. One value,
/// two sides: a build writes where the server opens.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    #[serde(default)]
    schema: Option<PathBuf>,
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

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServe {
    #[serde(default)]
    viewer: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    control: Option<String>,
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
    max_category_values: Option<usize>,
    #[serde(default)]
    max_suggestions: Option<usize>,
    #[serde(default)]
    max_suggestion_walk: Option<u64>,
    #[serde(default)]
    max_suggest_set_entities: Option<u64>,
    #[serde(default)]
    max_shape_vertices: Option<u64>,
    #[serde(default)]
    max_region_vertices: Option<u64>,
    #[serde(default)]
    max_region_cells: Option<usize>,
    #[serde(default)]
    max_browse_rows: Option<usize>,
    #[serde(default)]
    region_cache_bytes: Option<u64>,
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
    single_flight_wait_ms: Option<u64>,
    #[serde(default)]
    stream_flush_bytes: Option<usize>,
    #[serde(default)]
    stream_write_stall_ms: Option<u64>,
    #[serde(default)]
    stream_deadline_ms: Option<u64>,
    #[serde(default)]
    row_projection_cache_bytes: Option<u64>,
    #[serde(default)]
    masked_count_cache_bytes: Option<u64>,
    #[serde(default)]
    occupancy_cache_bytes: Option<u64>,
    #[serde(default)]
    fragment_cache_bytes: Option<u64>,
    #[serde(default)]
    expected_concurrent_sessions: Option<usize>,
    #[serde(default)]
    segment_floor_bytes: Option<u64>,
    #[serde(default)]
    max_merged_segment_bytes: Option<u64>,
    #[serde(default)]
    tier_width: Option<usize>,
    #[serde(default)]
    coalesce_width: Option<usize>,
    /// Browser origins permitted to call the viewer and session planes.
    ///
    /// **Absent means no CORS layer at all**, which is the only sensible default for a key whose
    /// effect is to let a page from another origin present a session token and the session
    /// credential. There is deliberately no environment variable and no wildcard: this is a
    /// development affordance, and the enumerated list is what keeps it from becoming an
    /// integration pattern. See [`crate::cors`].
    #[serde(default)]
    dev_cors_origins: Option<Vec<String>>,
    /// Browser origins permitted to call the **viewer plane** in a deployment.
    ///
    /// The production half of the pair, and it stops at the viewer plane: `/session/authorise` is
    /// gated by the session credential, which a browser must never hold, so no origin list opens
    /// it (decision 0102). Absent means no layer, and a wildcard is refused — an origin list is a
    /// deployment's statement about which pages may present its tokens, and `*` is not a
    /// statement. Unlike `dev_cors_origins` this is silent at startup. See [`crate::cors`].
    #[serde(default)]
    cors_origins: Option<Vec<String>>,
    /// Admit any page served from a loopback address on the **viewer plane**.
    ///
    /// The origin of a notebook front end is a port the kernel chose, so no operator can
    /// enumerate it (`python-sdk.md` §7) and `cors_origins` cannot state it. This key states the
    /// set instead: `http` or `https` on `localhost`, `127.0.0.1` or `[::1]`, any port. It is a
    /// disclosure control and absent by default; what it admits is a page that may present a
    /// **token**, on decision 0102's argument, and never one that may present the session
    /// credential. See [`crate::cors`].
    #[serde(default)]
    cors_loopback: Option<bool>,
    /// The longest a `wait=visible` write acknowledgement is held for its publication.
    ///
    /// Tuning, not a disclosure control: the wait changes when an answer is sent and nothing
    /// about what it contains. Past the bound the route answers as it would have without the
    /// wait, saying `visible: false`, so the ceiling costs a caller a poll of `/control/status`
    /// and never an error.
    #[serde(default)]
    visible_wait_max_secs: Option<u64>,
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
    /// `bundle.path`. **The build's output and the server's input**, declared once: `tessera
    /// build --out` overrides it, and `tessera serve` opens exactly what the last build wrote.
    pub bundle_path: PathBuf,
    pub cache_dir: PathBuf,
    pub wal_path: PathBuf,
    /// `build.schema` — the corpus declaration (`configuration.md` §1), defaulting to
    /// `schema.toml` beside this file. Read by `tessera build`; the server takes its schema from
    /// the bundle's `MANIFEST.json` and never opens this (`configuration.md` §4).
    pub schema_path: PathBuf,
    /// `identity.env` — the **name** of the environment variable carrying the identity key,
    /// defaulting to `TESSERA_IDENTITY_KEY`. Never the key.
    pub identity_env: String,
    pub token_max_lifetime_secs: u64,
    /// `None` where the deployment declares no `[serve]` addresses — legal, because `build` reads
    /// this file too. `serve` refuses rather than defaulting.
    pub viewer_addr: Option<SocketAddr>,
    pub session_addr: Option<SocketAddr>,
    pub control_listen: Option<ControlListen>,
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
    /// The most values `/v1/categories/{column}` returns in one response — the page size ceiling,
    /// and the default page size when a caller names none.
    ///
    /// **A performance knob, so it defaults** (SA §7). It bounds a response, not a disclosure:
    /// what a principal may be *told* is `visibility`'s question and is settled before paging starts.
    pub max_category_values: usize,
    /// `/v1/categories/{column}/suggest`'s page ceiling and `limit`'s default
    /// (`value-suggestion.md` §5.3). Published in `/v1/meta`'s `selection` block on
    /// `max_category_values`' own argument: a client must be able to tell a short list that means
    /// *that is all* from one the deployment truncated, which `more` alone does not.
    ///
    /// **A performance knob, so it defaults.** What a principal may be *told* is `visibility`'s
    /// question, settled before the page is cut.
    pub max_suggestions: usize,
    /// The walk budget `Engine::suggest` spends before it stops and answers `more: true`
    /// (`value-suggestion.md` §5.3, §6.2). A client that receives `more` on an unfilled page reads
    /// it as this deployment constant rather than as its own arithmetic being wrong.
    ///
    /// **Bounds latency, not disclosure**: the enumeration walks the whole vocabulary unbudgeted,
    /// and this bound exists only because a suggestion is per keystroke. A performance knob, so it
    /// defaults, identical for every principal.
    pub max_suggestion_walk: u64,
    /// The composed cardinality at or under which the suggestion verb builds this session a set of
    /// its visible values and answers from it (`value-suggestion.md` §6.3, decision 0124). A wider
    /// viewer keeps the probe route, as does every keystroke before the set lands and every column
    /// with nothing to sweep.
    ///
    /// **A performance knob, so it defaults**, and a deployment constant identical for every
    /// principal: the quantity it is compared against is the caller's own composed cardinality,
    /// which a zoom-0 viewport already returns exactly as `visible`.
    pub max_suggest_set_entities: u64,
    /// The most vertices a published polygon may carry after canonicalisation
    /// (`polygon-membership.md` §9, ruling (e)): over it, `PUT /control/layers/{name}/artifacts`
    /// is a `422` naming the count and the cap. Published on `/v1/meta`. The held decomposition
    /// is reported and never capped; this bounds the one input a caller can simplify.
    pub max_shape_vertices: u64,
    /// The most vertices a `region` filter leaf's polygon may carry (selection-operand §2): over
    /// it the request is a `422` naming the count and the cap. Published on `/v1/meta`'s
    /// `selection` block. A vertex count is the caller's own arithmetic, which is why this one
    /// refuses where `max_region_cells` does not.
    pub max_region_vertices: u64,
    /// The most boundary cells a `region` leaf's decomposition may hold at one depth
    /// (selection-operand §6). **Not a refusal**: over it the descent stops at the deepest depth
    /// that fits and the answer is a cover, said on `x-tessera-region`. Published on `/v1/meta`.
    pub max_region_cells: usize,
    /// The most rows `POST /v1/artifacts/browse` returns in one page — the page-size ceiling, and
    /// the default page size when a caller names none (`highlight-and-hierarchy.md` §4).
    ///
    /// **A response bound and not a disclosure control**, exactly as `max_category_values` is:
    /// what a principal may be *told* is the artifact's own existence criterion, settled before
    /// paging starts, and every page's fill counts only artifacts that cleared it. `limit` clamps
    /// to this and `limit = 0` is a `422`. Published on `/v1/meta`'s `selection` block.
    pub max_browse_rows: usize,
    /// The byte bound on the region decomposition cache (`tessera_engine::region`), which is
    /// shared across principals and pruned per generation; a decomposition is a perimeter's worth
    /// of work, so a bound that evicts costs latency and nothing else.
    pub region_cache_bytes: u64,
    /// Emit `x-tessera-stage-ns` on viewport responses. **Fails closed**: absent means false, and
    /// even true does nothing in a binary built without the `bench-timing` feature. The header
    /// carries only durations and row counts — no identifier, no per-principal label (SA §9) —
    /// but it quantifies the C4 timing channel, so it stays off unless a measurement asked for it.
    pub stage_timing: bool,
    /// `serve.dev_cors_origins`. Empty is the default and means no CORS layer is mounted at all —
    /// not a layer that allows nothing. See [`crate::cors`] for why this is a development
    /// affordance rather than an integration feature.
    pub dev_cors_origins: Vec<String>,
    /// `serve.cors_origins` — the production origin list, viewer plane only (decision 0102).
    /// Empty is the default and mounts nothing. Both lists may be set; a duplicate origin across
    /// the two is not an error. See [`crate::cors`].
    pub cors_origins: Vec<String>,
    /// `serve.visible_wait_max_secs` (contracts §3.4): the ceiling on a `wait=visible` wait,
    /// 30 seconds by default. Zero means a route answers without waiting at all, which reports
    /// `visible: false` on every write whose cycle has not already published.
    pub visible_wait_max_secs: u64,
    /// `serve.cors_loopback`, viewer plane only and `false` by default. A page served from
    /// `localhost`, `127.0.0.1` or `[::1]` on any port is admitted as a listed origin is. See
    /// [`crate::cors`].
    pub cors_loopback: bool,
    /// **Where** the session bearer secret comes from, not what it is. Read at
    /// [`crate::prepare`], never at parse: `tessera build` reads this same file and has no
    /// business requiring a serving secret to be exported before it will write a bundle. What
    /// parse still refuses is a credential *declared nowhere* — the fail-closed half, and the one
    /// that needs no secret to check.
    pub session_credential: Credential,
    pub operator_credential: Credential,
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
    /// How long a request parks on **another request's in-flight row-projection build** before it
    /// is shed (decision 0058).
    ///
    /// **A different wait from [`Self::admission_timeout_ms`], and sized from a different
    /// quantity.** That one bounds queueing for a compute permit and is 250 ms; this one bounds
    /// waiting for work that is already running, so it is argued from the build's own measured
    /// cost — see `tessera_engine`'s `DEFAULT_WAIT_BUDGET_MS`. A parked request holds its compute
    /// permit throughout, which is the occupancy decision 0059 bounds with this key rather than
    /// with a per-principal cap.
    pub single_flight_wait_ms: u64,
    /// The streamed viewport's flush threshold: a points frame is handed to the wire once its
    /// estimated payload reaches this, always at a whole-tile boundary
    /// (`streamed-serving.md` §2). Size-based only — there is deliberately no time-based flush,
    /// which would make response bytes nondeterministic. See [`DEFAULT_STREAM_FLUSH_BYTES`].
    pub stream_flush_bytes: usize,
    /// How long one frame send may wait on a client that has stopped reading before the stream
    /// is aborted. See [`DEFAULT_STREAM_WRITE_STALL_MS`], and `stream_deadline_ms` for the
    /// whole-stream bound a per-send deadline alone cannot give.
    pub stream_write_stall_ms: u64,
    /// The whole emit phase's wall budget, from first flush. Bounds a slot's occupancy at
    /// `slots x deadline` absolutely — the dripping-reader shape a per-send stall deadline
    /// admits (`streamed-serving.md` §5; owner-ruled, decision 0060 — raising it is choosing a
    /// larger parkable admission surface). See [`DEFAULT_STREAM_DEADLINE_MS`].
    pub stream_deadline_ms: u64,

    // ---- The write-path and admission knobs. The argument for each default is on its
    // `DEFAULT_*` constant.
    /// The **row** count at which a commit window closes (an *item* is a row; entries are admitted
    /// whole, so a window closes at or just past this).
    /// See [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`]. `1` is the honest way to disable group commit.
    pub commit_window_max_items: usize,
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
    /// Per-request body-byte cap on `PUT` and `PATCH /control/layers/{name}/artifacts`; over is
    /// 422. See [`DEFAULT_PUBLISH_MAX_BODY_BYTES`]. Published on `/control/status`'s `limits`.
    pub publish_max_body_bytes: usize,
    /// Artifact records per `PUT /control/layers/{name}/artifacts`; over is 422. See
    /// [`DEFAULT_MAX_ARTIFACTS_PER_REQUEST`].
    pub max_artifacts_per_request: usize,
    /// Members per `PATCH /control/layers/{name}/artifacts`, summed over the page; over is 422.
    /// See [`DEFAULT_MAX_MEMBERS_PER_REQUEST`].
    pub max_members_per_request: usize,
    /// The bound an exclusion list is admissible under (ingest §2.3). Published; not enforced,
    /// since the field it bounds is not built. See [`DEFAULT_MAX_EXCLUDED_PER_REQUEST`].
    pub max_excluded_per_request: usize,
    /// The WAL's byte ceiling *as a startup relation between config values*, and the right-hand
    /// side of the headroom assertion. **Not a runtime ceiling: appends do not stop here** — `Wal`
    /// has no length accessor, so nothing compares the live log against this number. See
    /// [`DEFAULT_WAL_HARD_LIMIT_BYTES`] for what would be needed to make the name true.
    pub wal_hard_limit_bytes: u64,
    /// Overlay depth at which an alarm is raised. See [`DEFAULT_OVERLAY_SOFT_LIMIT`].
    ///
    /// **The alarm is on total depth and the fold trigger is not** (compaction §9): this counts
    /// `deleted ∪ suppressed`, which is the right thing for an operator to see, while
    /// [`Config::compaction`]'s route keys on the deletions alone — a suppression never retires, so
    /// a trigger reading this number would dispatch a no-op fold for ever on a deployment holding
    /// standing suppressions.
    pub overlay_soft_limit: usize,
    /// When a fold is dispatched with nobody asking for one — compaction §9's automatic trigger as
    /// decision 0056 rules it, parsed from the four `ingest.compaction_*` keys.
    pub compaction: tessera_engine::CompactionSchedule,
    /// The flush tick: the period at which geometry is published, and therefore the bound on how
    /// stale an acknowledged item's absence may be. See [`DEFAULT_FLUSH_MAX_AGE_SECS`].
    pub flush_max_age_secs: u64,
    /// Buffered rows at which the flush tick comes due ahead of its period (§4.1). See
    /// [`DEFAULT_FLUSH_MAX_ITEMS`].
    pub flush_max_items: usize,
    /// Buffer occupancy at which `/control/ingest` is refused with a 429 (§1.3).
    ///
    /// A **distinct knob** from [`Config::ingest_queue_bound`], which bounds queued *commands*:
    /// the executor drains a job into the buffer in milliseconds, so no ingest rate produces a 429
    /// by buffer size through that one. See [`DEFAULT_INGEST_BUFFER_MAX_ITEMS`].
    pub ingest_buffer_max_items: usize,
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
    ///
    /// Refused below 2 ([`ConfigError::SelectionWidthBelowTwo`]): selection needs a window of at
    /// least two segments, so 1 is "never merge", silently. This key and
    /// [`Config::segment_floor_bytes`] were parsed here and reached the engine nowhere until
    /// 2026-08-15 — the merge policy hard-coded 4 and 16 MiB — and are kept, wired, against
    /// decision 0045's delete-by-default because the correctness suite drives merge eligibility
    /// through them (correctness-suite §12.3).
    pub tier_width: usize,
    /// Same-tier entries, per entity-space axis, before a coalesce is selected. See
    /// [`DEFAULT_COALESCE_WIDTH`]. Same below-2 refusal as [`Config::tier_width`], for the same
    /// silent-non-run reason; unlike that key this one is new — the width had no key at all.
    pub coalesce_width: usize,
    /// Byte bound on the row-projection cache. See [`DEFAULT_ROW_PROJECTION_CACHE_BYTES`], and
    /// [`MEASURED_PROJECTION_BYTES_AT_1E9`] for the per-entry size the startup validation weighs it
    /// against.
    pub row_projection_cache_bytes: u64,
    /// Byte bound on the masked-count cache — the per-`(session, layer, level)` histograms a
    /// **row-major** layer's counts come from. See [`DEFAULT_MASKED_COUNT_CACHE_BYTES`].
    pub masked_count_cache_bytes: u64,
    /// Byte bound on the occupancy memo — θ's `N_occ` ladder, one rung per
    /// `(session, view, depth, generation)`. See
    /// [`tessera_engine::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES`], which argues the figure and
    /// says what a deployment with many concurrent sessions should raise it to.
    pub occupancy_cache_bytes: u64,
    /// Byte bound on the *in-memory* fragment tier. See [`DEFAULT_FRAGMENT_CACHE_BYTES`]. The
    /// `.frag` sidecar tier is untouched by it.
    pub fragment_cache_bytes: u64,
    /// The concurrency the projection cache must not collapse at; the startup validation is that
    /// [`Config::row_projection_cache_bytes`] admits at least this many entries. See
    /// [`DEFAULT_EXPECTED_CONCURRENT_SESSIONS`].
    pub expected_concurrent_sessions: usize,
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
/// `serve.visible_wait_max_secs`. Long enough that a page and the tick it pulls forward complete
/// on a loaded machine, short enough that a caller who set the parameter by mistake is not held
/// for a tick period.
const DEFAULT_VISIBLE_WAIT_MAX_SECS: u64 = 30;

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

/// One page of a category vocabulary.
///
/// Sized against the two vocabularies that exist: arXiv's `archive` has 8 values and
/// `primary_category` 171, so a legend for either arrives in one request and the cursor is unused.
/// It bounds the pathological case instead — a `u16` vocabulary that has minted its way to tens of
/// thousands of values, where an unpaged response is megabytes against a measured 79 KB viewport
/// response and, being per-principal, shares no cache with anyone.
const DEFAULT_MAX_CATEGORY_VALUES: usize = 1_000;

/// `/v1/categories/{column}/suggest`'s page ceiling and default (`value-suggestion.md` §5.3,
/// contracts §3.2's r72). The owner's recommended default: a typeahead page, not a legend.
const DEFAULT_MAX_SUGGESTIONS: usize = 20;

/// The suggestion walk's budget (`value-suggestion.md` §5.3, §6.2). The owner's recommended
/// default — measured (`probes/2026-09-02-value-suggestion/`) as the smallest budget that fills
/// the sparsest measured viewer's page on a one-character prefix at 10⁷ values.
const DEFAULT_MAX_SUGGESTION_WALK: u64 = 100_000;

/// The composed cardinality at or under which a suggestion is answered from a per-session set of
/// visible values rather than by probing a posting per value walked (`value-suggestion.md` §6.3,
/// [decision 0124](../../../docs/decisions/0124-the-suggestion-route-may-follow-the-viewers-cardinality.md)).
///
/// The owner's recommended default: 10⁷ is the *measured* 46–61 ms point for the sweep the set is
/// built by (`probes/2026-09-02-value-suggestion/` arm 3), and a wider viewer stays on the probe
/// route, which fills that viewer's page in under a millisecond anyway. A deployment constant,
/// identical for every principal — which is what makes a route keyed on the caller's own
/// cardinality admissible under §8.2 at all.
const DEFAULT_MAX_SUGGEST_SET_ENTITIES: u64 = 10_000_000;

/// One page of a layer's hierarchy (`highlight-and-hierarchy.md` §4).
///
/// Sized against the panel that reads it: a tree row is a name, a count and an expander, and a
/// hundred of them is more than fits a column at any zoom. Rung 3's MeSH DAG has 30,217
/// descriptors and 16 top-level roots, so a root page and a typical expansion each arrive in one
/// request; what this bounds is the pathological expansion — a node with thousands of children —
/// where an unpaged answer is a scroll nobody reads and a response nobody shares, being
/// per-principal.
const DEFAULT_MAX_BROWSE_ROWS: usize = 200;

/// A `region` leaf's vertex cap. A lasso is drawn with a mouse at one vertex per pointer event, so
/// a few hundred is an elaborate one; ten thousand leaves room for a client that hands over a
/// polygon it holds rather than one it drew, and stays well inside what the descent's per-edge
/// cost makes a millisecond's work. The publication cap (`max_shape_vertices`) is two orders
/// larger because a held shape pays its decomposition once.
const DEFAULT_MAX_REGION_VERTICES: u64 = 10_000;

/// The region decomposition cache's bound. A whole-world box at the cell budget is a few
/// megabytes of ranges and contexts; this holds dozens of such shapes, and an ordinary lasso is
/// kilobytes.
const DEFAULT_REGION_CACHE_BYTES: u64 = 256 * 1024 * 1024;

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

/// The client's asked-for flush cadence is ~1–2 MB (the streaming handover memo, criterion 3);
/// 1 MiB is its low end, favouring time-to-next-paint over per-frame overhead, which at ~5 header
/// bytes plus a few hundred bytes of repeated Arrow schema per frame is noise against the payload.
const DEFAULT_STREAM_FLUSH_BYTES: usize = 1 << 20;

/// Ten seconds of a full channel before one send gives up. Generous against any healthy reader —
/// the channel holds ~2 flushes, so a reader consuming a megabyte every ten seconds stays under
/// it — while bounding what a stopped reader can hold. Modelled, not measured; the whole-stream
/// deadline below is the bound that actually caps occupancy.
const DEFAULT_STREAM_WRITE_STALL_MS: u64 = 10_000;

/// Sixty seconds for the whole emit phase, from first flush. At the measured op point (a 42 MB
/// heaviest arrival) this admits a reader as slow as ~0.7 MB/s before cutting it — well below any
/// deployment link this pre-release system has — and caps one slot's occupancy at a minute
/// (`streamed-serving.md` §5; owner-ruled — decision 0060).
const DEFAULT_STREAM_DEADLINE_MS: u64 = 60_000;

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

// `commit_window_max_age_ms` is deleted, not inert (decision 0045). There is no linger for an
// age bound to cap — a window closes on its row bound or on the work queue observed empty, and
// none survives the executor's blocking point — so the key had no possible consumer. The full
// no-linger argument lives at `Executor::run_work_pass` and decision 0034; anything that builds
// a linger re-adds the key *with* its consumer, in one commit.

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
///   handlers accumulate somewhere that holds no queue slot — a slow `terms_of_labels`, slow sidecar
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
/// - Arrow decode, the plugin's `terms_of_labels` loop, `resolve_terms` and the external-ID sidecar
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

/// Per-request body-byte cap on `PUT` and `PATCH /control/layers/{name}/artifacts`; over is 422
/// (ingest §2.1). A pagination unit, never a ceiling on reach: an artifact's membership travels
/// in as many growth pages as it needs (decision 0127), and a publication carries a first page.
/// 64 MiB carries about 4.4 million base64 external ids a page (measured on rung 3, 2026-09-05).
const DEFAULT_PUBLISH_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Artifact records per `PUT /control/layers/{name}/artifacts`; over is 422 (ingest §2.1). The
/// same shape as the row cap: a page's cost on the executor is linear in its records, and the
/// count bounds one executor step where the bytes bound one connection.
const DEFAULT_MAX_ARTIFACTS_PER_REQUEST: usize = 10_000;

/// Members per `PATCH /control/layers/{name}/artifacts`, summed over the page's artifacts; over
/// is 422 (ingest §2.1). Set above what the default byte cap admits (about 4.4 million ids at
/// 15 bytes each), so at the defaults the byte cap is the one a page meets first.
const DEFAULT_MAX_MEMBERS_PER_REQUEST: usize = 5_000_000;

/// Entities an exclusion list may name in one request (ingest §2.3): published on
/// `/control/status`. Not built yet: the `excluding` field does not exist on the publication
/// route, so nothing is enforced against it; a client reads it as the bound the field will take.
const DEFAULT_MAX_EXCLUDED_PER_REQUEST: usize = 1_000_000;

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
/// ceiling and is not one: nothing compares the live WAL against this number. It is consumed in
/// exactly one place — the startup assertion that the queue's worst case plus reserved deny
/// headroom sits strictly below it — and past that point the WAL grows until the filesystem
/// refuses, at which point `WalError::Poisoned` makes the handle dead.
///
/// **The live log's size is readable and is not read here.** `Wal::disc_bytes` exists and
/// `/control/status` publishes it beside the member count and the rotation bound, which is the
/// gauge half of the question. What is still missing is the ruling: what a node at the limit
/// should do. Refusing ingest is straightforward; refusing a *deny* is fail-open, and the deny
/// lane is the one that has nowhere else to go. So the accessor is a gauge and this key acts on
/// nothing.
const DEFAULT_WAL_HARD_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Overlay depth at which an alarm is raised. **SA §7's own default**, carried across unchanged.
///
/// **It alarms, and it is now also the default trigger for the thing that acts.** The alarm counts
/// `deleted ∪ suppressed`, which is the right thing for an operator to see; the fold trigger it
/// seeds — `ingest.compaction_after_deletions` — counts the deletions alone, because a suppression
/// never retires and a fold keyed on the union would rewrite the corpus to retire nothing on any
/// deployment holding standing suppressions (compaction §9; r3, memory F5). The depth matters
/// beyond memory: every deny acceptance clones the overlay inside the WAL critical section, so
/// overlay depth is a term in deny-ack latency.
const DEFAULT_OVERLAY_SOFT_LIMIT: usize = 500_000;

/// **The flush's row trigger**: buffered rows at which the tick comes due ahead of its period.
///
/// **40,000 — four commit windows, and it is measured rather than assumed.**
/// `docs/evidence/memos/2026-08-05-ingest-rate.md` sweeps `B/W`, rows buffered between
/// publications over rows per window, and finds an *interior* optimum: including the flush,
/// `B/W = 4` is at or next to the best at every term density, `B/W = 1` is 30–42% worse (the
/// flush's fixed cost over too few rows) and `B/W = 24` is 20–36% worse (the window close's
/// `O(B)` buffer copy, which the `B²/2W` law makes quadratic over an interval). Four windows at
/// [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`] is that optimum.
///
/// **This is the knob that was missing, not a second cadence.** Under
/// [`DEFAULT_FLUSH_MAX_AGE_SECS`] alone, `B` is the arrival rate times the tick — a loader
/// sustaining 380,000 rows/s across a 90 s tick buffers ~34,000,000 rows, `B/W ≈ 3,400`, a
/// hundred times the right-hand end of that table — so the age tick sets `B` by arithmetic
/// nobody chose. A publication satisfies both triggers and restarts the period, so a fast loader
/// publishes on rows and never reaches the age, and a slow one publishes on age and never
/// reaches the rows.
///
/// **It does not replace [`DEFAULT_INGEST_BUFFER_MAX_ITEMS`]**, which is the occupancy at which
/// ingest is *refused*. This one publishes; that one sheds.
///
/// (Decision 0045 deleted this key for having no reader. The reader is
/// `Executor::tick_if_due`'s row trip, added 2026-09-04.)
const DEFAULT_FLUSH_MAX_ITEMS: usize = 4 * DEFAULT_COMMIT_WINDOW_MAX_ITEMS;

/// **The flush tick**, and therefore the bound on how stale an acknowledged item's absence may be:
/// a buffered item contributes to no viewport, count or density until a flush gives it a row. A
/// visibility-latency control, not merely a segment-count one.
///
/// **90, and it is no longer a validated floor.** It was raised from SA §7's 60 to clear the 75 s
/// the pin TTL and drain depth imposed, and that relation went with the pin retention
/// (`geometry-pinning.md` §0). Nothing now refuses a shorter tick at startup.
///
/// **What does bound it is the row-projection cache, and it is a cost rather than a refusal.**
/// Every publication rotates `RowProjectionKey`'s `segments_version`, so every live session's
/// projection has to be brought forward at the next request — a patch where the superseded entry
/// is still resident (`KEEP_SUPERSEDED_GENERATIONS`), a full rebuild otherwise, and a rebuild is a
/// *measured* 1 277 ms at 10⁹. A tick shorter than a deployment's patch cost synchronises that work
/// across the whole session population at every tick. 90 is kept because it is the number this
/// deployment has run at, not because anything now forces it.
const DEFAULT_FLUSH_MAX_AGE_SECS: u64 = 90;

/// Compaction §9's floor, under both trigger routes: at most one fold a day.
///
/// It is also what keeps the daily window to one firing without any "did I fire today" state — the
/// window is hours wide and the floor is a day, so the arithmetic does the bookkeeping.
const DEFAULT_COMPACTION_MIN_INTERVAL_SECS: u64 = 86_400;

/// Midnight **UTC**, per decision 0056. Not local time: a local window shifts by an hour twice a
/// year and on the transition day fires twice or not at all, for the most expensive operation in
/// the system.
const DEFAULT_COMPACTION_WINDOW_START_SECS: u32 = 0;

/// Four hours. **Assumed, not sized** (compaction §14): long enough that a node restarting inside
/// the quiet period still folds, short enough that one down all night does not start at breakfast.
const DEFAULT_COMPACTION_WINDOW_SECS: u32 = 4 * 3_600;

/// Eight live segments in any one view. **Assumed, not sized** (compaction §14): decision 0049
/// measured ~73 ms on a 300-tile viewport at ~152 segments against a 135–164 ms baseline, so this
/// is where a fold begins to be worth its flip cost and is otherwise a guess. Probe P1 calibrates
/// it.
const DEFAULT_COMPACTION_WINDOW_MIN_SEGMENTS: usize = 8;

/// Sixty-four live segments in any one view, **at any hour** — compaction §9's own default, and
/// the ceiling above which deferring segment growth to the next window costs more than folding now.
///
/// **The one threshold here with a measurement behind it, though not at this value**: decision 0049
/// measured ~73 ms on a 300-tile viewport at ~152 segments against a 135–164 ms baseline, so the
/// regression is real and roughly linear in segment count. 64 is where §9 drew the line between
/// "gradual, and can wait for a quiet hour" and "every viewer is paying for this now"; the
/// interpolation is a judgement and probe P1 is what would replace it.
///
/// It must sit strictly above [`DEFAULT_COMPACTION_WINDOW_MIN_SEGMENTS`], and a configuration that
/// inverts the two is refused — see [`ConfigError::CompactionSegmentThresholdsInverted`].
const DEFAULT_COMPACTION_MAX_SEGMENTS: usize = 64;

/// Tombstoned rows as a fraction of live rows at which a fold is dispatched (compaction §9).
///
/// **0.2, and it is assumed rather than sized.** A fifth of every viewport's scanned rows being
/// invisible to every viewer is obviously not-yet-urgent and obviously not fine; where between
/// those a deployment wants the line is a judgement, and probe P1 does not settle it — P1 measures
/// what a fold *costs*, and this is about when the saving is worth paying for.
const DEFAULT_COMPACTION_DEAD_ROWS_FRACTION: f64 = 0.2;

/// On-disc bytes over manifest-named bytes at which a fold is dispatched (compaction §9).
///
/// **1.0 — paying double for storage — and it sits below the measured no-compaction steady state
/// of 2.0–2.6×** (`docs/evidence/memos/2026-08-05-write-path-at-scale.md`), which is what makes it
/// a threshold a real deployment crosses rather than one it lives above. Assumed on the same
/// footing as the fraction above.
const DEFAULT_COMPACTION_DEAD_BYTES_RATIO: f64 = 1.0;

/// Buffer occupancy at which `/control/ingest` is refused (§1.3).
///
/// **`ingest_queue_bound` does not bound this.** That one bounds the *command queue* — 32 jobs by
/// default — and the executor drains a job into the buffer in milliseconds, so nothing in the
/// system compared buffer occupancy to anything and no ingest rate produced a 429 by buffer size.
/// Between ticks the buffer is what grows, and this is the bound on it.
///
/// Sized well above anything a healthy tick leaves behind: what this bounds is the pathological
/// case — repeated flush failure — where the buffer grows without a flush to drain it, and 429
/// is the intended backpressure (§10).
const DEFAULT_INGEST_BUFFER_MAX_ITEMS: usize = 1_000_000;

/// Below this, segments compare equal for merge selection (§5.1), so a tail of tiny segments does
/// not dominate it.
const DEFAULT_SEGMENT_FLOOR_BYTES: u64 = 16 * 1024 * 1024;

/// Segments in a tier before a merge is selected (§5.1).
const DEFAULT_TIER_WIDTH: usize = 4;

/// Same-tier entries, per axis, before an entity-space coalesce is selected (write-path §7).
///
/// The engine's own default (`tessera_engine`'s `CoalescePolicy::default` carries the argument:
/// the same shape as `tier_width` and a little wider, because an entity-space pass costs no
/// projection rebuild and can afford to run less often per byte moved). Restated here rather than
/// imported because the server may not depend on the store's vocabulary, and a default that
/// silently tracked a library's would move a deployment's behaviour without a config change.
const DEFAULT_COALESCE_WIDTH: usize = 8;

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
///    a `Vec<u16>` of values carries capacity slack that the serialised form does not. A mask
///    dominated by **bitmap** containers has an in-memory size that *is* its serialised size
///    (8 KB, a fixed 2¹⁶-bit block) — ratio ≈ 1.0 — and 125.12 MB over a 10⁹ row space is that
///    mask. So the factor does not apply at this figure; it applies to sparse,
///    array-container-dominated masks, which are small in absolute terms anyway.
///
///    **This argument does not reach a run-optimised projection, and does not need to.**
///    `tessera_engine`'s `RowProjection::from_rows` run-optimises, so a grant covering runs of row
///    space holds run containers where this argument assumes bitmap ones, and croaring converts a
///    container only where the run form is smaller. The measured figure is therefore an
///    over-estimate of what such an entry is charged rather than a bound that has stopped holding
///    — the error is in the direction that over-provisions the cache, and the validation below
///    stays a floor. A projection that runs badly is charged the 125.12 MB shape this constant
///    describes, which is the case the floor exists for.
/// 2. It is the 10⁹ figure, so a smaller corpus leaves the bounds below over-provisioned rather
///    than wrong.
/// 3. It is **per (session, view, segments_version) entry**, not per session — the cache key's
///    three components (see `tessera_engine`'s `RowProjectionKey`). "Eight sessions, eight
///    entries" holds only while one partition emits one view, which is true today and silently
///    false the moment a build emits two: the same eight sessions then occupy sixteen entries.
pub const MEASURED_PROJECTION_BYTES_AT_1E9: u64 = 125_120_000;

/// Byte bound on the row-projection cache.
///
/// **Sized so the cache cannot collapse at the expected concurrency, because plain LRU does not
/// degrade in this regime — it collapses.** A projection miss is
/// `RowProjection::new`, *measured* at 1 277 ms at 10⁹, so the
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
/// session count: **more than one view per session** (the key is
/// `(token_id, view, segments_version)`, so a two-view bundle doubles the entries at unchanged
/// concurrency), and **a generation swap**, during which a pinned request's old-`segments_version`
/// entry coexists with the new one until `prune_generation` runs at drain-list reclaim. Both are
/// entry-count effects, and at this bound either one alone still fits.
const DEFAULT_ROW_PROJECTION_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Byte bound on the masked-count cache — the histograms a **row-major** layer's counts come from
/// ([decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
/// one named exception, `tessera_engine::histogram`).
///
/// **256 MiB, and the arithmetic is the decision's own.** An entry is ~4 B per artifact — 4 MB at
/// 10⁶ artifacts and 40 MB at 10⁷ — per `(session, layer, level)`. At the 10⁶ figure this admits
/// ~64 entries, which is [`DEFAULT_EXPECTED_CONCURRENT_SESSIONS`]'s eight sessions over eight
/// row-major levels; at 10⁷ it is six, and a deployment there should raise it.
///
/// **An eighth of [`DEFAULT_ROW_PROJECTION_CACHE_BYTES`], deliberately, and the asymmetry is not a
/// margin.** A projection miss is a *measured* 1 277 ms rebuild that every request for that session
/// then waits on; a histogram miss is one walk of a mask that is already resident, and it is paid by
/// the levels that are row-major — which is a property of the corpus, and is **none of them** in a
/// deployment that has never flipped one. Sizing this like its sibling would reserve gigabytes
/// against a structure most deployments never build one of.
///
/// **It is not covered by [`crate::validate_cache_bounds`]**, and that is the same judgement: the
/// relation that validation enforces is `bound >= expected_concurrent_sessions x per_entry`, and
/// `per_entry` here is the *artifact count of a row-major level*, which the config cannot know and
/// which is zero for most deployments. An under-sized bound costs a rebuilt histogram per request,
/// which is slow rather than a 429 storm — so it is reported by the cache's own gauges rather than
/// refused at startup.
const DEFAULT_MASKED_COUNT_CACHE_BYTES: u64 = 256 * 1024 * 1024;

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
/// row-projection margin's two justifications (a view multiplier per session, and a generation
/// swap's transient duplicate) do not apply — a view does not appear in this key at all, and a
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

/// `HH:MM`, 24-hour, to seconds past UTC midnight.
///
/// Strict: exactly two fields, both numeric, hour `< 24` and minute `< 60`. A lenient parser here
/// would accept `"24:00"` or `"0:0"` and answer with a time the operator did not write, for a knob
/// whose whole purpose is *which hour*.
fn parse_time_of_day(value: &str) -> Result<u32> {
    let bad = || ConfigError::CompactionWindowNotATime(value.to_string());
    let (hh, mm) = value.split_once(':').ok_or_else(bad)?;
    if hh.len() != 2 || mm.len() != 2 {
        return Err(bad());
    }
    let hour: u32 = hh.parse().map_err(|_| bad())?;
    let minute: u32 = mm.parse().map_err(|_| bad())?;
    if hour > 23 || minute > 59 {
        return Err(bad());
    }
    Ok(hour * 3_600 + minute * 60)
}

/// A ratio key: absent takes `default`, `"off"` switches the route off, a number must be finite and
/// strictly positive, and anything else is refused by name.
///
/// **Strictly positive rather than merely non-negative.** A threshold of zero is satisfied by every
/// possible measurement, so the route dispatches a fold at every tick that clears the interval floor
/// — the ungated timer compaction §9 declines, reached by configuration rather than by design. A
/// deployment that wants a route to always fire has said something it does not mean; a deployment
/// that wants it off spells that `"off"`.
fn ratio_or_off(key: &'static str, raw: Option<&RawRatio>, default: f64) -> Result<Option<f64>> {
    let value = match raw {
        None => return Ok(Some(default)),
        Some(RawRatio::Word(word)) if word == "off" => return Ok(None),
        Some(RawRatio::Word(word)) => {
            return Err(ConfigError::CompactionRatioNotANumberOrOff {
                key,
                value: word.clone(),
            })
        }
        Some(RawRatio::Ratio(value)) => *value,
    };
    if !value.is_finite() || value <= 0.0 {
        return Err(ConfigError::CompactionRatioNotPositive { key, value });
    }
    Ok(Some(value))
}

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

/// The two selection widths (`serve.tier_width`, `serve.coalesce_width`) refuse anything below 2,
/// not merely zero: selection needs a window of at least two entries, so 1 is as silently
/// pass-disabling as 0 and [`non_zero_usize`] would wave it through. See
/// [`ConfigError::SelectionWidthBelowTwo`].
fn selection_width(key: &'static str, width: usize) -> Result<usize> {
    if width < tessera_engine::MIN_SELECTION_WIDTH {
        return Err(ConfigError::SelectionWidthBelowTwo { key, width });
    }
    Ok(width)
}

/// The file name every deployment's configuration is found under.
pub const DEPLOYMENT_FILE: &str = "tessera.toml";

/// The environment variable carrying the identity key when `[identity]` names none.
pub const DEFAULT_IDENTITY_ENV: &str = "TESSERA_IDENTITY_KEY";

/// The corpus declaration when `[build]` names none.
pub const DEFAULT_SCHEMA_FILE: &str = "schema.toml";

/// Find this deployment's `tessera.toml`: `explicit` if given, else the nearest one at or above
/// `from` (`configuration.md` §3).
///
/// **Walking up, as `Cargo.toml` is found**, so both verbs work from anywhere under the project
/// root and neither needs a flag in the ordinary case. The search stops at the first hit rather
/// than merging what it finds on the way: a deployment is one file, and a partial one further up
/// silently supplying half the paths is exactly the guessing this file exists to prevent.
pub fn discover(explicit: Option<&Path>, from: &Path) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let mut dir = Some(from);
    while let Some(here) = dir {
        let candidate = here.join(DEPLOYMENT_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = here.parent();
    }
    Err(ConfigError::NoDeploymentConfig {
        from: from.to_path_buf(),
    })
}

pub fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)?;
    let mut config = parse(&text)?;
    // **Every path resolves against this file's own directory** (`configuration.md` §3). Done
    // here rather than in `parse` so the parse stays a pure function of the text — which is what
    // the several dozen cases below rely on — and so there is exactly one place that knows where
    // the document came from.
    let base = path.parent().unwrap_or(Path::new(""));
    for slot in [
        Some(&mut config.bundle_path),
        Some(&mut config.cache_dir),
        Some(&mut config.wal_path),
        Some(&mut config.schema_path),
        config.session_credential.file.as_mut(),
        config.operator_credential.file.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        if slot.is_relative() {
            *slot = base.join(&*slot);
        }
    }
    Ok(config)
}

fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;

    if raw.plugin.module != "builtin:passthrough" {
        return Err(ConfigError::UnsupportedPlugin(raw.plugin.module));
    }

    let token_max_lifetime_secs = raw
        .disclosure
        .ok_or(ConfigError::MissingDisclosureSection)?
        .token_max_lifetime
        .ok_or(ConfigError::MissingDisclosureKey("token_max_lifetime"))?;

    // `[identity]` names the variable, never the key. Absent means the default, which is what
    // makes the section omissible in the ordinary deployment.
    let identity_env = match &raw.identity {
        None => DEFAULT_IDENTITY_ENV.to_string(),
        Some(value) => {
            let table = value.as_table().ok_or(ConfigError::IdentityNotATable)?;
            for key in table.keys() {
                match key.as_str() {
                    "env" => {}
                    "key" => return Err(ConfigError::IdentityKeyInline),
                    other => return Err(ConfigError::UnknownIdentityKey(other.to_string())),
                }
            }
            match table.get("env").and_then(toml::Value::as_str) {
                Some(name) if !name.trim().is_empty() => name.to_string(),
                _ => DEFAULT_IDENTITY_ENV.to_string(),
            }
        }
    };
    let schema_path = raw
        .build
        .schema
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SCHEMA_FILE));

    // **`[serve]` is optional, because `tessera build` reads this same file.** A project that only
    // builds a bundle has no listen addresses to state, and requiring three ports before it could
    // write one would be a serving concern refusing a build. Absent here is not absent at startup:
    // `serve` refuses a deployment that declares no addresses, naming them (`lib.rs`'s `prepare`),
    // so nothing comes up listening on a default nobody chose.
    let viewer_addr: Option<SocketAddr> = raw
        .serve
        .viewer
        .as_deref()
        .map(|a| a.parse().map_err(|_| ConfigError::BadAddr(a.to_string())))
        .transpose()?;
    let session_addr: Option<SocketAddr> = raw
        .serve
        .session
        .as_deref()
        .map(|a| a.parse().map_err(|_| ConfigError::BadAddr(a.to_string())))
        .transpose()?;
    let control_listen = raw
        .serve
        .control
        .as_deref()
        .map(parse_control_listen)
        .transpose()?;

    let session_credential = Credential::declared(
        raw.serve.session_credential_file,
        raw.serve.session_credential_env,
    );
    let operator_credential = Credential::declared(
        raw.serve.operator_credential_file,
        raw.serve.operator_credential_env,
    );

    // §7.2's clause parameters. Every check here refuses rather than clamps: a typo must not
    // silently disable an invariant (the floor) or silently blank the density signal (theta).
    let k_min = raw.serve.k_min.unwrap_or(DEFAULT_K_MIN);
    let k_max_marks = raw.serve.k_max_marks.unwrap_or(DEFAULT_K_MAX_MARKS);
    let max_k = raw.serve.max_k.unwrap_or(DEFAULT_MAX_K);
    let theta_target_marks = raw
        .serve
        .theta_target_marks
        .unwrap_or(DEFAULT_THETA_TARGET_MARKS);
    if k_min < tessera_engine::MIN_K_MIN {
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

    // Both CORS lists are enumerated or absent (decision 0102), so a wildcard is refused here
    // rather than dropped downstream: dropping it would leave an operator who asked for open CORS
    // with a *working* server and no CORS, which is a worse answer than a refusal naming the key.
    let dev_cors_origins = raw.serve.dev_cors_origins.unwrap_or_default();
    let cors_origins = raw.serve.cors_origins.unwrap_or_default();
    let cors_loopback = raw.serve.cors_loopback.unwrap_or(false);
    let visible_wait_max_secs = raw
        .serve
        .visible_wait_max_secs
        .unwrap_or(DEFAULT_VISIBLE_WAIT_MAX_SECS);
    for (key, origins) in [
        ("dev_cors_origins", &dev_cors_origins),
        ("cors_origins", &cors_origins),
    ] {
        if origins.iter().any(|origin| origin.trim() == "*") {
            return Err(ConfigError::CorsWildcard { key });
        }
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
    let single_flight_wait_ms = raw
        .serve
        .single_flight_wait_ms
        .unwrap_or(tessera_engine::DEFAULT_SINGLE_FLIGHT_WAIT_MS);
    if single_flight_wait_ms == 0 {
        return Err(ConfigError::SingleFlightWaitZero);
    }
    let stream_flush_bytes = raw
        .serve
        .stream_flush_bytes
        .unwrap_or(DEFAULT_STREAM_FLUSH_BYTES);
    if stream_flush_bytes == 0 {
        return Err(ConfigError::StreamFlushBytesZero);
    }
    let stream_write_stall_ms = raw
        .serve
        .stream_write_stall_ms
        .unwrap_or(DEFAULT_STREAM_WRITE_STALL_MS);
    if stream_write_stall_ms == 0 {
        return Err(ConfigError::StreamWriteStallZero);
    }
    let stream_deadline_ms = raw
        .serve
        .stream_deadline_ms
        .unwrap_or(DEFAULT_STREAM_DEADLINE_MS);
    if stream_deadline_ms == 0 {
        return Err(ConfigError::StreamDeadlineZero);
    }

    // The write-path knobs. All of them default (SA §7: performance knobs default, disclosure
    // controls do not — none of these is a disclosure control); all of them refuse a zero, each with
    // its own silent failure named. Every one of them has a consumer (decision 0045).
    let commit_window_max_items = non_zero_usize(
        "ingest.commit_window_max_items",
        raw.ingest
            .commit_window_max_items
            .unwrap_or(DEFAULT_COMMIT_WINDOW_MAX_ITEMS),
        "a window that closes at zero rows is not group commit switched off, it is group \
         commit silently doing nothing — set it to 1 to disable batching honestly",
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
    let publish_max_body_bytes = non_zero_usize(
        "ingest.publish_max_body_bytes",
        raw.ingest
            .publish_max_body_bytes
            .unwrap_or(DEFAULT_PUBLISH_MAX_BODY_BYTES),
        "every publication and growth would be refused with 422, closing the artifact plane \
         while every health surface still reports the server up",
    )?;
    let max_artifacts_per_request = non_zero_usize(
        "ingest.max_artifacts_per_request",
        raw.ingest
            .max_artifacts_per_request
            .unwrap_or(DEFAULT_MAX_ARTIFACTS_PER_REQUEST),
        "every publication would be refused with 422, since a publication carries at least one \
         artifact",
    )?;
    let max_members_per_request = non_zero_usize(
        "ingest.max_members_per_request",
        raw.ingest
            .max_members_per_request
            .unwrap_or(DEFAULT_MAX_MEMBERS_PER_REQUEST),
        "every growth page naming a member would be refused with 422",
    )?;
    let max_excluded_per_request = non_zero_usize(
        "ingest.max_excluded_per_request",
        raw.ingest
            .max_excluded_per_request
            .unwrap_or(DEFAULT_MAX_EXCLUDED_PER_REQUEST),
        "a published bound of zero admits no exclusion list at all, which is the inclusion \
         spelling's job and not a bound",
    )?;
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
        "a zero row trigger would make every commit-window close a publication, which is the \
         B/W = 1 end of the measured curve and 30-42% worse than the optimum",
    )?;
    let flush_max_age_secs = non_zero_u64(
        "ingest.flush_max_age_secs",
        raw.ingest
            .flush_max_age_secs
            .unwrap_or(DEFAULT_FLUSH_MAX_AGE_SECS),
        "a zero tick asks the executor to publish geometry continuously, which is a publication \
         per loop iteration and a drain entry per publication",
    )?;
    // ---- the compaction schedule (compaction §9, decision 0056) ---------------------------------
    //
    // Two routes, each switchable off on its own, and a floor under both. Every value is validated
    // here rather than clamped, on this file's standing rule: a maintenance route that silently
    // does not run is indistinguishable from one that has nothing to do.
    let compaction_window_secs = raw
        .ingest
        .compaction_window_secs
        .unwrap_or(DEFAULT_COMPACTION_WINDOW_SECS);
    if u64::from(compaction_window_secs) >= 86_400 {
        return Err(ConfigError::CompactionWindowNotAWindow(
            compaction_window_secs,
        ));
    }
    let compaction_window_start_secs = match raw.ingest.compaction_window_start.as_deref() {
        None => Some(DEFAULT_COMPACTION_WINDOW_START_SECS),
        Some("off") => None,
        Some(value) => Some(parse_time_of_day(value)?),
    };
    // **Defaults to `overlay_soft_limit`**, which is the action compaction §9 says that alarm was
    // always supposed to prompt. Two independently-set thresholds is how an operator ends up with
    // an alarm that never has a consequence; it stays separately settable for the deployment that
    // wants to be told earlier than it wants to act.
    let compaction_after_deletions = match &raw.ingest.compaction_after_deletions {
        None => Some(overlay_soft_limit as u64),
        Some(RawThreshold::Count(n)) => Some(*n),
        Some(RawThreshold::Word(word)) if word == "off" => None,
        Some(RawThreshold::Word(word)) => {
            return Err(ConfigError::CompactionThresholdNotANumberOrOff(
                word.clone(),
            ))
        }
    };
    // **The segment gauge's ceiling**: the count past which deferring to the next window costs more
    // than folding now. Off is legitimate for a deployment that would rather never fold in
    // business hours than never carry a slow viewport.
    let compaction_max_segments = match &raw.ingest.compaction_max_segments {
        None => Some(DEFAULT_COMPACTION_MAX_SEGMENTS),
        Some(RawThreshold::Count(n)) => Some(*n as usize),
        Some(RawThreshold::Word(word)) if word == "off" => None,
        Some(RawThreshold::Word(word)) => {
            return Err(ConfigError::CompactionThresholdNotANumberOrOff(
                word.clone(),
            ))
        }
    };
    // **The two gauges compaction §9 names for the obligations the counts above cannot see.** The
    // fraction is what a *viewport* pays — rows scanned that no viewer may see — and is a different
    // question from `compaction_after_deletions` over the same numerator: a small corpus crosses
    // the ratio long before the absolute, and a 10⁹-row one the other way round. The byte ratio is
    // the only route that covers reclamation at all: a deployment with heavy merge churn and few
    // deletions has a bounded segment count, a shallow overlay, and three copies of its corpus.
    //
    // **Refused rather than clamped, and a ratio has two ways to be nonsense.** Not finite, or not
    // positive: a zero or negative threshold is a route that can never decline, which is the
    // ungated timer §9 declines reached by setting a gauge below every possible value.
    let compaction_dead_rows_fraction = ratio_or_off(
        "ingest.compaction_dead_rows_fraction",
        raw.ingest.compaction_dead_rows_fraction.as_ref(),
        DEFAULT_COMPACTION_DEAD_ROWS_FRACTION,
    )?;
    let compaction_dead_bytes_ratio = ratio_or_off(
        "ingest.compaction_dead_bytes_ratio",
        raw.ingest.compaction_dead_bytes_ratio.as_ref(),
        DEFAULT_COMPACTION_DEAD_BYTES_RATIO,
    )?;
    let compaction_window_min_segments = non_zero_usize(
        "ingest.compaction_window_min_segments",
        raw.ingest
            .compaction_window_min_segments
            .unwrap_or(DEFAULT_COMPACTION_WINDOW_MIN_SEGMENTS),
        "a zero threshold makes every night's window fold a bundle that is already one segment \
         per view — the ungated timer compaction §9 declines, reached by setting a gauge to a \
         value nothing can be below",
    )?;
    // The floor must sit strictly below the ceiling or the window can never open — see
    // `ConfigError::CompactionSegmentThresholdsInverted`. Checked only while the window is armed:
    // with `compaction_window_start = "off"` there is no window for the floor to be inert in.
    if let (Some(_), Some(ceiling)) = (compaction_window_start_secs, compaction_max_segments) {
        if ceiling <= compaction_window_min_segments {
            return Err(ConfigError::CompactionSegmentThresholdsInverted {
                window_min_segments: compaction_window_min_segments,
                max_segments: ceiling,
            });
        }
    }
    let compaction = tessera_engine::CompactionSchedule {
        min_interval_secs: raw
            .ingest
            .compaction_min_interval_secs
            .unwrap_or(DEFAULT_COMPACTION_MIN_INTERVAL_SECS),
        window_start_secs: compaction_window_start_secs,
        window_secs: compaction_window_secs,
        window_min_segments: compaction_window_min_segments,
        max_segments: compaction_max_segments,
        after_deletions: compaction_after_deletions,
        tombstoned_rows_fraction: compaction_dead_rows_fraction,
        dead_bytes_ratio: compaction_dead_bytes_ratio,
    };

    let ingest_buffer_max_items = non_zero_usize(
        "ingest.ingest_buffer_max_items",
        raw.ingest
            .ingest_buffer_max_items
            .unwrap_or(DEFAULT_INGEST_BUFFER_MAX_ITEMS),
        "a zero buffer bound refuses EVERY ingest with 429 while denies continue normally — \
         indistinguishable from ingest being switched off, but silently",
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
    let tier_width = selection_width(
        "serve.tier_width",
        raw.serve.tier_width.unwrap_or(DEFAULT_TIER_WIDTH),
    )?;
    let coalesce_width = selection_width(
        "serve.coalesce_width",
        raw.serve.coalesce_width.unwrap_or(DEFAULT_COALESCE_WIDTH),
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
    let masked_count_cache_bytes = non_zero_u64(
        "serve.masked_count_cache_bytes",
        raw.serve
            .masked_count_cache_bytes
            .unwrap_or(DEFAULT_MASKED_COUNT_CACHE_BYTES),
        "a cache that admits nothing rebuilds a whole level's masked counts on every request that \
         reaches a row-major layer, which is a walk of the session's entire mask per request",
    )?;
    let occupancy_cache_bytes = non_zero_u64(
        "serve.occupancy_cache_bytes",
        raw.serve
            .occupancy_cache_bytes
            .unwrap_or(tessera_engine::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES),
        "a memo that admits nothing makes every request at a new depth walk the session's mask \
         and the Morton column again, which is the walk the memo exists to pay once per session \
         and generation",
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
        segment_floor_bytes,
        max_merged_segment_bytes,
        tier_width,
        coalesce_width,
        bundle_path: raw.bundle.path,
        cache_dir: raw.bundle.cache,
        wal_path: raw.bundle.wal,
        schema_path,
        identity_env,
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
        max_category_values: raw
            .serve
            .max_category_values
            .unwrap_or(DEFAULT_MAX_CATEGORY_VALUES),
        max_suggestions: raw.serve.max_suggestions.unwrap_or(DEFAULT_MAX_SUGGESTIONS),
        max_suggestion_walk: raw
            .serve
            .max_suggestion_walk
            .unwrap_or(DEFAULT_MAX_SUGGESTION_WALK),
        max_suggest_set_entities: raw
            .serve
            .max_suggest_set_entities
            .unwrap_or(DEFAULT_MAX_SUGGEST_SET_ENTITIES),
        max_shape_vertices: raw
            .serve
            .max_shape_vertices
            .unwrap_or(tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES),
        max_region_vertices: raw
            .serve
            .max_region_vertices
            .unwrap_or(DEFAULT_MAX_REGION_VERTICES),
        max_region_cells: raw
            .serve
            .max_region_cells
            .unwrap_or(tessera_engine::DEFAULT_MAX_REGION_CELLS),
        max_browse_rows: raw.serve.max_browse_rows.unwrap_or(DEFAULT_MAX_BROWSE_ROWS),
        region_cache_bytes: raw
            .serve
            .region_cache_bytes
            .unwrap_or(DEFAULT_REGION_CACHE_BYTES),
        stage_timing: raw.serve.stage_timing.unwrap_or(false),
        dev_cors_origins,
        cors_origins,
        cors_loopback,
        visible_wait_max_secs,
        session_credential,
        operator_credential,
        compute_threads,
        compute_admission,
        compute_queue,
        admission_timeout_ms,
        single_flight_wait_ms,
        stream_flush_bytes,
        stream_write_stall_ms,
        stream_deadline_ms,
        commit_window_max_items,
        ingest_queue_bound,
        ingest_admission,
        ingest_max_batch_rows,
        ingest_max_batch_bytes,
        publish_max_body_bytes,
        max_artifacts_per_request,
        max_members_per_request,
        max_excluded_per_request,
        wal_hard_limit_bytes,
        overlay_soft_limit,
        compaction,
        flush_max_age_secs,
        flush_max_items,
        row_projection_cache_bytes,
        masked_count_cache_bytes,
        occupancy_cache_bytes,
        fragment_cache_bytes,
        expected_concurrent_sessions,
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

/// Where a bearer secret comes from: a `0600` file, or an environment variable. **Never inline** —
/// `tessera.toml` belongs in git.
///
/// **The locator is configuration; the secret is not.** Holding the two apart is what lets
/// `tessera build` read this file without a serving credential in its environment, and it keeps
/// the secret out of a `Debug`-derived struct that gets logged.
#[derive(Debug, Clone)]
pub struct Credential {
    file: Option<PathBuf>,
    env: Option<String>,
}

impl Credential {
    fn declared(file: Option<PathBuf>, env: Option<String>) -> Credential {
        Credential { file, env }
    }

    /// Read the secret, refusing a credential declared nowhere. Called once, at startup, from
    /// [`crate::prepare`] — so a plane can never come up without its secret, and a build that
    /// reads this same file never needs one.
    pub fn resolve(&self, name: &'static str) -> Result<String> {
        if let Some(path) = &self.file {
            let secret = fs::read_to_string(path).map_err(|source| {
                ConfigError::CredentialFileUnreadable {
                    which: name,
                    path: path.clone(),
                    source,
                }
            })?;
            return Ok(secret.trim().to_string());
        }
        if let Some(var) = &self.env {
            return std::env::var(var).map_err(|_| ConfigError::MissingCredential(name));
        }
        Err(ConfigError::MissingCredential(name))
    }
}

#[cfg(test)]
mod tests {

    /// **A build-only deployment declares no `[serve]` section at all**, and that parses.
    ///
    /// `tessera build` reads this same file (`configuration.md` §3), so requiring three listen
    /// addresses before a bundle could be written would be a serving concern refusing a build.
    /// What is *not* optional is a server coming up without them: `lib.rs`'s `prepare` refuses,
    /// naming all three, rather than binding a port nobody chose.
    #[test]
    fn a_build_only_deployment_needs_no_serve_section() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            token_max_lifetime = 3600
        "#;
        let config = parse(toml).expect("a build-only deployment parses");
        assert!(config.viewer_addr.is_none());
        assert!(config.session_addr.is_none());
        assert!(config.control_listen.is_none());
    }
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

    /// `[disclosure]` takes `token_max_lifetime` and nothing else. An operator who writes a key
    /// the table does not know is told so, rather than having it ignored.
    #[test]
    fn an_unknown_disclosure_key_refuses_to_start() {
        let with = |extra: &str| {
            format!(
                r#"
                [bundle]
                path = "b"
                cache = "c"
                wal = "w"
                [plugin]
                module = "builtin:passthrough"
                [disclosure]
                token_max_lifetime = 3600
                {extra}
            "#
            )
        };
        parse(&with("")).expect("token_max_lifetime alone is the whole section");
        let err = parse(&with("min_visible_members = 10")).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    // ---- the compaction schedule (compaction §9, decision 0056) ------------------------------

    /// **The shipped defaults are the ones compaction §9 states**, and a deployment that writes no
    /// `[ingest]` section gets them: a fold at midnight UTC for four hours once a view reaches
    /// eight segments, an unwindowed fold at `overlay_soft_limit` deletions, and one fold a day.
    ///
    /// **Mutations this kills:** defaulting the window off (a deployment gets a mechanism only if
    /// it remembers, which is what decision 0056's D3 predecessor refuses); defaulting
    /// `after_deletions` to a number of its own rather than to `overlay_soft_limit` (an alarm with
    /// no consequence).
    #[test]
    fn the_compaction_schedule_defaults_to_section_9s_own_numbers() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).unwrap();
        assert_eq!(
            config.compaction,
            tessera_engine::CompactionSchedule {
                min_interval_secs: 86_400,
                window_start_secs: Some(0),
                window_secs: 4 * 3_600,
                window_min_segments: 8,
                max_segments: Some(64),
                after_deletions: Some(DEFAULT_OVERLAY_SOFT_LIMIT as u64),
                tombstoned_rows_fraction: Some(0.2),
                dead_bytes_ratio: Some(1.0),
            }
        );
    }

    /// **The two ratio gauges parse, switch off by name, and refuse a threshold that can never
    /// decline.**
    ///
    /// Zero and negative are the sharp cases: both are satisfied by every possible measurement, so
    /// the route would dispatch a fold at every tick that clears the interval floor — the ungated
    /// timer compaction §9 declines, reached by configuration rather than by design.
    ///
    /// **Mutations this kills:** clamping instead of refusing; accepting `<= 0.0`; accepting a
    /// non-finite value; reading an unrecognised word as absent (the default) rather than refusing.
    #[test]
    fn a_ratio_gauge_takes_a_number_or_off_and_refuses_a_threshold_nothing_can_be_under() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        let set = parse(&valid_toml_with(
            "",
            "compaction_dead_rows_fraction = 0.05\ncompaction_dead_bytes_ratio = 2.5",
        ))
        .unwrap();
        assert_eq!(set.compaction.tombstoned_rows_fraction, Some(0.05));
        assert_eq!(set.compaction.dead_bytes_ratio, Some(2.5));

        let off = parse(&valid_toml_with(
            "",
            "compaction_dead_rows_fraction = \"off\"\ncompaction_dead_bytes_ratio = \"off\"",
        ))
        .unwrap();
        assert_eq!(off.compaction.tombstoned_rows_fraction, None);
        assert_eq!(off.compaction.dead_bytes_ratio, None);

        for bad in ["0.0", "-1.0", "\"sometimes\""] {
            let err = parse(&valid_toml_with(
                "",
                &format!("compaction_dead_bytes_ratio = {bad}"),
            ))
            .expect_err("a threshold nothing can be under must be refused");
            let text = err.to_string();
            assert!(
                text.contains("compaction_dead_bytes_ratio"),
                "the refusal must name the key: {text}"
            );
        }
    }

    /// **The two segment thresholds are a floor and a ceiling, and an inverted pair is refused.**
    ///
    /// A ceiling at or below the floor makes the window unreachable: every count that would have
    /// opened it has already fired the any-hour route, so three keys parse, validate and can never
    /// fire — the inert key decision 0045 forbids.
    ///
    /// **Mutations this kills:** dropping the check (a deployment ships with a dead window);
    /// making it `<` rather than `<=` (equal thresholds leave the window equally unreachable, since
    /// the unwindowed route is consulted first); applying it while the window is off, where the
    /// floor has no route to be inert in.
    #[test]
    fn an_inverted_pair_of_segment_thresholds_is_refused_unless_the_window_is_off() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        for ceiling in ["4", "8"] {
            let toml = valid_toml_with(
                "",
                &format!(
                    "compaction_window_min_segments = 8
compaction_max_segments = {ceiling}
"
                ),
            );
            assert!(
                matches!(
                    parse(&toml),
                    Err(ConfigError::CompactionSegmentThresholdsInverted { .. })
                ),
                "a ceiling of {ceiling} under a floor of 8 leaves the window unreachable"
            );
        }
        assert!(parse(&valid_toml_with(
            "",
            "compaction_window_min_segments = 8
compaction_max_segments = 9
"
        ))
        .is_ok());

        // With the window off there is no window to make inert, so the pair is not compared.
        let window_off = parse(&valid_toml_with(
            "",
            "compaction_window_start = \"off\"
compaction_window_min_segments = 8
             compaction_max_segments = 4
",
        ))
        .expect("a window that is off cannot be made unreachable");
        assert_eq!(window_off.compaction.max_segments, Some(4));
    }

    /// The any-hour segment ceiling switches off on its own, leaving segment growth to the window
    /// however far it goes — the posture a deployment takes when it would rather carry a slow
    /// viewport than fold in business hours.
    #[test]
    fn the_segment_ceiling_switches_off_on_its_own() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let parsed = parse(&valid_toml_with(
            "",
            "compaction_max_segments = \"off\"
",
        ))
        .unwrap();
        assert_eq!(parsed.compaction.max_segments, None);
        assert!(parsed.compaction.window_start_secs.is_some());
    }

    /// `overlay_soft_limit` is the *alarm*, and the unwindowed route follows it unless told
    /// otherwise — compaction §9's "the action its alarm was always supposed to prompt".
    #[test]
    fn the_deletion_route_follows_the_overlay_alarm_unless_set_apart_from_it() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let followed = parse(&valid_toml_with(
            "",
            "overlay_soft_limit = 42
",
        ))
        .unwrap();
        assert_eq!(followed.compaction.after_deletions, Some(42));

        let apart = parse(&valid_toml_with(
            "",
            "overlay_soft_limit = 42
compaction_after_deletions = 9000
",
        ))
        .unwrap();
        assert_eq!(
            apart.compaction.after_deletions,
            Some(9000),
            "a deployment may be told earlier than it acts"
        );
    }

    /// Each route switches off on its own, spelled `"off"` — compaction §9's own word for it.
    #[test]
    fn each_compaction_route_switches_off_independently() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let no_window = parse(&valid_toml_with(
            "",
            "compaction_window_start = \"off\"
",
        ))
        .unwrap();
        assert_eq!(no_window.compaction.window_start_secs, None);
        assert!(no_window.compaction.after_deletions.is_some());

        let no_depth = parse(&valid_toml_with(
            "",
            "compaction_after_deletions = \"off\"
",
        ))
        .unwrap();
        assert_eq!(no_depth.compaction.after_deletions, None);
        assert!(no_depth.compaction.window_start_secs.is_some());
    }

    /// `HH:MM` is parsed strictly, in UTC, and anything else is refused at startup.
    ///
    /// **Refused rather than defaulted**, because both failure directions are silent: read as
    /// "off" it retires the deployment's only fold schedule, and read as midnight it starts hours
    /// of IO at the hour the operator was trying to avoid.
    ///
    /// **Mutation this kills:** a lenient parser — `"9:30"`, `"24:00"` and `"00:60"` would all be
    /// accepted, each answering with a time nobody wrote.
    #[test]
    fn the_window_start_is_a_strict_utc_time_of_day() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let parsed = parse(&valid_toml_with(
            "",
            "compaction_window_start = \"02:30\"
",
        ))
        .unwrap();
        assert_eq!(parsed.compaction.window_start_secs, Some(2 * 3_600 + 1_800));

        for bad in ["9:30", "24:00", "00:60", "0230", "2:3", "midnight", ""] {
            let toml = valid_toml_with(
                "",
                &format!(
                    "compaction_window_start = \"{bad}\"
"
                ),
            );
            assert!(
                matches!(parse(&toml), Err(ConfigError::CompactionWindowNotATime(_))),
                "'{bad}' should be refused"
            );
        }
    }

    /// A width of a whole day is the ungated timer compaction §9 declines, reached by setting a
    /// width rather than by asking for one — so it is refused rather than accepted as "always".
    #[test]
    fn a_day_wide_window_is_refused_because_it_is_not_a_window() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let toml = valid_toml_with(
            "",
            "compaction_window_secs = 86400
",
        );
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::CompactionWindowNotAWindow(86_400))
        ));
        // And one second under it is a window, however impractical.
        assert!(parse(&valid_toml_with(
            "",
            "compaction_window_secs = 86399
"
        ))
        .is_ok());
    }

    /// A zero segment threshold makes every night's window fold a bundle that is already one
    /// segment per view — the same ungated timer, reached through the gauge instead.
    #[test]
    fn a_zero_segment_threshold_is_refused() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let toml = valid_toml_with(
            "",
            "compaction_window_min_segments = 0
",
        );
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::MustBeNonZero {
                key: "ingest.compaction_window_min_segments",
                ..
            })
        ));
    }

    // ---- the deployment file itself (`configuration.md` §3) -----------------------------------

    /// **Both halves default**, so the ordinary `tessera.toml` writes neither section: the
    /// declaration is `schema.toml` beside this file, and the key is in `TESSERA_IDENTITY_KEY`.
    #[test]
    fn the_build_half_defaults_to_schema_toml_and_the_named_variable() {
        let config = parse(&valid_toml("")).expect("a config naming neither must load");
        assert_eq!(config.schema_path, PathBuf::from(DEFAULT_SCHEMA_FILE));
        assert_eq!(config.identity_env, DEFAULT_IDENTITY_ENV);
    }

    #[test]
    fn the_build_half_is_declarable() {
        let toml = format!(
            "{}\n[build]\nschema = \"corpus/declaration.toml\"\n[identity]\nenv = \"ACME_KEY\"\n",
            valid_toml("")
        );
        let config = parse(&toml).expect("both sections must load");
        assert_eq!(config.schema_path, PathBuf::from("corpus/declaration.toml"));
        assert_eq!(config.identity_env, "ACME_KEY");
    }

    /// **The key itself never appears in this file**, which belongs in git. Refused with its own
    /// message rather than as an unknown field, because the mistake is a reasonable one —
    /// `--identity-file`'s format does spell it `key` — and the consequence is a committed secret.
    #[test]
    fn a_key_written_into_the_deployment_file_is_refused() {
        let toml = format!("{}\n[identity]\nkey = \"00\"\n", valid_toml(""));
        let err = parse(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::IdentityKeyInline));
        let message = err.to_string();
        assert!(message.contains("never appears in this file"), "{message}");
        assert!(message.contains("TESSERA_IDENTITY_KEY"), "{message}");

        let toml = format!("{}\n[identity]\nenvv = \"X\"\n", valid_toml(""));
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::UnknownIdentityKey(ref k)) if k == "envv"
        ));
    }

    /// **`tessera.toml` is found by walking up**, as `Cargo.toml` is, and a missing one is a
    /// refusal naming what to create — never a silent set of defaults, since every path in it is a
    /// decision.
    #[test]
    fn the_deployment_file_is_found_by_walking_up_and_its_absence_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();

        let err = discover(None, &deep).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no tessera.toml found"), "{message}");
        assert!(message.contains("--deployment"), "{message}");
        for section in ["[bundle]", "[build]", "[identity]", "[serve]"] {
            assert!(message.contains(section), "{section} missing: {message}");
        }

        let at = tmp.path().join(DEPLOYMENT_FILE);
        std::fs::write(&at, "").unwrap();
        assert_eq!(discover(None, &deep).unwrap(), at);

        // Named outright, the search does not run at all.
        let named = PathBuf::from("/elsewhere/tessera.toml");
        assert_eq!(discover(Some(&named), &deep).unwrap(), named);
    }

    /// **Every path resolves against the file's own directory, not the shell's.** A relative
    /// `bundle.path` that moved with the working directory would make `cd crates && tessera serve`
    /// open a different bundle from the one `tessera build` had just written.
    #[test]
    fn paths_resolve_against_the_deployment_files_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let at = tmp.path().join(DEPLOYMENT_FILE);
        std::fs::write(&at, valid_toml("")).unwrap();
        let config = load(&at).expect("the file loads");
        assert_eq!(config.bundle_path, tmp.path().join("b"));
        assert_eq!(config.cache_dir, tmp.path().join("c"));
        assert_eq!(config.wal_path, tmp.path().join("w"));
        assert_eq!(config.schema_path, tmp.path().join(DEFAULT_SCHEMA_FILE));
    }

    /// **A credential file resolves against the deployment file's directory too**, and a missing
    /// one is refused naming the path that was tried. Resolving it against the working directory
    /// makes `tessera serve --deployment <dir>/tessera.toml` from anywhere but `<dir>` refuse with
    /// an error naming nothing.
    #[test]
    fn a_credential_file_resolves_against_the_deployment_files_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let at = tmp.path().join(DEPLOYMENT_FILE);
        // The process's working directory is the crate root, which holds no `session.cred`, so a
        // path read against it cannot be the one that resolves.
        std::fs::write(
            tmp.path().join("session.cred"),
            "s3cret
",
        )
        .unwrap();
        std::fs::write(
            &at,
            valid_toml("session_credential_file = \"session.cred\"\n").replace(
                "session_credential_env = \"TESSERA_TEST_SESSION_CRED\"\n",
                "",
            ),
        )
        .unwrap();
        assert!(
            !Path::new("session.cred").exists(),
            "the cwd must not hold one"
        );

        let config = load(&at).expect("the file loads");
        assert_eq!(
            config.session_credential.file.as_deref(),
            Some(tmp.path().join("session.cred").as_path())
        );
        assert_eq!(
            config.session_credential.resolve("session").unwrap(),
            "s3cret"
        );

        // A file named and absent is refused naming the resolved path, not a bare io error.
        let at = tmp.path().join("absent").join(DEPLOYMENT_FILE);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(
            &at,
            valid_toml("operator_credential_file = \"operator.cred\"\n").replace(
                "operator_credential_env = \"TESSERA_TEST_OPERATOR_CRED\"\n",
                "",
            ),
        )
        .unwrap();
        let config = load(&at).expect("the file loads");
        let err = config
            .operator_credential
            .resolve("operator")
            .expect_err("an absent credential file is refused");
        let message = err.to_string();
        let resolved = tmp.path().join("absent").join("operator.cred");
        assert!(
            message.contains(&resolved.display().to_string()),
            "{message}"
        );
        assert!(message.contains("operator"), "{message}");
    }

    /// **A serving secret is read at startup, never at parse.** `tessera build` reads this same
    /// file and has no business requiring one to be exported before it will write a bundle; what
    /// a plane cannot do is come up without one.
    #[test]
    fn a_serving_credential_is_read_at_startup_rather_than_at_parse() {
        // A variable nothing in this process sets, named rather than unset: the cases around this
        // one set credential variables of their own, and `remove_var` would race them.
        let toml = valid_toml("").replace(
            "TESSERA_TEST_SESSION_CRED",
            "TESSERA_TEST_CREDENTIAL_THAT_IS_NEVER_SET",
        );
        let config = parse(&toml).expect("an unset credential variable must still parse");
        assert!(matches!(
            config.session_credential.resolve("session"),
            Err(ConfigError::MissingCredential("session"))
        ));

        // And a credential declared nowhere at all fails the same way, at the same moment.
        let toml = valid_toml("").replace(
            "session_credential_env = \"TESSERA_TEST_SESSION_CRED\"\n",
            "",
        );
        let config = parse(&toml).expect("a config declaring no session credential still parses");
        assert!(matches!(
            config.session_credential.resolve("session"),
            Err(ConfigError::MissingCredential("session"))
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
        assert_eq!(config.segment_floor_bytes, DEFAULT_SEGMENT_FLOOR_BYTES);
        assert_eq!(config.tier_width, DEFAULT_TIER_WIDTH);
        assert_eq!(config.coalesce_width, DEFAULT_COALESCE_WIDTH);
        assert_eq!(
            config.max_merged_segment_bytes, None,
            "no fixed default can satisfy relation 2 — it is derived from the base segment"
        );
    }

    /// A selection width below 2 can select nothing — `MergePolicy::select` returns `None`, and
    /// the coalesce declines the same way — so a deployment that set 1 would silently never merge
    /// (or coalesce): the same silent-failure class as the inert keys these two used to be.
    /// Refused at load, naming the key, rather than discovered as an artefact count that never
    /// comes down.
    #[test]
    fn a_selection_width_below_two_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        for key in ["tier_width", "coalesce_width"] {
            for value in [0usize, 1] {
                let err = parse(&valid_toml(&format!("{key} = {value}"))).unwrap_err();
                let ConfigError::SelectionWidthBelowTwo { key: named, width } = err else {
                    panic!("serve.{key} = {value} must be refused as SelectionWidthBelowTwo, got {err}");
                };
                assert_eq!(named, format!("serve.{key}"));
                assert_eq!(width, value);
            }
        }
        // 2 is the smallest width that can select, on both keys.
        assert!(parse(&valid_toml("tier_width = 2\ncoalesce_width = 2")).is_ok());
    }

    /// An explicitly set merge or coalesce knob lands in [`Config`]. This pins the *parse*; that
    /// the value then reaches selection and changes which segments merge is behaviour, and is
    /// asserted where the policy lives (`tessera-engine`'s `tests/merge.rs` and
    /// `tests/coalesce.rs`) — a key that parses into a struct nobody reads was exactly the defect
    /// these keys had.
    #[test]
    fn the_merge_and_coalesce_knobs_parse_when_set() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml(
            "tier_width = 2\nsegment_floor_bytes = 1\ncoalesce_width = 3",
        ))
        .expect("explicit merge knobs must load");
        assert_eq!(config.tier_width, 2);
        assert_eq!(config.segment_floor_bytes, 1);
        assert_eq!(config.coalesce_width, 3);
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

    /// `theta_target_marks = 0` anchors θ at a cut admitting nothing at every depth, so every tile
    /// would draw exactly `k_min` at every zoom with no error — the failure
    /// `Threshold::at_depth`'s saturation test prevents in the arithmetic, reached through config
    /// instead. `N_occ(d)` cannot lift a zero product, so no corpus shape rescues it.
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
        assert!(
            config.cors_origins.is_empty(),
            "the dev key must not populate the production list — they are two postures, not one \
             list with two spellings"
        );
    }

    /// `serve.cors_loopback` is absent by default, on `cors_origins`' reasoning: a disclosure
    /// control a deployment has not written down is one it has not decided.
    #[test]
    fn cors_loopback_defaults_to_false_and_round_trips() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("a config naming no CORS keys must load");
        assert!(!config.cors_loopback);

        let config = parse(&valid_toml("cors_loopback = true")).expect("the key must load");
        assert!(config.cors_loopback);
        assert!(
            config.cors_origins.is_empty() && config.dev_cors_origins.is_empty(),
            "the rule is not a list and must populate neither"
        );
    }

    /// `serve.cors_origins` — the production list (decision 0102) — defaults to empty on the same
    /// fail-closed reasoning, and round-trips beside the development key rather than instead of
    /// it. Both may be set: a laptop pointed at a deployment that also serves a drop-in.
    #[test]
    fn cors_origins_defaults_to_empty_and_round_trips_beside_the_dev_key() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("a config naming no CORS origins must load");
        assert!(config.cors_origins.is_empty());

        let toml = valid_toml(
            "dev_cors_origins = [\"http://localhost:5173\"]\n\
             cors_origins = [\"https://app.example\", \"https://docs.example\"]",
        );
        let config = parse(&toml).expect("both lists together must load");
        assert_eq!(config.dev_cors_origins, vec!["http://localhost:5173"]);
        assert_eq!(
            config.cors_origins,
            vec!["https://app.example", "https://docs.example"]
        );
    }

    /// A duplicate origin across the two lists is not an error. The viewer plane matches an origin
    /// by equality against the concatenation, so a repeat costs a comparison and nothing else —
    /// and refusing it would make the ordinary case (a dev origin still listed after the
    /// production one arrives) a startup failure for no disclosure reason.
    #[test]
    fn an_origin_in_both_lists_is_not_an_error() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let toml = valid_toml(
            "dev_cors_origins = [\"https://app.example\"]\n\
             cors_origins = [\"https://app.example\"]",
        );
        let config = parse(&toml).expect("a duplicated origin must load");
        assert_eq!(config.dev_cors_origins, config.cors_origins);
    }

    /// A wildcard is refused at parse, in **either** list, naming the key that carries it.
    ///
    /// Decision 0102's list is enumerated or absent. Dropping the wildcard instead would leave an
    /// operator who asked for open CORS with a working server and no CORS; allowing it would hand
    /// every page there has ever been the right to present this deployment's tokens.
    #[test]
    fn a_wildcard_origin_is_refused_in_either_list() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("cors_origins = [\"*\"]")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::CorsWildcard {
                    key: "cors_origins"
                }
            ),
            "{err}"
        );
        let err = parse(&valid_toml(
            "dev_cors_origins = [\"http://localhost:5173\", \" * \"]",
        ))
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::CorsWildcard {
                    key: "dev_cors_origins"
                }
            ),
            "surrounding whitespace must not smuggle a wildcard past the check: {err}"
        );
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
        assert_eq!(config.ingest_queue_bound, DEFAULT_INGEST_QUEUE_BOUND);
        assert_eq!(config.ingest_admission, DEFAULT_INGEST_ADMISSION);
        assert_eq!(config.ingest_max_batch_rows, DEFAULT_INGEST_MAX_BATCH_ROWS);
        assert_eq!(
            config.ingest_max_batch_bytes,
            DEFAULT_INGEST_MAX_BATCH_BYTES
        );
        assert_eq!(
            config.publish_max_body_bytes,
            DEFAULT_PUBLISH_MAX_BODY_BYTES
        );
        assert_eq!(
            config.max_artifacts_per_request,
            DEFAULT_MAX_ARTIFACTS_PER_REQUEST
        );
        assert_eq!(
            config.max_members_per_request,
            DEFAULT_MAX_MEMBERS_PER_REQUEST
        );
        assert_eq!(
            config.max_excluded_per_request,
            DEFAULT_MAX_EXCLUDED_PER_REQUEST
        );
        assert_eq!(config.wal_hard_limit_bytes, DEFAULT_WAL_HARD_LIMIT_BYTES);
        assert_eq!(config.overlay_soft_limit, DEFAULT_OVERLAY_SOFT_LIMIT);
        assert_eq!(config.flush_max_age_secs, DEFAULT_FLUSH_MAX_AGE_SECS);
        assert_eq!(config.flush_max_items, DEFAULT_FLUSH_MAX_ITEMS);
        assert_eq!(
            config.row_projection_cache_bytes,
            DEFAULT_ROW_PROJECTION_CACHE_BYTES
        );
        assert_eq!(config.fragment_cache_bytes, DEFAULT_FRAGMENT_CACHE_BYTES);
        assert_eq!(
            config.expected_concurrent_sessions,
            DEFAULT_EXPECTED_CONCURRENT_SESSIONS
        );
    }

    /// And the keys are actually wired to their fields — a defaults test alone would pass just as
    /// happily against fifteen constants nothing parses. One key per section, plus the byte- and
    /// duration-typed shapes, so a mis-sectioned or mis-typed key is caught here.
    #[test]
    fn the_stage_2_1_knobs_are_read_from_their_sections() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml_with(
            "expected_concurrent_sessions = 42\nrow_projection_cache_bytes = 777000000",
            // 3 GiB + a bit. This key is an operand of the WAL headroom relation, so a value
            // chosen only for legibility would make the whole config refuse to start; it is kept
            // above `queue worst case + reserved deny headroom` at the defaults.
            "commit_window_max_items = 7\nwal_hard_limit_bytes = 3000000000\nflush_max_items = 55",
        ))
        .expect("must load");
        assert_eq!(config.expected_concurrent_sessions, 42);
        assert_eq!(config.row_projection_cache_bytes, 777_000_000);
        assert_eq!(config.commit_window_max_items, 7);
        assert_eq!(config.wal_hard_limit_bytes, 3_000_000_000);
        // Restored 2026-09-04, so it is wired here as well as defaulted above — the property
        // decision 0045 found missing was exactly this one.
        assert_eq!(config.flush_max_items, 55);
    }

    /// Every write-path knob refuses a zero, and refuses it by *name*. Zero is degenerate for all
    /// thirteen — never "off" — and the failure modes are silent ones: a window that batches
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
            "ingest_queue_bound",
            "ingest_admission",
            "ingest_max_batch_rows",
            "ingest_max_batch_bytes",
            "publish_max_body_bytes",
            "max_artifacts_per_request",
            "max_members_per_request",
            "max_excluded_per_request",
            "wal_hard_limit_bytes",
            "overlay_soft_limit",
            "flush_max_age_secs",
        ];
        let serve_keys = [
            "row_projection_cache_bytes",
            "masked_count_cache_bytes",
            "occupancy_cache_bytes",
            "fragment_cache_bytes",
            "expected_concurrent_sessions",
        ];
        assert_eq!(
            ingest_keys.len() + serve_keys.len(),
            17,
            "there are seventeen write-path, artifact-plane and admission knobs; this table must \
             cover all of them"
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
            valid_toml_with("", "expected_concurrent_sessions = 42"),
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
             (token_id, view, segments_version), so a second view or a generation swap doubles \
             the entries at unchanged session concurrency"
        );
    }
}
