//! The store's public paths follow `MUXEL_USER_DATA_DIR`, which is what
//! `muxel --user-data-dir DIR` sets at startup.
//!
//! A test binary of its own, with a single test, because it changes a variable
//! the whole process reads.

use std::path::PathBuf;

#[test]
fn every_store_path_moves_under_the_user_data_dir() {
    let root = std::env::temp_dir().join(format!("muxel-store-user-data-{}", std::process::id()));
    // SAFETY (every env change here): this binary runs only this test, so no
    // other thread reads the environment while it changes. The variable may be
    // inherited, from a muxel pane, so it goes first.
    unsafe { std::env::remove_var(muxel_store::USER_DATA_DIR_ENV) };
    let platform_config = muxel_store::config_dir();
    let platform_data = muxel_store::data_dir();
    assert_ne!(platform_data, Some(root.join("data")));

    unsafe { std::env::set_var(muxel_store::USER_DATA_DIR_ENV, &root) };
    let config = root.join("config");
    let data = root.join("data");
    assert_eq!(muxel_store::config_dir(), Some(config.clone()));
    assert_eq!(muxel_store::data_dir(), Some(data.clone()));
    let under = |path: Option<PathBuf>, dir: &PathBuf| {
        let path = path.expect("a path");
        assert!(
            path.starts_with(dir),
            "{} is not under {}",
            path.display(),
            dir.display()
        );
    };
    under(muxel_store::settings_path(), &config);
    under(muxel_store::workspace_path(), &data);
    under(muxel_store::workspaces_dir(), &data);
    under(muxel_store::workspaces_index_path(), &data);
    under(muxel_store::window_geom_path(), &data);
    under(muxel_store::event_log_path(), &data);
    under(muxel_store::models_dir(), &data);

    // Empty is the same as unset: the platform's directories.
    unsafe { std::env::set_var(muxel_store::USER_DATA_DIR_ENV, "") };
    assert_eq!(muxel_store::config_dir(), platform_config);
    assert_eq!(muxel_store::data_dir(), platform_data);
    unsafe { std::env::remove_var(muxel_store::USER_DATA_DIR_ENV) };
    // Some of the paths above create their directory.
    let _ = std::fs::remove_dir_all(&root);
    assert!(!root.exists(), "{} is still there", root.display());
}
