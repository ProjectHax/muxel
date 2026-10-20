//! The accomplishment-log flag in the layout doc peers exchange over SSH.
//!
//! Like shared memory, `.muxel/ACCOMPLISHMENTS.md` lives at the project root on the
//! host and every agent there appends to the same file, so whether the log is on
//! belongs to the project, not to whichever machine last looked at it.

use muxel_core::{Project, RemoteLayout, Workspace};

fn project() -> Project {
    let mut p = Project::new("proj", "/srv/app");
    p.remote = Some(muxel_core::RemoteRef {
        host_id: uuid::Uuid::new_v4(),
        remote_root: "/srv/app".to_string(),
    });
    p
}

#[test]
fn the_flag_travels_with_the_project() {
    let ws = Workspace::default();
    let mut p = project();
    p.accomplishments_enabled = true;

    let doc = RemoteLayout::capture(&p, &ws, 100);
    assert_eq!(doc.accomplishments_enabled, Some(true));

    let parsed = RemoteLayout::parse(&doc.to_json(), "/srv/app").expect("round-trips");
    assert_eq!(parsed.accomplishments_enabled, Some(true));
}

/// A doc from before the field (an older desktop, or the iOS app before its port)
/// means "no opinion", so it can't switch the log off for everyone.
#[test]
fn an_older_doc_has_no_opinion_rather_than_saying_off() {
    let json = r#"{
        "version": 1,
        "updated_at": 100,
        "remote_root": "/srv/app",
        "layout": null,
        "instances": [],
        "worktrees": [],
        "memory_enabled": true
    }"#;
    let parsed = RemoteLayout::parse(json, "/srv/app").expect("still a valid doc");
    assert_eq!(parsed.accomplishments_enabled, None);
    assert_eq!(parsed.memory_enabled, Some(true));
}

/// Flipping the toggle counts as a change, so it's pushed to the host.
#[test]
fn toggling_the_log_changes_the_content_key() {
    let ws = Workspace::default();
    let off = project();
    let mut on = project();
    on.id = off.id;
    on.accomplishments_enabled = true;

    let key_off = RemoteLayout::capture(&off, &ws, 100).content_key();
    let key_on = RemoteLayout::capture(&on, &ws, 100).content_key();
    assert_ne!(key_off, key_on);
}
