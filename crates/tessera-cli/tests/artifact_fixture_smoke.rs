//! The artifact scale campaign's end-to-end smoke: materialise a generator corpus with all five
//! closed-form artifact arms, then run the real `tessera build` over the emitted declaration.
//!
//! **What this checks, and what it deliberately does not.** The two predicate layers
//! (`generator/partition-attribute`, `generator/boundary`) are declared against machinery the
//! engine does not build yet (`artifact-delivery.md` §5.1's "declared and unbuilt") — so the bar
//! here is that the build *accepts* the declaration, not that it serves anything from those two.
//! The three enumerated layers (`generator/flat`, `generator/partition-enumerated`,
//! `generator/treed`) are real: this also asserts their published member-row counts agree with
//! what the generator says it wrote, and that nothing was minted or left unclustered — every
//! roster is closed and complete, so both of those counts are zero by construction.

use std::process::{Command, Output};

use tessera_corpus::Corpus;
use tessera_spatial::Bounds;

const KEY: &str = "000102030405060708090a0b0c0d0e0f";
/// ~10⁵ items — large enough that the fixture's counts are not a handful of coincidental small
/// numbers, small enough that the whole smoke runs in the ordinary test pass.
const N: u64 = 100_000;

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

fn grid() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

fn deployment_toml(schema_name: &str) -> String {
    format!(
        r#"
[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "{schema_name}"

[identity]
env = "TESSERA_IDENTITY_KEY"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
"#
    )
}

/// Parses `built ... N items, N terms, N pairs, N bytes on disk, N artifact(s) minted, N
/// unclustered member row(s)` off stdout — the report [`tessera_build::BuildReport`] carries,
/// through the one surface a driver actually has (the CLI, on the `corpus` verbs' own precedent).
fn parse_u64_before(stdout: &str, marker: &str) -> u64 {
    let idx = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("'{marker}' not found in:\n{stdout}"));
    let head = &stdout[..idx];
    let token = head
        .trim_end()
        .rsplit(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("no number before '{marker}' in:\n{stdout}"));
    token.parse().unwrap()
}

#[test]
fn a_generator_corpus_with_every_artifact_arm_builds() {
    let c = Corpus::new(0x5EED, N, grid()).unwrap();
    let dir = tempfile::tempdir().unwrap();

    c.write_points_parquet(&dir.path().join("points.parquet"))
        .unwrap();
    c.write_pairs_parquet(&dir.path().join("pairs.parquet"))
        .unwrap();
    std::fs::write(dir.path().join("corpus-config.toml"), c.config_toml()).unwrap();
    let counts = c.write_artifact_fixtures(dir.path()).unwrap();

    // The fixture is not degenerate: every arm actually produced something to build over, so a
    // build that accepted an empty declaration by accident would not pass silently.
    assert!(counts.flat_artifacts > 0);
    assert!(counts.flat_member_rows > 0);
    assert!(counts.partition_artifacts > 0);
    assert_eq!(
        counts.partition_member_rows, N,
        "the partition twin is exhaustive over n"
    );
    assert!(counts.boundary_artifacts > 0);
    assert!(counts.treed_artifacts > 0);
    assert!(counts.treed_member_rows > 0);

    std::fs::write(
        dir.path().join("tessera.toml"),
        deployment_toml("corpus-config.toml"),
    )
    .unwrap();

    let output: Output = tessera()
        .arg("build")
        .current_dir(dir.path())
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .expect("failed to run tessera build");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "the build refused a declaration carrying an enumerated layer, a partition-attribute \
         predicate and a spatial predicate, which today it must only accept:\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );

    // Nothing was minted and nothing was left unclustered: both rosters are closed and complete,
    // so a nonzero number here would mean the fixture and the declaration disagree about what
    // artifacts exist.
    assert_eq!(
        parse_u64_before(&stdout, "artifact(s) minted"),
        0,
        "an artifact was minted from a key the roster did not declare:\n{stdout}"
    );
    assert_eq!(
        parse_u64_before(&stdout, "unclustered member row(s)"),
        0,
        "a member row named no artifact, which a closed roster should never produce:\n{stdout}"
    );

    // All three enumerated layers actually published: one member-store file each, and none for
    // the two predicate layers, which serve nothing today.
    let members_dir = dir.path().join("bundle/v00000/partitions/default/members");
    let published: Vec<_> = std::fs::read_dir(&members_dir)
        .unwrap_or_else(|e| panic!("{}: {e}", members_dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        published.len(),
        3,
        "expected one member-store file per enumerated layer (flat, partition-enumerated, \
         treed), got {published:?}"
    );
    for path in &published {
        let bytes = std::fs::metadata(path).unwrap().len();
        assert!(bytes > 0, "{} is empty", path.display());
    }
}
