//! `dedupe_instances` — the load-time repair for a workspace that ended up with two
//! panes on one tmux session.
//!
//! Two instances bound to the same session both attach to it as tmux clients: they
//! mirror each other keystroke for keystroke while muxel treats them as separate
//! agents, and closing either kills the session under the other. It happened for
//! real — two concurrent remote connects each adopted the host's running sessions —
//! and a workspace that got into the state must not reproduce it every launch.

use muxel_core::{Instance, LeafData, PaneNode, Project, Uuid, Workspace, dedupe_instances};

/// A project whose layout holds `instances` as tabs in one leaf.
fn project_with(instances: &[Uuid]) -> Project {
    let mut p = Project::new("proj", "/tmp/proj");
    p.layout = Some(PaneNode::Leaf(LeafData {
        tabs: instances.to_vec(),
        active: 0,
    }));
    p
}

fn instance(project: Uuid, session: Option<&str>) -> Instance {
    let mut i = Instance::shell(project);
    i.tmux_session = session.map(str::to_string);
    i
}

#[test]
fn a_second_instance_on_the_same_session_is_dropped_from_both_the_list_and_the_layout() {
    let mut ws = Workspace::default();
    let mut proj = project_with(&[]);
    let pid = proj.id;

    let first = instance(pid, Some("muxel_sro_client_90f9def0"));
    let dup = instance(pid, Some("muxel_sro_client_90f9def0"));
    let other = instance(pid, Some("muxel_sro_client_d0d464c4"));
    proj.layout = Some(PaneNode::Leaf(LeafData {
        tabs: vec![first.id, dup.id, other.id],
        active: 1, // the duplicate is focused — removing it must not leave a hole
    }));
    let (first_id, dup_id, other_id) = (first.id, dup.id, other.id);
    ws.projects.push(proj);
    ws.instances = vec![first, dup, other];

    dedupe_instances(&mut ws);

    assert_eq!(
        ws.instances.iter().map(|i| i.id).collect::<Vec<_>>(),
        [first_id, other_id],
        "the first claim on a session wins; the second is dropped"
    );
    let tabs = match ws.projects[0].layout.as_ref() {
        Some(PaneNode::Leaf(l)) => l.tabs.clone(),
        other => panic!("expected one leaf, got {other:?}"),
    };
    assert_eq!(tabs, [first_id, other_id], "and its pane leaves the layout");
    assert!(!tabs.contains(&dup_id));
}

/// The same row twice — a workspace written while two code paths mutated it. Keep
/// one copy: its id is still referenced by the layout, so the pane must survive.
#[test]
fn a_repeated_id_collapses_to_one_and_keeps_its_pane() {
    let mut ws = Workspace::default();
    let inst = instance(Uuid::new_v4(), Some("muxel_p_1a2b3c4d"));
    let mut proj = project_with(&[inst.id]);
    proj.id = inst.project_id;
    ws.projects.push(proj);
    ws.instances = vec![inst.clone(), inst.clone()];

    dedupe_instances(&mut ws);

    assert_eq!(
        ws.instances.iter().map(|i| i.id).collect::<Vec<_>>(),
        [inst.id],
        "one copy kept — not both dropped"
    );
    let tabs = match ws.projects[0].layout.as_ref() {
        Some(PaneNode::Leaf(l)) => l.tabs.clone(),
        other => panic!("expected one leaf, got {other:?}"),
    };
    assert_eq!(
        tabs,
        [inst.id],
        "the surviving copy still answers to its pane"
    );
}

#[test]
fn instances_without_a_session_are_never_deduped_against_each_other() {
    let mut ws = Workspace::default();
    let pid = Uuid::new_v4();
    let (a, b) = (instance(pid, None), instance(pid, None));
    let mut proj = project_with(&[a.id, b.id]);
    proj.id = pid;
    ws.projects.push(proj);
    ws.instances = vec![a, b];

    dedupe_instances(&mut ws);

    assert_eq!(
        ws.instances.len(),
        2,
        "no session, no claim, nothing to dedupe"
    );
}

/// The mirror image of a duplicate: an instance the layout never got. It shows in no
/// pane, so it can't be seen or closed — yet it still owns its tmux session, which
/// keeps that session from being re-adopted. It must get a pane back.
#[test]
fn an_instance_missing_from_the_layout_gets_a_pane_back() {
    let mut ws = Workspace::default();
    let pid = Uuid::new_v4();
    let (seen, lost1, lost2) = (
        instance(pid, Some("muxel_p_00000001")),
        instance(pid, Some("muxel_p_00000002")),
        instance(pid, Some("muxel_p_00000003")),
    );
    let (seen_id, lost1_id, lost2_id) = (seen.id, lost1.id, lost2.id);
    let mut proj = project_with(&[seen_id]); // only the first is in the tree
    proj.id = pid;
    ws.projects.push(proj);
    ws.instances = vec![seen, lost1, lost2];

    dedupe_instances(&mut ws);

    let tabs = match ws.projects[0].layout.as_ref() {
        Some(PaneNode::Leaf(l)) => l.tabs.clone(),
        other => panic!("expected one leaf, got {other:?}"),
    };
    assert_eq!(
        tabs,
        [seen_id, lost1_id, lost2_id],
        "the stranded instances join the existing pane as tabs"
    );
    assert_eq!(ws.instances.len(), 3, "and none of them is dropped");
}

/// A project with instances but no layout at all still gets panes.
#[test]
fn an_empty_layout_is_seeded_from_its_instances() {
    let mut ws = Workspace::default();
    let pid = Uuid::new_v4();
    let (a, b) = (
        instance(pid, Some("muxel_p_00000001")),
        instance(pid, Some("muxel_p_00000002")),
    );
    let (a_id, b_id) = (a.id, b.id);
    let mut proj = Project::new("proj", "/tmp/proj");
    proj.id = pid;
    proj.layout = None;
    ws.projects.push(proj);
    ws.instances = vec![a, b];

    dedupe_instances(&mut ws);

    let tabs = match ws.projects[0].layout.as_ref() {
        Some(PaneNode::Leaf(l)) => l.tabs.clone(),
        other => panic!("expected one leaf, got {other:?}"),
    };
    assert_eq!(tabs, [a_id, b_id]);
}

/// The same session name in two different projects is two different sessions —
/// on two different hosts, most likely. Never collapse across projects.
#[test]
fn the_same_session_name_in_two_projects_is_left_alone() {
    let mut ws = Workspace::default();
    let (p1, p2) = (Uuid::new_v4(), Uuid::new_v4());
    let a = instance(p1, Some("muxel_p_1a2b3c4d"));
    let b = instance(p2, Some("muxel_p_1a2b3c4d"));
    let mut proj1 = project_with(&[a.id]);
    proj1.id = p1;
    let mut proj2 = project_with(&[b.id]);
    proj2.id = p2;
    ws.projects.push(proj1);
    ws.projects.push(proj2);
    ws.instances = vec![a, b];

    dedupe_instances(&mut ws);

    assert_eq!(ws.instances.len(), 2);
}
