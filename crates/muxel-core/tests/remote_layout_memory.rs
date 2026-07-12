//! The shared-memory flag in the layout doc peers exchange over SSH.
//!
//! `.muxel/MEMORY.md` lives at the project root on the host, and every agent working
//! there — desktop's panes, the iOS app's — reads and writes the same file. So
//! whether shared memory is *on* belongs to the project, not to whichever machine
//! last looked at it: without it in the doc, each client kept its own idea and a
//! project with memory plainly in use still showed the toggle off elsewhere.

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
    p.memory_enabled = true;

    let doc = RemoteLayout::capture(&p, &ws, 100);
    assert_eq!(doc.memory_enabled, Some(true));

    let parsed = RemoteLayout::parse(&doc.to_json(), "/srv/app").expect("round-trips");
    assert_eq!(parsed.memory_enabled, Some(true), "and survives the wire");
}

/// A doc written before the field existed must decode as "no opinion" — *not* as
/// `false`. Read as `false`, the first sync from an older peer would quietly switch
/// shared memory off for everyone.
#[test]
fn an_older_doc_has_no_opinion_rather_than_saying_off() {
    let json = r#"{
        "version": 1,
        "updated_at": 100,
        "remote_root": "/srv/app",
        "layout": null,
        "instances": [],
        "worktrees": []
    }"#;
    let parsed = RemoteLayout::parse(json, "/srv/app").expect("still a valid doc");
    assert_eq!(parsed.memory_enabled, None);
}

/// Flipping the toggle has to count as a change, or it would never be pushed to the
/// host and the other clients would never hear about it.
#[test]
fn toggling_memory_changes_the_content_key() {
    let ws = Workspace::default();
    let mut off = project();
    off.memory_enabled = false;
    let mut on = project();
    on.id = off.id;
    on.memory_enabled = true;

    let key_off = RemoteLayout::capture(&off, &ws, 100).content_key();
    let key_on = RemoteLayout::capture(&on, &ws, 100).content_key();
    assert_ne!(key_off, key_on);
}
