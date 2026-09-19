//! What `Engine::open` refuses in an `EngineConfig`.

mod common;

use tempfile::TempDir;

use tessera_engine::{Engine, EngineConfig, EngineError};

use common::*;

fn open(config: EngineConfig) -> Result<Engine, EngineError> {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
}

#[test]
fn a_config_that_draws_nothing_in_a_sparse_tile_does_not_open() {
    let refused = open(EngineConfig { k_min: 0, ..config() });
    assert!(matches!(refused, Err(EngineError::ConfigRefused(_))));
}

#[test]
fn a_config_whose_merge_or_coalesce_could_never_select_does_not_open() {
    for width in [0, 1] {
        let refused = open(EngineConfig { tier_width: Some(width), ..config() });
        assert!(matches!(refused, Err(EngineError::ConfigRefused(_))), "tier_width {width}");
        let refused = open(EngineConfig { coalesce_width: Some(width), ..config() });
        assert!(matches!(refused, Err(EngineError::ConfigRefused(_))), "coalesce_width {width}");
    }
}

#[test]
fn the_smallest_lawful_values_open() {
    let opened = open(EngineConfig {
        k_min: 1,
        tier_width: Some(2),
        coalesce_width: Some(2),
        ..config()
    });
    assert!(opened.is_ok());
}
