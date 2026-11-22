//! muxel Cloud state never leaks into the layout doc that peers exchange over
//! SSH. The iOS app decodes that doc strictly, so a workspace's team binding,
//! its task links and a project's cloud overrides must stay local.

use muxel_core::cloud::{ProjectCloud, Tristate, WorkspaceCloud};
use muxel_core::{Instance, Project, RemoteLayout, Workspace};
use uuid::Uuid;

#[test]
fn cloud_fields_stay_out_of_the_shared_doc() {
    let mut p = Project::new("proj", "/srv/app");
    p.remote = Some(muxel_core::RemoteRef {
        host_id: Uuid::new_v4(),
        remote_root: "/srv/app".to_string(),
    });
    let baseline = RemoteLayout::capture(&p, &Workspace::default(), 100).to_json();

    p.cloud = ProjectCloud {
        share_activity: Tristate::Off,
        tasks: Tristate::On,
    };
    let mut ws = Workspace {
        cloud: Some(WorkspaceCloud {
            team_id: Uuid::new_v4(),
            team_name: "Team".into(),
            share_activity: true,
            tasks_enabled: true,
        }),
        ..Workspace::default()
    };
    let inst = Instance::shell(p.id);
    ws.task_links.insert(inst.id, Uuid::new_v4());
    ws.set_task_launch_hash(inst.id, Some([42; 32]));
    let target =
        muxel_core::tmux::SessionTarget::new("fixture", Some(muxel_core::tmux::Socket::Private));
    ws.task_tmux_launches.insert(
        inst.id,
        muxel_core::tmux::task_launch::TaskLaunch::new(target),
    );
    let doc = RemoteLayout::capture(&p, &ws, 100).to_json();

    assert_eq!(doc, baseline);
    for key in [
        "cloud",
        "task_links",
        "task_launch_hashes",
        "task_tmux_launches",
        "team_id",
        "share_activity",
    ] {
        assert!(!doc.contains(key), "{key} leaked into the layout doc");
    }
}

#[test]
fn defaults_add_nothing_to_workspace_json() {
    let ws = Workspace::default();
    let json = serde_json::to_string(&ws).unwrap();
    assert!(!json.contains("\"cloud\""));
    assert!(!json.contains("task_links"));
    assert!(!json.contains("task_launch_hashes"));
    assert!(!json.contains("task_tmux_launches"));
    let p = Project::new("p", "/tmp/p");
    assert!(!serde_json::to_string(&p).unwrap().contains("\"cloud\""));
}
