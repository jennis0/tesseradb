//! `tessera` — one binary for the build pipeline and (later) the server (SA §3: Rust is the
//! implementation language for engine, build and serving alike).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tessera_spatial::Bounds;
use tessera_store::manifest::identity_key_fingerprint;
use tessera_types::{IdentityKey, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

#[derive(Parser)]
#[command(name = "tessera", about = "Tessera: a permission-masked point service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // `Build` carries the identity-key flags; the enum is
                                     // parsed once per process invocation, never hot.
enum Command {
    /// Build a bundle from this deployment's corpus declaration and the sources it names.
    ///
    /// **`tessera build`, with no flags at all, is the whole invocation.** `tessera.toml` — found
    /// by walking up from the working directory — says where the declaration is and where the
    /// bundle goes; the declaration says where the corpus is and what frame it is quantised
    /// against; the environment carries the identity key. Everything below is an override or a
    /// performance knob (`configuration.md` §3).
    Build {
        /// This deployment's `tessera.toml`, instead of the one found by walking up from the
        /// working directory.
        #[arg(long, value_name = "PATH")]
        deployment: Option<PathBuf>,
        /// Bundle root to create, overriding `tessera.toml`'s `bundle.path`.
        ///
        /// **The same value the server's `bundle_path` is**, seen from the other side, which is
        /// why it is declared once rather than typed twice: a build and a server naming different
        /// directories is a server serving whatever was there before.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Which view to materialise. Omit where the declaration has exactly one — the ordinary
        /// case — and a declaration with several refuses rather than choosing.
        #[arg(long = "view")]
        view_id: Option<String>,
        /// Keep only source rows with `entity_id < LIMIT` (a prefix of entity space).
        #[arg(long)]
        limit: Option<u64>,
        /// The corpus declaration, overriding `tessera.toml`'s `build.schema`: one TOML document
        /// declaring the corpus, its views, its vocabularies, its attributes and its layers
        /// (configuration.md §1).
        ///
        /// **A build input, never server configuration** (configuration.md §4). It compiles into
        /// MANIFEST.json and the server reads the compiled form, so a server cannot be restarted
        /// against a bundle whose columns disagree with a schema it holds.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Read one of the declaration's sources from somewhere else: `--file KEY=PATH`,
        /// repeatable (configuration.md §8).
        ///
        /// **An override, not a binding.** Every `source` in the declaration is already a path,
        /// relative to the declaration itself, so the ordinary build names no files here at all.
        /// This is for the deployment that stages one source elsewhere, and KEY is the *object*
        /// whose source it replaces — `corpus`, `view:s0`, `view:s0:point_visibility`,
        /// `vocabulary:severity`, `layer:clusters/a`, `layer:clusters/a:members`.
        ///
        /// Fail-closed both ways: a key no object declares is a refusal listing the ones that
        /// exist, and an override never *creates* a source — so a closed vocabulary cannot be
        /// opened, nor a view given geometry, from the command line alone.
        ///
        /// Note the interaction with `--limit` for a member source: a member outside the limited
        /// prefix names nothing this build assigned, and refuses it. Limit the members file with
        /// the corpus.
        #[arg(long = "file", value_name = "KEY=PATH", value_parser = parse_file_binding)]
        file: Vec<(String, PathBuf)>,
        /// Mint an external ID for every item from its source entity id, and write the
        /// external-id extents and locator. **Off by default**: contracts §2.4 forbids
        /// manufacturing an external ID for an item whose caller supplied none, and this
        /// build's inputs carry none — the flag exists so benchmark fixtures can keep carrying
        /// the sidecar's cost realistically (2026-07-30 memo §3.2 D1).
        #[arg(long)]
        mint_external_ids: bool,
        /// Skip writing `pairs.parquet`. The file serves only the test-time reference oracle
        /// and build-cadence tooling — nothing on any request path reads it — so a deployment
        /// that runs no conformance suite against the bundle can save writing and hashing it.
        #[arg(long)]
        no_oracle_pairs: bool,
        /// Signature-sort batch size in items (design §11.1 r23: assignment is
        /// signature-sorted per batch). Omit to derive the largest batch the memory budget
        /// supports — usually the whole corpus in one batch. Whatever is used is recorded in
        /// MANIFEST provenance when it batches, and is **identity-bearing**: a rebuild
        /// preserving this corpus's identity must replay the recorded value
        /// (`--carry-id-key-from` does so automatically).
        #[arg(long)]
        batch_items: Option<u64>,
        /// Peak-memory budget for the build's own structures, e.g. `24g`, `900m` or bytes.
        /// Omit to derive from the machine's available memory. Batch and band sizing, and the
        /// fail-closed pre-flight, all follow from it.
        #[arg(long, value_parser = parse_byte_size)]
        memory_budget: Option<u64>,

        /// Carry `identity.key` and `identity.idset` forward from an existing bundle's
        /// MANIFEST.json. **This is the normal rebuild path** (contracts §2.2).
        #[arg(long, value_name = "BUNDLE_ROOT")]
        carry_id_key_from: Option<PathBuf>,
        /// Read the deployment's identity key from a file — `[identity]\nkey = "<32 lowercase
        /// hex>"`, plus an optional `idset = <n>`.
        ///
        /// The ordinary route is the environment: `tessera.toml`'s `[identity] env` names the
        /// variable (default `TESSERA_IDENTITY_KEY`), and a `.env` beside `tessera.toml` may
        /// supply it. This flag is for the deployment that would rather keep the key in a `0600`
        /// file, which an environment — readable from `/proc` — is not.
        #[arg(long, value_name = "PATH")]
        identity_file: Option<PathBuf>,
        /// Explicitly mint a fresh 16-byte key from the OS CSPRNG at idset 1 and print it
        /// prominently. **Starts a NEW identity lineage; every `tessera_id` any client holds
        /// becomes wrong.**
        #[arg(long)]
        mint_id_key: bool,
        /// Required to proceed when a key was carried or supplied *and* it disagrees with
        /// another given source. **Invalidates every `tessera_id` any client holds, and — since
        /// the key is now part of the storage sort key — reorders every row.**
        #[arg(long)]
        rotate_id_key: bool,
        /// Advance `identity.idset` while keeping the key: the repartitioning/resharding signal
        /// (contracts §2a).
        #[arg(long)]
        bump_idset: bool,
        /// Set `identity.idset` explicitly. Accompanies **any** key source — the environment,
        /// `--identity-file` (whose `[identity].idset`, if present, it overrides) or
        /// `--carry-id-key-from` (whose carried idset it overrides) — and is the only way to
        /// state an idset for a key source that records none. Default 1.
        #[arg(long)]
        idset: Option<u32>,
    },
    /// Verify a bundle: the read protocol (digests, manifests) plus permutation bijectivity and
    /// the identity column (contracts §2.6: `tessera_id` re-derived from the key).
    Verify {
        /// Bundle root (the directory containing `CURRENT`).
        bundle: PathBuf,
        /// Also run the deep structural pass (correctness-suite §11): postings sorted,
        /// duplicate-free and bounded; the external-id locator and its sidecar agreeing in both
        /// directions; dictionary extents positional and never repeating a descriptor; and
        /// `pairs.parquet` matching the base postings it was written with. Point it at a bundle
        /// no live engine is publishing into.
        #[arg(long)]
        deep: bool,
    },
    /// Analyse text through the shipped tokeniser, one input per line, tokens tab-separated.
    ///
    /// **The conformance oracle's access to the analyser** (`records-and-search.md` §4.4). The
    /// oracle derives expected `match` results from the fixture's own values and must pass them
    /// through the *same* pipeline the index was built with; reimplementing it in Python would
    /// test PyICU's ICU4C against icu4x rather than testing Tessera. This verb is that access, on
    /// the harness's existing drive-the-CLI precedent.
    ///
    /// It is also how a client diagnoses an empty `match`: `/v1/meta` publishes each text column's
    /// analyser identity (`declared_scalars[].analyser`), and running the query text through the
    /// analyser that identity names shows whether it segmented the way the index did.
    Tokenise {
        /// Text to analyse. Repeatable. With none given, reads one input per line from stdin.
        #[arg(long = "text")]
        text: Vec<String>,
        /// Which analyser, by declared name (decision 0070). A column records the identity this
        /// resolves to — `/v1/meta` publishes it per column — and an unknown name is refused
        /// rather than defaulted.
        #[arg(long, default_value = tessera_analyse::UNICODE)]
        analyser: String,
        /// Print the analyser's identity and exit — what a column records, and what a rebuild moves.
        #[arg(long)]
        identity: bool,
    },
    /// The correctness suite's generated corpus: expected items by served join key, and expected
    /// masked counts per tile (`docs/design/correctness-suite.md` §8, §12.1).
    ///
    /// **The suite's access to the generator**, on the `tokenise` precedent: expected answers must
    /// come from the same statement of the corpus the fixtures were materialised from, and a
    /// Python reimplementation would put the fixture under test rather than the system. The verb
    /// granularity is deliberate — one `items` call per recorded response and one `census` per
    /// run, never a call per row — so the O(n) work stays in Rust and the driver compares vectors.
    Corpus {
        #[command(subcommand)]
        command: CorpusCommand,
    },
    /// Serve a bundle: the three HTTP planes (viewer/session/control), per `tessera.toml`.
    ///
    /// Takes no required flag: the same `tessera.toml` `tessera build` wrote into names the
    /// bundle to open (`configuration.md` §3).
    Serve {
        /// This deployment's `tessera.toml`, instead of the one found by walking up from the
        /// working directory (SA §7).
        #[arg(long, value_name = "PATH")]
        deployment: Option<PathBuf>,
    },
}

/// The corpus extent both verbs default to: the cell grid's own coordinates, which is what the
/// conformance fixtures build against (contracts §2.5 — the grid is 2^16 × 2^16).
const GRID_EXTENT: &str = "0,65536,0,65536";

#[derive(Subcommand)]
enum CorpusCommand {
    /// Expected properties for served rows: `fx_key` values in (one decimal per line), an Arrow
    /// IPC stream out on stdout — each key's item identity, position and every declared field.
    ///
    /// Takes no `--n`, and that absence is the point: an item's properties depend on the seed and
    /// the item alone (the generator is prefix-stable), so the verb answers for any key a
    /// response carried without being told how large the corpus was. The keys are `fx_key`
    /// values — item identities — never `tessera_id`s, which invert to entity ids and say nothing
    /// about items (correctness-suite §8).
    Items {
        /// The run's seed.
        #[arg(long)]
        seed: u64,
        /// Where the fx_key values come from: `-` for stdin, else a file path.
        #[arg(long)]
        ids: PathBuf,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent, default_value = GRID_EXTENT)]
        extent: Bounds,
    },
    /// Expected masked count per depth-`zoom` tile for one principal: an Arrow IPC stream on
    /// stdout, `(tile, count)` ascending, non-empty tiles only.
    ///
    /// Per tile rather than one global total, because a single number passes any defect that
    /// moves rows between tiles while preserving the sum (correctness-suite §9.2). The counts are
    /// what the *corpus* holds: denies the harness has had accepted are its own to subtract.
    Census {
        /// The run's seed.
        #[arg(long)]
        seed: u64,
        /// The corpus size — the one derivation-free use of n: the census's loop bound.
        #[arg(long)]
        n: u64,
        /// Tile depth, 0..=16.
        #[arg(long)]
        zoom: u8,
        /// The principal's grant, in the mask catalogue's term-set encoding: comma-separated
        /// decimal term descriptors (the `builtin:passthrough` label form).
        #[arg(long)]
        grant: String,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent, default_value = GRID_EXTENT)]
        extent: Bounds,
    },
}

fn parse_extent(raw: &str) -> Result<Bounds, String> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "expected four comma-separated numbers 'x_min,x_max,y_min,y_max', got '{raw}'"
        ));
    }
    let mut values = [0f64; 4];
    for (slot, text) in values.iter_mut().zip(parts) {
        *slot = text
            .parse::<f64>()
            .map_err(|e| format!("'{text}' is not a number: {e}"))?;
    }
    let extent = Bounds {
        x_min: values[0],
        x_max: values[1],
        y_min: values[2],
        y_max: values[3],
    };
    extent.validate()?;
    Ok(extent)
}

/// What [`resolve_identity`] decided: the parsed key, its canonical hex form (carried alongside
/// the key rather than recovered from it — `IdentityKey` deliberately has no hex accessor, to
/// preserve its redacted `Debug`), and the idset this build's MANIFEST should record.
struct ResolvedIdentity {
    key: IdentityKey,
    hex: String,
    idset: u32,
    /// Set only by `--mint-id-key`, so the caller can print it prominently — the one and only
    /// place a freshly minted key is ever surfaced.
    minted: bool,
}

/// The N-1 refusal message (contracts §2.2, plan Critical N-1): named once so the CLI's refusal
/// and every test asserting it read the same text.
///
/// `env_name` is whatever `tessera.toml`'s `[identity] env` names, because a message telling an
/// operator to set `TESSERA_IDENTITY_KEY` when their own file named something else is worse than
/// no message.
fn no_key_decision_message(env_name: &str) -> String {
    format!(
        "no identity key decision: set ${env_name} (or put it in a .env beside tessera.toml) to \
         this deployment's key, pass --carry-id-key-from <bundle> to keep its lineage from an \
         existing bundle (the normal rebuild), pass --identity-file <path> to read it from a 0600 \
         file, or pass --mint-id-key to start a new lineage — which invalidates every tessera_id \
         any client holds."
    )
}

/// The identity key as the environment supplies it: the process environment first, then a `.env`
/// beside `tessera.toml`.
///
/// **The process environment wins.** A `.env` is a convenience for a working copy; an operator who
/// exported a variable for one invocation has said something more specific than a file checked in
/// beside the config, and a file quietly overriding them would be the wrong way round.
///
/// **The `.env` parse is written out here rather than taken as a dependency.** It is `KEY=VALUE`
/// per line, `#` comments and blanks skipped, an optional `export ` prefix, and one layer of
/// matching quotes stripped — which is the whole of what this file is for. A crate would bring
/// variable interpolation, multi-line values and `.env.local` layering, none of which anything
/// here reads, into the process that holds the identity key.
fn identity_from_environment(env_name: &str, deployment: &Path) -> Option<(String, String)> {
    if let Ok(value) = std::env::var(env_name) {
        if !value.trim().is_empty() {
            return Some((value.trim().to_string(), format!("${env_name}")));
        }
    }
    let dotenv = deployment.parent().unwrap_or(Path::new("")).join(".env");
    let text = std::fs::read_to_string(&dotenv).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != env_name {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if value.is_empty() {
            return None;
        }
        return Some((
            value.to_string(),
            format!("{} ({env_name})", dotenv.display()),
        ));
    }
    None
}

/// Read `identity.key`, `identity.idset`, `identity.construction` and `identity.rounds`
/// verbatim from `bundle_root`'s current `MANIFEST.json` (contracts §2.2's `--carry-id-key-from`
/// behaviour). A direct JSON read rather than the full digest-verifying read protocol: carrying
/// a key forward needs the manifest's own claims, not a re-verification of every segment file.
fn read_carried_identity(bundle_root: &Path) -> Result<(String, u32, String, u32), String> {
    let current_path = bundle_root.join("CURRENT");
    let current_bytes = std::fs::read(&current_path)
        .map_err(|e| format!("--carry-id-key-from {}: {e}", current_path.display()))?;
    let current: tessera_store::manifest::CurrentPointer = serde_json::from_slice(&current_bytes)
        .map_err(|e| {
        format!(
            "--carry-id-key-from {}: CURRENT is not valid JSON: {e}",
            current_path.display()
        )
    })?;
    let manifest_path = bundle_root.join(&current.prefix).join("MANIFEST.json");
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|e| format!("--carry-id-key-from {}: {e}", manifest_path.display()))?;
    let manifest: tessera_store::manifest::Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| {
            format!(
                "--carry-id-key-from {}: MANIFEST.json does not parse (does it predate the r6 \
                 identity column?): {e}",
                manifest_path.display()
            )
        })?;
    Ok((
        manifest.identity.key,
        manifest.identity.idset,
        manifest.identity.construction,
        manifest.identity.rounds,
    ))
}

/// Read `--identity-file`'s minimal TOML shape: `[identity]\nkey = "<32 lowercase hex>"`, plus an
/// **optional** `idset = <u32>`.
///
/// **The idset belongs in this file** (contracts §2.2 designates a file as one home for the key
/// bundle and leaves "the file's wider schema … not specified here", so extending it is
/// legitimate). Without it, a deployment that advanced to idset 2 for a repartition and then
/// rebuilt from its key file — the spec's own recommended rebuild path — republished idset 1, and
/// a stale pre-repartition `tessera_id` then compared *equal* and was accepted: exactly the
/// failure §2.2 says the idset exists to prevent. A key file that records no idset still means
/// idset 1 (the lineage never advanced), and `--idset` overrides whatever the file says.
///
/// Unknown top-level sections are ignored (so a later phase's wider deployment config file can
/// grow without breaking this binary); an unknown key *inside* `[identity]` is an error, so a
/// misspelt `kye =` does not fall through to a refusal that reads "no key given". There is no
/// default search path — the caller always names this path explicitly, which is the only reason
/// this flag counts as an explicit decision under N-1.
fn read_identity_file(path: &Path) -> Result<(String, Option<u32>), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("--identity-file {}: {e}", path.display()))?;
    let value: toml::Value = text
        .parse()
        .map_err(|e| format!("--identity-file {}: invalid TOML: {e}", path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("--identity-file {}: not a TOML table", path.display()))?;
    let identity = table.get("identity").ok_or_else(|| {
        format!(
            "--identity-file {}: missing [identity] section",
            path.display()
        )
    })?;
    let identity_table = identity.as_table().ok_or_else(|| {
        format!(
            "--identity-file {}: [identity] must be a table",
            path.display()
        )
    })?;
    for key_name in identity_table.keys() {
        if key_name != "key" && key_name != "idset" {
            return Err(format!(
                "--identity-file {}: unknown key '{key_name}' in [identity] (expected 'key' or \
                 'idset')",
                path.display()
            ));
        }
    }
    let key = identity_table
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "--identity-file {}: [identity].key is missing or not a string",
                path.display()
            )
        })?;
    let idset = match identity_table.get("idset") {
        None => None,
        Some(value) => {
            let raw = value.as_integer().ok_or_else(|| {
                format!(
                    "--identity-file {}: [identity].idset must be an integer",
                    path.display()
                )
            })?;
            // §2.2: conforming writers start at 1 and advance; 0 (or a value past `u32`) is a
            // config error, and `IdentityDescriptor::validate` would refuse it at read time
            // anyway — refuse it here, where the operator can still see which file said it.
            let idset = u32::try_from(raw).map_err(|_| {
                format!(
                    "--identity-file {}: [identity].idset {raw} is out of range for a u32",
                    path.display()
                )
            })?;
            if idset == 0 {
                return Err(format!(
                    "--identity-file {}: [identity].idset is 0; conforming writers start at 1 and \
                     advance (contracts §2.2)",
                    path.display()
                ));
            }
            Some(idset)
        }
    };
    Ok((key.to_string(), idset))
}

/// Draw a fresh 16-byte key from the OS CSPRNG, retrying on a degenerate draw (`k1 == 0`,
/// including the all-zero key — memo §1.3). The **only** place randomness enters the build.
fn mint_identity_key() -> (IdentityKey, String) {
    use rand::RngCore;
    loop {
        let mut bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if let Ok(key) = IdentityKey::from_hex(&hex) {
            return (key, hex);
        }
        // A degenerate draw is vanishingly rare (k1 == 0 out of a uniform 64-bit half) — retry.
    }
}

/// The carried bundle's recorded signature-batch size, if its build batched at all (absent
/// key == one batch — pre-batching manifests never carry it).
fn read_carried_batch_items(bundle_root: &Path) -> Result<Option<u64>, String> {
    let current_path = bundle_root.join("CURRENT");
    let current_bytes = std::fs::read(&current_path)
        .map_err(|e| format!("--carry-id-key-from {}: {e}", current_path.display()))?;
    let current: tessera_store::manifest::CurrentPointer = serde_json::from_slice(&current_bytes)
        .map_err(|e| {
        format!(
            "--carry-id-key-from {}: CURRENT is not valid JSON: {e}",
            current_path.display()
        )
    })?;
    let manifest_path = bundle_root.join(&current.prefix).join("MANIFEST.json");
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|e| format!("--carry-id-key-from {}: {e}", manifest_path.display()))?;
    let manifest: tessera_store::manifest::Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("--carry-id-key-from {}: {e}", manifest_path.display()))?;
    Ok(manifest
        .provenance
        .get("batch_items")
        .and_then(|v| v.as_u64()))
}

/// `24g` / `512m` / `1073741824` — the human forms a budget is actually typed in.
fn parse_byte_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (digits, multiplier) = match value.chars().last() {
        Some('g') | Some('G') => (&value[..value.len() - 1], 1u64 << 30),
        Some('m') | Some('M') => (&value[..value.len() - 1], 1u64 << 20),
        Some('k') | Some('K') => (&value[..value.len() - 1], 1u64 << 10),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .map_err(|e| format!("not a byte size: {e}"))?
        .checked_mul(multiplier)
        .ok_or_else(|| "byte size overflows u64".to_string())
}

// Eight parameters, and they are eight *decisions*: three key sources, the variable's name for
// the message, and the four flags that say what to do with what they produce. Grouping them into
// a struct would hide which of them a given refusal is about, which is the one thing every message
// in here has to say.
/// Resolve the deployment identity key from its four sources — the environment (or a `.env`),
/// `--identity-file`, `--carry-id-key-from`, and `--mint-id-key` — applying every refusal rule
/// contracts §2.2/§2a specifies. Called, and must fail, **before any build work starts**: no
/// output directory, no input read (plan Critical N-1).
///
/// **There is no flag that takes a key.** One on a command line reaches shell history, process
/// listings and CI logs, so the ordinary route is a variable `tessera.toml` names and the
/// alternative is a `0600` file, which an environment — readable from `/proc` — is not.
#[allow(clippy::too_many_arguments)]
fn resolve_identity(
    carry_id_key_from: &Option<PathBuf>,
    identity_file: &Option<PathBuf>,
    from_environment: Option<(String, String)>,
    env_name: &str,
    mint_id_key: bool,
    rotate_id_key: bool,
    bump_idset: bool,
    idset_flag: Option<u32>,
) -> Result<ResolvedIdentity, String> {
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut carried_idset: Option<u32> = None;
    let mut file_idset: Option<u32> = None;

    if let Some(root) = carry_id_key_from {
        let (hex, idset, construction, rounds) = read_carried_identity(root)?;
        if construction != IDENTITY_CONSTRUCTION || rounds != IDENTITY_ROUNDS {
            return Err(format!(
                "--carry-id-key-from {}: identity construction/rounds ({construction}, {rounds}) \
                 differ from this binary's ({IDENTITY_CONSTRUCTION}, {IDENTITY_ROUNDS}); refusing \
                 rather than silently deriving every identifier under a different construction \
                 while the key looks unchanged",
                root.display()
            ));
        }
        sources.push(("--carry-id-key-from".to_string(), hex));
        carried_idset = Some(idset);
    }
    if let Some(path) = identity_file {
        let (hex, idset) = read_identity_file(path)?;
        sources.push(("--identity-file".to_string(), hex));
        file_idset = idset;
    }
    // The environment is a *source*, not a fallback: if it disagrees with a carried lineage the
    // build refuses, exactly as two flags disagreeing would. A key that silently lost to another
    // source would be the one shape of this flow that produces a bundle nobody chose.
    let from_environment_hex = from_environment.as_ref().map(|(hex, _)| hex.clone());
    if let Some((hex, label)) = from_environment {
        sources.push((label, hex));
    }

    if sources.is_empty() && !mint_id_key {
        return Err(no_key_decision_message(env_name));
    }

    if mint_id_key {
        if !sources.is_empty() {
            return Err(format!(
                "--mint-id-key cannot be combined with --carry-id-key-from, --identity-file or a \
                 key in ${env_name}: minting starts a NEW lineage, it does not restore one. Unset \
                 the variable for this invocation if a fresh lineage is what is wanted"
            ));
        }
        let (key, hex) = mint_identity_key();
        return Ok(ResolvedIdentity {
            key,
            hex,
            idset: 1,
            minted: true,
        });
    }

    // Validate every source's hex up front (typed errors naming the source), before comparing.
    let mut parsed: Vec<(String, String)> = Vec::with_capacity(sources.len());
    for (label, hex) in sources {
        IdentityKey::from_hex(&hex).map_err(|e| format!("{label}: {e}"))?;
        parsed.push((label, hex));
    }

    let first_hex = parsed[0].1.clone();
    let disagreement = parsed.iter().any(|(_, hex)| *hex != first_hex);
    if disagreement && !rotate_id_key {
        // Fingerprints, never the keys themselves. A key printed in full reaches shell history,
        // process listings and CI logs — which is exactly why there is no flag that takes one —
        // and a refusal path that printed both disagreeing keys would put them there anyway. A
        // fingerprint is enough to tell an operator which source is the odd one out, which is all
        // the message needs to do.
        let described = parsed
            .iter()
            .map(|(label, hex)| format!("{label}={}", identity_key_fingerprint(hex)))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "identity key sources disagree ({described}); pass --rotate-id-key to confirm the \
             rotation — this invalidates every tessera_id any client holds and reorders every \
             row, since the key is now part of the storage sort key"
        ));
    }

    // On a confirmed rotation, `--identity-file` (the source a person typed a path for) wins if
    // given, then the environment, then the sole remaining source. Agreement makes this moot.
    let final_hex = if disagreement {
        parsed
            .iter()
            .find(|(l, _)| l == "--identity-file")
            .map(|(_, hex)| hex.clone())
            .or(from_environment_hex)
            .unwrap_or(first_hex)
    } else {
        first_hex
    };
    let final_key = IdentityKey::from_hex(&final_hex).map_err(|e| format!("identity key: {e}"))?;

    // Idset resolution, most explicit source first: `--idset`, then the key file's own
    // `[identity].idset`, then the idset carried out of an existing bundle, then 1.
    //
    // The order matters for the reason a key has a home outside the bundle at all (contracts
    // §2.2): a normal rebuild from that home must not silently republish
    // idset 1 after the deployment advanced to 2 for a repartition — a stale pre-repartition
    // `tessera_id` would then compare equal and be accepted, which is precisely the failure the
    // idset prevents. Two *recorded* idsets that disagree are refused rather than silently
    // ranked: whichever we picked, the other could be the true one, and getting it wrong is
    // fail-open. `--idset` is how the operator resolves that.
    let mut idset = if disagreement {
        // A rotation resets the idset (contracts §2.2), unless the operator also supplied an
        // explicit --idset to accompany the new key.
        idset_flag.unwrap_or(1)
    } else {
        if let (None, Some(carried), Some(from_file)) = (idset_flag, carried_idset, file_idset) {
            if carried != from_file {
                return Err(format!(
                    "idset sources disagree (--carry-id-key-from={carried}, \
                     --identity-file={from_file}); pass --idset <n> to state which idset this \
                     build publishes — guessing risks republishing a superseded idset, under \
                     which a stale pre-repartition tessera_id compares equal and is accepted"
                ));
            }
        }
        idset_flag.or(file_idset).or(carried_idset).unwrap_or(1)
    };
    if bump_idset {
        idset += 1;
    }

    // NOT IMPLEMENTED, deliberately, and flagged rather than built: contracts §2.2 also requires
    // a build whose **partitioning or sharding differs** from the bundle it carried the key from
    // to advance the idset *or refuse*. Nothing here checks that, because nothing here can
    // differ: this build emits exactly one partition (`default`) and shard 0, both hard-coded in
    // `tessera_build` (`PHASH`, `shard_id`). The refusal becomes reachable — and required — the
    // moment either becomes a build input; it belongs next to this idset resolution, comparing
    // this build's partition/shard plan against `--carry-id-key-from`'s manifest and refusing
    // unless `--bump-idset` (or an explicit `--idset`) accompanies the change.

    Ok(ResolvedIdentity {
        key: final_key,
        hex: final_hex,
        idset,
        minted: false,
    })
}

/// Find and read this deployment's `tessera.toml`, returning the path it was found at beside the
/// configuration itself (`configuration.md` §3).
///
/// The path comes back because two things are relative to it and to nothing else: the `.env` that
/// may carry the identity key, and the paths inside the file.
fn load_deployment(
    explicit: Option<&Path>,
) -> Result<(PathBuf, tessera_server::config::Config), String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = tessera_server::config::discover(explicit, &cwd).map_err(|e| e.to_string())?;
    let config = tessera_server::config::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, config))
}

/// The `--file` bindings as one map, refusing a key bound twice.
///
/// **Last-one-wins is not available to a binding**: the two paths are two corpora, and a build that
/// silently took the second would produce a bundle nobody can tell from one built on the first.
fn collect_bindings(
    file: Vec<(String, PathBuf)>,
) -> Result<std::collections::HashMap<String, PathBuf>, String> {
    let mut bindings: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::with_capacity(file.len());
    for (key, path) in file {
        if let Some(already) = bindings.get(&key) {
            return Err(format!(
                "--file bound '{key}' twice, to {} and to {}. Which file a source names is not a \
                 last-one-wins question: the two are two different corpora",
                already.display(),
                path.display()
            ));
        }
        bindings.insert(key, path);
    }
    Ok(bindings)
}

/// `--file KEY=PATH`. Split at the **first** `=` so a path may contain one.
fn parse_file_binding(raw: &str) -> Result<(String, PathBuf), String> {
    let (key, path) = raw.split_once('=').ok_or_else(|| {
        format!(
            "--file expects KEY=PATH, got '{raw}' (no '=' — the key is the one a `source` in the \
             config names, the path is the file it binds to)"
        )
    })?;
    if key.is_empty() || path.is_empty() {
        return Err(format!(
            "--file '{raw}': both the key and the path must be non-empty"
        ));
    }
    Ok((key.to_string(), PathBuf::from(path)))
}

/// Print the schema's residency cost against the corpus about to be built (§2.3).
///
/// **Totalled, not per column.** Several categories are what makes the cost bite, and a
/// per-attribute table lets each one look affordable on its own. §10.5 prices a hot column at
/// 0.93 GiB per byte per row per 10⁹ items, which is what the projection below reproduces.
///
/// Warns on what §2.3 names as breaking in practice — which is now one thing, not three.
///
/// `discovered` + `u8` is unreachable while discovered vocabularies are refused at parse, and
/// `render_in` is refused outright, so its every-view consequence is stated once rather than per
/// column.
///
/// **Dense codes under `listing = "per_viewer"` is deliberately not warned about** (owner ruling,
/// 2026-08-07), and this is worth recording because §2.3 asks for the warning and a reader will
/// otherwise add it. That warning guards vocabulary *cardinality* — a visible code being a lower
/// bound on how many values exist. The owner does not hold cardinality as a threat. What must be
/// enforced is the other half: **a principal may see a category value only if it belongs to data
/// they can see** — §3.3's membership-derived visibility, which is a property of the read path,
/// not of how an author numbered their codes. A cardinality warning here would be mechanism that
/// looks like access control and is not.
fn report_residency(schema: &tessera_build::config::Schema, limit: Option<u64>) {
    let columns = schema.attributes.len();
    match schema.row_bits() {
        Some(per_row_bits) => {
            // Reported in bytes because that is the unit §10.5 prices a column in, but computed
            // from bits and shown to two places: a schema of `bool`s costs a real fraction of a
            // byte per row, and rounding it to zero would report the cheap case as free.
            let per_row = per_row_bits as f64 / 8.0;
            eprintln!(
                "schema: {columns} column(s), {per_row:.2} B/row against the 12 B fixed row \
                 (+{:.0}%)",
                per_row / 12.0 * 100.0
            );
            if let Some(rows) = limit {
                let gib = per_row * rows as f64 / (1024.0 * 1024.0 * 1024.0);
                eprintln!("        {gib:.2} GiB resident at {rows} items");
            }
            eprintln!(
                "        {:.2} GiB per 10^9 items — unalterable without rewriting the corpus",
                per_row * 0.93
            );
        }
        None => eprintln!("schema: {columns} column(s), variable width per row"),
    }
    eprintln!(
        "        every column materialises in EVERY view — including ones whose items carry no \
         value for it (§3.9). Per-view columns need contracts §2.6's per-view enumeration"
    );
}

/// `tessera corpus items` (correctness-suite §12.1): served `fx_key` values in, their expected
/// items out. The corpus is constructed with `n = 0` because the lookups take no part in it —
/// see the verb's own doc.
fn corpus_items(seed: u64, ids: &Path, extent: Bounds) -> ExitCode {
    use arrow::array::{
        ArrayRef, Float32Builder, StringBuilder, TimestampMicrosecondBuilder, UInt32Builder,
        UInt64Builder,
    };
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let corpus = match tessera_corpus::Corpus::new(seed, 0, extent) {
        Ok(corpus) => corpus,
        Err(e) => {
            eprintln!("corpus items: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = if ids == Path::new("-") {
        use std::io::Read;
        let mut buffer = String::new();
        if let Err(e) = std::io::stdin().lock().read_to_string(&mut buffer) {
            eprintln!("corpus items: reading stdin: {e}");
            return ExitCode::FAILURE;
        }
        buffer
    } else {
        match std::fs::read_to_string(ids) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("corpus items: {}: {e}", ids.display());
                return ExitCode::FAILURE;
            }
        }
    };

    let mut fx_key = UInt64Builder::new();
    let mut e_col = UInt64Builder::new();
    let mut x = Float32Builder::new();
    let mut y = Float32Builder::new();
    let mut weight = UInt32Builder::new();
    let mut seen_at = TimestampMicrosecondBuilder::new();
    let mut bay = StringBuilder::new();
    let mut tag = StringBuilder::new();
    let mut blurb = StringBuilder::new();
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(fx) = line.parse::<u64>() else {
            // Refused rather than skipped: a non-decimal line means the caller is feeding this
            // verb something other than fx_key values, and a silently shortened answer would be
            // compared as if it were complete.
            eprintln!(
                "corpus items: line {}: '{line}' is not a decimal fx_key",
                line_no + 1
            );
            return ExitCode::FAILURE;
        };
        let e = corpus.item_of_fx_key(fx);
        let item = corpus.item(e);
        fx_key.append_value(fx);
        e_col.append_value(e);
        x.append_value(item.x);
        y.append_value(item.y);
        weight.append_option(item.weight);
        seen_at.append_option(item.seen_at);
        bay.append_option(item.bay);
        tag.append_option(item.tag.as_deref());
        blurb.append_option(item.blurb.as_deref());
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("fx_key", DataType::UInt64, false),
        Field::new("e", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("weight", DataType::UInt32, true),
        Field::new(
            "seen_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("bay", DataType::Utf8, true),
        Field::new("tag", DataType::Utf8, true),
        Field::new("blurb", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(fx_key.finish()) as ArrayRef,
            Arc::new(e_col.finish()),
            Arc::new(x.finish()),
            Arc::new(y.finish()),
            Arc::new(weight.finish()),
            Arc::new(seen_at.finish()),
            Arc::new(bay.finish()),
            Arc::new(tag.finish()),
            Arc::new(blurb.finish()),
        ],
    )
    .expect("columns built to one length from one loop");
    let schema = batch.schema();
    write_arrow_stdout(&schema, &[batch], "corpus items")
}

/// `tessera corpus census` (correctness-suite §9.2, §12.1): the expected masked count per tile,
/// computed in one O(n) pass here so the driver compares two count vectors.
fn corpus_census(seed: u64, n: u64, zoom: u8, grant: &str, extent: Bounds) -> ExitCode {
    use arrow::array::{ArrayRef, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    if zoom > 16 {
        eprintln!("corpus census: zoom {zoom} exceeds grid depth 16 (contracts §2.5)");
        return ExitCode::FAILURE;
    }
    let grant = match tessera_corpus::Grant::parse(grant) {
        Ok(grant) => grant,
        Err(e) => {
            eprintln!("corpus census: {e}");
            return ExitCode::FAILURE;
        }
    };
    let corpus = match tessera_corpus::Corpus::new(seed, n, extent) {
        Ok(corpus) => corpus,
        Err(e) => {
            eprintln!("corpus census: {e}");
            return ExitCode::FAILURE;
        }
    };
    let counts = corpus.census(zoom, &grant);

    let schema = Arc::new(Schema::new(vec![
        Field::new("tile", DataType::UInt64, false),
        Field::new("count", DataType::UInt64, false),
    ]));
    // Chunked: at deep zooms the tile vector can run to millions of rows, and a bounded batch
    // size keeps the stream's consumers (and this process's transient) flat.
    let batches: Vec<RecordBatch> = counts
        .chunks(1 << 16)
        .map(|chunk| {
            let tiles = UInt64Array::from_iter_values(chunk.iter().map(|(tile, _)| *tile));
            let totals = UInt64Array::from_iter_values(chunk.iter().map(|(_, count)| *count));
            RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(tiles) as ArrayRef, Arc::new(totals)],
            )
            .expect("two columns of one chunk's length")
        })
        .collect();
    write_arrow_stdout(&schema, &batches, "corpus census")
}

/// One Arrow IPC stream on stdout. An empty batch list still writes a valid stream carrying only
/// the schema — "no tiles" must be distinguishable from "no output".
fn write_arrow_stdout(
    schema: &std::sync::Arc<arrow::datatypes::Schema>,
    batches: &[arrow::record_batch::RecordBatch],
    verb: &str,
) -> ExitCode {
    let out = std::io::stdout().lock();
    let write = || -> Result<(), arrow::error::ArrowError> {
        let mut writer = arrow::ipc::writer::StreamWriter::try_new(out, schema)?;
        for batch in batches {
            writer.write(batch)?;
        }
        writer.finish()
    };
    match write() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{verb}: writing Arrow stream: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            deployment,
            out,
            view_id,
            limit,
            config,
            file,
            mint_external_ids,
            no_oracle_pairs,
            batch_items,
            memory_budget,
            carry_id_key_from,
            identity_file,
            mint_id_key,
            rotate_id_key,
            bump_idset,
            idset,
        } => {
            // **The deployment file first, because everything else is read through it**: where the
            // declaration is, where the bundle goes, and which environment variable carries the
            // key. A missing one is a refusal naming what to create (configuration.md §3) —
            // never a silent set of defaults, since every path in it is a decision.
            let (deployment_path, deployment) = match load_deployment(deployment.as_deref()) {
                Ok(pair) => pair,
                Err(detail) => {
                    eprintln!("build refused: {detail}");
                    return ExitCode::FAILURE;
                }
            };
            let schema_path = config.unwrap_or_else(|| deployment.schema_path.clone());
            let out = out.unwrap_or_else(|| deployment.bundle_path.clone());

            // CRITICAL N-1: resolved and refused, if it refuses, before any work — before `df`,
            // before reading input, before creating the output directory.
            let identity = match resolve_identity(
                &carry_id_key_from,
                &identity_file,
                identity_from_environment(&deployment.identity_env, &deployment_path),
                &deployment.identity_env,
                mint_id_key,
                rotate_id_key,
                bump_idset,
                idset,
            ) {
                Ok(identity) => identity,
                Err(detail) => {
                    eprintln!("build refused: {detail}");
                    return ExitCode::FAILURE;
                }
            };
            if identity.minted {
                eprintln!(
                    "minted a new identity key (idset 1): {} — starts a NEW identity lineage; \
                     every tessera_id any client holds becomes wrong. Record this key (e.g. via \
                     ${} in a .env beside tessera.toml) so future rebuilds can carry it forward.",
                    identity.hex, deployment.identity_env
                );
            }

            // Parsed and refused before any work, for the identity key's reason: a declaration
            // refusal is an operator's typo, and discovering it after a multi-minute build has
            // written a bundle prefix costs the whole build. Every rule in
            // `tessera_build::config` fires here, against no data at all.
            let bindings = match collect_bindings(file) {
                Ok(bindings) => bindings,
                Err(detail) => {
                    eprintln!("build refused: {detail}");
                    return ExitCode::FAILURE;
                }
            };
            let config = match tessera_build::config::Config::parse(&schema_path, &bindings) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // **A build materialises one view.** With one declared, naming it is noise; with
            // several, choosing for the operator would publish a coordinate system nobody asked
            // for, so `sole_view` refuses and lists them.
            let view_id = match view_id.map(Ok).unwrap_or_else(|| {
                config.sole_view().map(str::to_string)
            }) {
                Ok(view_id) => view_id,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // The files this build reads, resolved from the declaration and any overrides — and
            // every absence a refusal here rather than an empty read (configuration.md §8).
            let acquired = match config.acquire(&view_id) {
                Ok(acquired) => acquired,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // **The frame, resolved before any work.** `auto` costs a pass over the points file's
            // two coordinate columns; every other spelling is already the answer. Printed either
            // way, because the extent is what every stored cell is relative to and a fitted one is
            // not otherwise visible anywhere.
            let extent = match tessera_build::config::resolve_extent(
                &view_id,
                &acquired.extent,
                &acquired.points,
                &acquired.point_fields,
                limit,
            ) {
                Ok(extent) => extent,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            eprintln!(
                "view '{view_id}': quantising against x [{}, {}], y [{}, {}]",
                extent.x_min, extent.x_max, extent.y_min, extent.y_max
            );
            let schema = config.schema;
            // §2.3: the cost is reported, never hidden — a hot column is baked into every row and
            // is unalterable without rewriting the corpus, so the operator sees the per-row and
            // total figures at the moment they can still change the declaration. Reported and
            // **never refused**: Appendix A is a sizing table with no deployment ceiling, and
            // giving this a refusal means giving Appendix A a ceiling first, which is an owner
            // decision rather than a derivable number.
            if !schema.is_empty() {
                report_residency(&schema, limit);
            }

            // The carried bundle's recorded batch size is identity-bearing exactly like its
            // key: replayed when this rebuild names no size of its own, refused loudly when a
            // conflicting size is given — a different batch size is a different permanent
            // assignment under the same identity key, which is the one silent state this flow
            // must never produce.
            let batch_items = match (&carry_id_key_from, batch_items) {
                (Some(root), passed) => {
                    let carried = match read_carried_batch_items(root) {
                        Ok(carried) => carried,
                        Err(detail) => {
                            eprintln!("build refused: {detail}");
                            return ExitCode::FAILURE;
                        }
                    };
                    match (carried, passed) {
                        (Some(recorded), Some(given)) if recorded != given => {
                            eprintln!(
                                "build refused: --carry-id-key-from {}: that bundle was built \
                                 with --batch-items {recorded}, but {given} was given; an \
                                 identity-preserving rebuild must replay the recorded value \
                                 (drop --batch-items to do so)",
                                root.display()
                            );
                            return ExitCode::FAILURE;
                        }
                        (Some(recorded), _) => Some(recorded),
                        (None, Some(given)) => {
                            eprintln!(
                                "note: the carried bundle was built as a single batch; \
                                 --batch-items {given} makes this build a DIFFERENT permanent \
                                 assignment under the same identity key"
                            );
                            Some(given)
                        }
                        (None, None) => None,
                    }
                }
                (None, passed) => passed,
            };

            let args = tessera_build::BuildArgs {
                points: acquired.points,
                point_fields: acquired.point_fields,
                corpus: acquired.corpus,
                corpus_fields: acquired.corpus_fields,
                access: acquired.access,
                out: out.clone(),
                extent,
                view_id,
                limit,
                identity_key: identity.key,
                identity_key_hex: identity.hex,
                idset: identity.idset,
                shard_id: 0,
                mint_external_ids,
                emit_oracle_pairs: !no_oracle_pairs,
                batch_items,
                memory_budget,
                band_rows: None,
                schema,
                layers: config.layers,
                layer_inputs: acquired.layers,
            };
            match tessera_build::build(&args) {
                Ok(report) => {
                    println!(
                        "built {} ({}): {} items, {} terms, {} pairs, {} bytes on disk",
                        out.display(),
                        report.prefix,
                        report.items,
                        report.terms,
                        report.pairs,
                        report.bundle_bytes
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("build FAILED: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Tokenise {
            text,
            analyser,
            identity,
        } => {
            let Some(analyser) = tessera_analyse::analyser(&analyser) else {
                eprintln!(
                    "'{analyser}' is not an analyser this binary carries. Available: {}",
                    tessera_analyse::ANALYSER_NAMES.join(", ")
                );
                return ExitCode::FAILURE;
            };
            if identity {
                println!("{}", analyser.identity());
                let _ = text;
                return ExitCode::SUCCESS;
            }
            // Tab-separated because a token can contain anything but a tab or a newline: the
            // segmenter's word-like segments never span a line break, and a caller that split on
            // spaces would corrupt nothing here but would elsewhere.
            let emit = |line: &str| println!("{}", analyser.tokens(line).join("\t"));
            if text.is_empty() {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines() {
                    emit(&line.expect("a readable line on stdin"));
                }
            } else {
                for line in &text {
                    emit(line);
                }
            }
            ExitCode::SUCCESS
        }
        Command::Verify { bundle, deep } => {
            let print_shallow = |report: &tessera_build::VerifyReport| {
                println!(
                    "OK {} ({}): {} partition(s), {} view(s), {} segment(s), {} rows, \
                     entity_id_high_water {}",
                    bundle.display(),
                    report.prefix,
                    report.partitions,
                    report.views,
                    report.segments,
                    report.rows,
                    report.entity_id_high_water
                );
            };
            if deep {
                match tessera_build::verify_deep(&bundle, &tessera_build::VerifyOpts::default()) {
                    Ok(report) => {
                        print_shallow(&report.shallow);
                        println!(
                            "deep: {} term(s), {} delta tier(s), {} pairs row(s), {} dict \
                             record(s), {} external-id binding(s)",
                            report.terms,
                            report.delta_tiers,
                            report.pairs_rows,
                            report.dict_records,
                            report.external_id_bindings
                        );
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("FAILED {}: {e}", bundle.display());
                        ExitCode::FAILURE
                    }
                }
            } else {
                match tessera_build::verify(&bundle) {
                    Ok(report) => {
                        print_shallow(&report);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("FAILED {}: {e}", bundle.display());
                        ExitCode::FAILURE
                    }
                }
            }
        }
        Command::Corpus { command } => match command {
            CorpusCommand::Items { seed, ids, extent } => corpus_items(seed, &ids, extent),
            CorpusCommand::Census {
                seed,
                n,
                zoom,
                grant,
                extent,
            } => corpus_census(seed, n, zoom, &grant, extent),
        },
        Command::Serve { deployment } => {
            tracing_subscriber::fmt::init();
            let deployment = match tessera_server::config::discover(
                deployment.as_deref(),
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            ) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("tessera serve: refused to start: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let prepared = match tessera_server::prepare(&deployment) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("tessera serve: refused to start: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // **The blocking pool is sized here, from the config's declared consumers**, and this
            // is the only place in the process where that number exists.
            //
            // It is sized explicitly rather than left at tokio's default (512) because two things
            // depend on it and neither states it: the viewer plane's `spawn_blocking` closures,
            // bounded by `compute_admission`, and `/control/ingest`'s, bounded by
            // `ingest_admission`. A tokio release or an embedder's own builder could move that
            // default in silence, and an admitted viewport would then queue behind ingest closures
            // in the shared FIFO with no timeout — hanging rather than shedding.
            //
            // Derived rather than asserted-against: `serving_blocking_threads` covers both bounds
            // plus a reserve, so there is no configuration in which an admitted request finds no
            // thread. What `config::load` refuses is a pool the *machine* cannot carry
            // (`SERVING_BLOCKING_THREAD_CEILING`), which is a different question and is already
            // settled by the time this runs — `prepare` returned above.
            let blocking_threads =
                tessera_server::config::serving_blocking_threads(&prepared.config);
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .max_blocking_threads(blocking_threads)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("tessera serve: could not start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match runtime.block_on(tessera_server::run(prepared)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tessera serve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
