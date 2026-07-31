//! `tessera` — one binary for the build pipeline and (later) the server (SA §3: Rust is the
//! implementation language for engine, build and serving alike).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tessera_spatial::Extent;
use tessera_store::manifest::identity_key_fingerprint;
use tessera_types::{IdentityKey, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

#[derive(Parser)]
#[command(name = "tessera", about = "Tessera: a permission-masked point service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // `Build` carries the identity-key flags (Task 7); the
                                     // enum is parsed once per process invocation, never hot.
enum Command {
    /// Build a bundle from a points file and a `(entity_id, term_id)` pairs file.
    Build {
        /// Parquet points file: `entity_id` plus either `x`/`y` or `morton`.
        #[arg(long)]
        points: PathBuf,
        /// Parquet pairs file: `(entity_id, term_id)`.
        #[arg(long)]
        pairs: PathBuf,
        /// Bundle root to create.
        #[arg(long)]
        out: PathBuf,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent)]
        extent: Extent,
        /// Slice identifier for this build's segment.
        #[arg(long = "slice")]
        slice_id: String,
        /// Keep only source rows with `entity_id < LIMIT` (a prefix of entity space).
        #[arg(long)]
        limit: Option<u64>,
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

        /// Carry `identity.key` and `identity.epoch` forward from an existing bundle's
        /// MANIFEST.json. **This is the normal rebuild path** (contracts §2.2).
        #[arg(long, value_name = "BUNDLE_ROOT")]
        carry_id_key_from: Option<PathBuf>,
        /// Read the deployment's identity key from a config file named explicitly on the
        /// command line (owner ruling Q6) — `[identity]\nkey = "<32 lowercase hex>"`, plus an
        /// optional `epoch = <n>`. There is no default search path and no environment variable:
        /// the path must always be typed.
        #[arg(long, value_name = "PATH")]
        id_key_file: Option<PathBuf>,
        /// Use the given 32-lowercase-hex-character key directly. Discouraged in practice — a
        /// key on a command line reaches shell history, process listings and CI logs; prefer
        /// `--id-key-file`.
        #[arg(long, value_name = "HEX32")]
        id_key: Option<String>,
        /// Explicitly mint a fresh 16-byte key from the OS CSPRNG at epoch 1 and print it
        /// prominently. **Starts a NEW identity lineage; every `tessera_id` any client holds
        /// becomes wrong.**
        #[arg(long)]
        mint_id_key: bool,
        /// Required to proceed when a key was carried or supplied *and* it disagrees with
        /// another given source. **Invalidates every `tessera_id` any client holds, and — since
        /// the key is now part of the storage sort key — reorders every row.**
        #[arg(long)]
        rotate_id_key: bool,
        /// Advance `identity.epoch` while keeping the key: the repartitioning/resharding signal
        /// (contracts §2a).
        #[arg(long)]
        bump_id_epoch: bool,
        /// Set `identity.epoch` explicitly. Accompanies **any** key source — `--id-key`,
        /// `--id-key-file` (whose `[identity].epoch`, if present, it overrides) or
        /// `--carry-id-key-from` (whose carried epoch it overrides) — and is the only way to
        /// state an epoch for a key source that records none. Default 1.
        #[arg(long)]
        epoch: Option<u32>,
    },
    /// Verify a bundle: the read protocol (digests, manifests) plus permutation bijectivity and
    /// the identity column (contracts §2.6 r6: `tessera_id` re-derived from the key).
    Verify {
        /// Bundle root (the directory containing `CURRENT`).
        bundle: PathBuf,
    },
    /// Serve a bundle: the three HTTP planes (viewer/session/control), per `tessera.toml`.
    Serve {
        /// Path to `tessera.toml` (SA §7).
        #[arg(short = 'c', long = "config")]
        config: PathBuf,
    },
}

fn parse_extent(raw: &str) -> Result<Extent, String> {
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
    let extent = Extent {
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
/// preserve its redacted `Debug`), and the epoch this build's MANIFEST should record.
struct ResolvedIdentity {
    key: IdentityKey,
    hex: String,
    epoch: u32,
    /// Set only by `--mint-id-key`, so the caller can print it prominently — the one and only
    /// place a freshly minted key is ever surfaced.
    minted: bool,
}

/// The N-1 refusal message (contracts §2.2, plan Critical N-1): named once so the CLI's refusal
/// and every test asserting it read the same text.
fn no_key_decision_message() -> String {
    "no identity key decision: pass --carry-id-key-from <bundle> to keep this deployment's \
     lineage (the normal rebuild), --id-key-file <path> to read this deployment's key from its \
     config file, --id-key <32 hex> to restore a recorded key, or --mint-id-key to start a new \
     lineage — which invalidates every tessera_id any client holds."
        .to_string()
}

/// Read `identity.key`, `identity.epoch`, `identity.construction` and `identity.rounds`
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
        manifest.identity.epoch,
        manifest.identity.construction,
        manifest.identity.rounds,
    ))
}

/// Read `--id-key-file`'s minimal TOML shape: `[identity]\nkey = "<32 lowercase hex>"`, plus an
/// **optional** `epoch = <u32>`.
///
/// **The epoch belongs in this file** (contracts §2.2 designates it as the key's home outside the
/// bundle and leaves "the file's wider schema … not specified here", so extending it is
/// legitimate). Without it, a deployment that advanced to epoch 2 for a repartition and then
/// rebuilt from its key file — the spec's own recommended rebuild path — republished epoch 1, and
/// a stale pre-repartition `tessera_id` then compared *equal* and was accepted: exactly the
/// failure §2.2 says the epoch exists to prevent. A key file that records no epoch still means
/// epoch 1 (the lineage never advanced), and `--epoch` overrides whatever the file says.
///
/// Unknown top-level sections are ignored (so a later phase's wider deployment config file can
/// grow without breaking this binary); an unknown key *inside* `[identity]` is an error, so a
/// misspelt `kye =` does not fall through to a refusal that reads "no key given". There is no
/// default search path — the caller always names this path explicitly, which is the only reason
/// this flag counts as an explicit decision under N-1.
fn read_id_key_file(path: &Path) -> Result<(String, Option<u32>), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("--id-key-file {}: {e}", path.display()))?;
    let value: toml::Value = text
        .parse()
        .map_err(|e| format!("--id-key-file {}: invalid TOML: {e}", path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("--id-key-file {}: not a TOML table", path.display()))?;
    let identity = table.get("identity").ok_or_else(|| {
        format!(
            "--id-key-file {}: missing [identity] section",
            path.display()
        )
    })?;
    let identity_table = identity.as_table().ok_or_else(|| {
        format!(
            "--id-key-file {}: [identity] must be a table",
            path.display()
        )
    })?;
    for key_name in identity_table.keys() {
        if key_name != "key" && key_name != "epoch" {
            return Err(format!(
                "--id-key-file {}: unknown key '{key_name}' in [identity] (expected 'key' or \
                 'epoch')",
                path.display()
            ));
        }
    }
    let key = identity_table
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "--id-key-file {}: [identity].key is missing or not a string",
                path.display()
            )
        })?;
    let epoch = match identity_table.get("epoch") {
        None => None,
        Some(value) => {
            let raw = value.as_integer().ok_or_else(|| {
                format!(
                    "--id-key-file {}: [identity].epoch must be an integer",
                    path.display()
                )
            })?;
            // §2.2: conforming writers start at 1 and advance; 0 (or a value past `u32`) is a
            // config error, and `IdentityDescriptor::validate` would refuse it at read time
            // anyway — refuse it here, where the operator can still see which file said it.
            let epoch = u32::try_from(raw).map_err(|_| {
                format!(
                    "--id-key-file {}: [identity].epoch {raw} is out of range for a u32",
                    path.display()
                )
            })?;
            if epoch == 0 {
                return Err(format!(
                    "--id-key-file {}: [identity].epoch is 0; conforming writers start at 1 and \
                     advance (contracts §2.2)",
                    path.display()
                ));
            }
            Some(epoch)
        }
    };
    Ok((key.to_string(), epoch))
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

/// Resolve the deployment identity key from the CLI's four sources, applying every refusal rule
/// contracts §2.2/§2a specifies. Called, and must fail, **before any build work starts** — no
/// output directory, no input read (plan Critical N-1).
#[allow(clippy::too_many_arguments)]
fn resolve_identity(
    carry_id_key_from: &Option<PathBuf>,
    id_key_file: &Option<PathBuf>,
    id_key: &Option<String>,
    mint_id_key: bool,
    rotate_id_key: bool,
    bump_id_epoch: bool,
    epoch_flag: Option<u32>,
) -> Result<ResolvedIdentity, String> {
    let mut sources: Vec<(&'static str, String)> = Vec::new();
    let mut carried_epoch: Option<u32> = None;
    let mut file_epoch: Option<u32> = None;

    if let Some(root) = carry_id_key_from {
        let (hex, epoch, construction, rounds) = read_carried_identity(root)?;
        if construction != IDENTITY_CONSTRUCTION || rounds != IDENTITY_ROUNDS {
            return Err(format!(
                "--carry-id-key-from {}: identity construction/rounds ({construction}, {rounds}) \
                 differ from this binary's ({IDENTITY_CONSTRUCTION}, {IDENTITY_ROUNDS}); refusing \
                 rather than silently deriving every identifier under a different construction \
                 while the key looks unchanged",
                root.display()
            ));
        }
        sources.push(("--carry-id-key-from", hex));
        carried_epoch = Some(epoch);
    }
    if let Some(path) = id_key_file {
        let (hex, epoch) = read_id_key_file(path)?;
        sources.push(("--id-key-file", hex));
        file_epoch = epoch;
    }
    if let Some(hex) = id_key {
        sources.push(("--id-key", hex.clone()));
    }

    if sources.is_empty() && !mint_id_key {
        return Err(no_key_decision_message());
    }

    if mint_id_key {
        if !sources.is_empty() {
            return Err(
                "--mint-id-key cannot be combined with --carry-id-key-from / --id-key-file / \
                 --id-key: minting starts a NEW lineage, it does not restore one"
                    .to_string(),
            );
        }
        let (key, hex) = mint_identity_key();
        return Ok(ResolvedIdentity {
            key,
            hex,
            epoch: 1,
            minted: true,
        });
    }

    // Validate every source's hex up front (typed errors naming the source), before comparing.
    let mut parsed: Vec<(&'static str, String)> = Vec::with_capacity(sources.len());
    for (label, hex) in sources {
        IdentityKey::from_hex(&hex).map_err(|e| format!("{label}: {e}"))?;
        parsed.push((label, hex));
    }

    let first_hex = parsed[0].1.clone();
    let disagreement = parsed.iter().any(|(_, hex)| *hex != first_hex);
    if disagreement && !rotate_id_key {
        // Fingerprints, never the keys themselves: this message goes to stderr on a CLI whose own
        // `--id-key` documentation warns that a key on a command line reaches shell history,
        // process listings and CI logs — printing both disagreeing keys in full would put them
        // there through the *refusal* path as well. A fingerprint is enough to tell an operator
        // which source is the odd one out, which is all the message needs to do.
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

    // On a confirmed rotation, `--id-key` (the most explicit source) wins if given, else
    // `--id-key-file`, else the sole remaining source. Agreement makes this choice moot.
    let final_hex = if disagreement {
        id_key
            .clone()
            .or_else(|| {
                id_key_file
                    .as_ref()
                    .and_then(|_| parsed.iter().find(|(l, _)| *l == "--id-key-file"))
                    .map(|(_, hex)| hex.clone())
            })
            .unwrap_or(first_hex)
    } else {
        first_hex
    };
    let final_key = IdentityKey::from_hex(&final_hex).map_err(|e| format!("identity key: {e}"))?;

    // Epoch resolution, most explicit source first: `--epoch`, then the key file's own
    // `[identity].epoch`, then the epoch carried out of an existing bundle, then 1.
    //
    // The order matters for the reason `--id-key-file` exists: it is *the* home for a
    // deployment's key (contracts §2.2), so a normal rebuild from it must not silently republish
    // epoch 1 after the deployment advanced to 2 for a repartition — a stale pre-repartition
    // `tessera_id` would then compare equal and be accepted, which is precisely the failure the
    // epoch prevents. Two *recorded* epochs that disagree are refused rather than silently
    // ranked: whichever we picked, the other could be the true one, and getting it wrong is
    // fail-open. `--epoch` is how the operator resolves that.
    let mut epoch = if disagreement {
        // A rotation resets the epoch (contracts §2.2), unless the operator also supplied an
        // explicit --epoch to accompany the new key.
        epoch_flag.unwrap_or(1)
    } else {
        if let (None, Some(carried), Some(from_file)) = (epoch_flag, carried_epoch, file_epoch) {
            if carried != from_file {
                return Err(format!(
                    "identity epoch sources disagree (--carry-id-key-from={carried}, \
                     --id-key-file={from_file}); pass --epoch <n> to state which epoch this \
                     build publishes — guessing risks republishing a superseded epoch, under \
                     which a stale pre-repartition tessera_id compares equal and is accepted"
                ));
            }
        }
        epoch_flag.or(file_epoch).or(carried_epoch).unwrap_or(1)
    };
    if bump_id_epoch {
        epoch += 1;
    }

    // NOT IMPLEMENTED, deliberately, and flagged rather than built: contracts §2.2 also requires
    // a build whose **partitioning or sharding differs** from the bundle it carried the key from
    // to advance the epoch *or refuse*. Nothing here checks that, because nothing here can
    // differ: Phase 1 emits exactly one partition (`default`) and shard 0, both hard-coded in
    // `tessera_build` (`PHASH`, `shard_id`). The refusal becomes reachable — and required — the
    // moment either becomes a build input; it belongs next to this epoch resolution, comparing
    // this build's partition/shard plan against `--carry-id-key-from`'s manifest and refusing
    // unless `--bump-id-epoch` (or an explicit `--epoch`) accompanies the change.

    Ok(ResolvedIdentity {
        key: final_key,
        hex: final_hex,
        epoch,
        minted: false,
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            points,
            pairs,
            out,
            extent,
            slice_id,
            limit,
            mint_external_ids,
            no_oracle_pairs,
            carry_id_key_from,
            id_key_file,
            id_key,
            mint_id_key,
            rotate_id_key,
            bump_id_epoch,
            epoch,
        } => {
            // CRITICAL N-1: resolved and refused, if it refuses, before any work — before `df`,
            // before reading input, before creating the output directory.
            let identity = match resolve_identity(
                &carry_id_key_from,
                &id_key_file,
                &id_key,
                mint_id_key,
                rotate_id_key,
                bump_id_epoch,
                epoch,
            ) {
                Ok(identity) => identity,
                Err(detail) => {
                    eprintln!("build refused: {detail}");
                    return ExitCode::FAILURE;
                }
            };
            if identity.minted {
                eprintln!(
                    "minted a new identity key (epoch 1): {} — starts a NEW identity lineage; \
                     every tessera_id any client holds becomes wrong. Record this key (e.g. via \
                     --id-key-file's deployment config) so future rebuilds can carry it forward.",
                    identity.hex
                );
            }

            let args = tessera_build::BuildArgs {
                points,
                pairs,
                out: out.clone(),
                extent,
                slice_id,
                limit,
                identity_key: identity.key,
                identity_key_hex: identity.hex,
                identity_epoch: identity.epoch,
                shard_id: 0,
                mint_external_ids,
                emit_oracle_pairs: !no_oracle_pairs,
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
        Command::Verify { bundle } => match tessera_build::verify(&bundle) {
            Ok(report) => {
                println!(
                    "OK {} ({}): {} partition(s), {} slice(s), {} segment(s), {} rows, \
                     entity_id_high_water {}",
                    bundle.display(),
                    report.prefix,
                    report.partitions,
                    report.slices,
                    report.segments,
                    report.rows,
                    report.entity_id_high_water
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("FAILED {}: {e}", bundle.display());
                ExitCode::FAILURE
            }
        },
        Command::Serve { config } => {
            tracing_subscriber::fmt::init();
            let prepared = match tessera_server::prepare(&config) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("tessera serve: refused to start: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let runtime = match tokio::runtime::Builder::new_multi_thread()
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
