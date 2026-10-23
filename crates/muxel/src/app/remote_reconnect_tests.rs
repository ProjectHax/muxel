use super::{MuxelApp, launch_token_current};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::tmux::SessionTarget;
use muxel_core::{Instance, PaneNode, Project, RemoteHost, RemoteRef};
use muxel_terminal::{CommandSpec, TerminalLaunch};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

type Reply = anyhow::Result<SessionTarget>;
type Pending = Arc<Mutex<Vec<Option<async_channel::Sender<Reply>>>>>;

thread_local! {
    static PROBES: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

/// Replace only remote I/O. Dispatch, ticks, ownership and result landing still
/// run through the app's production continuation. Every probe stays pending
/// until the test releases its reply; no real SSH process can start.
pub(super) fn next_owner_probe() -> Option<async_channel::Receiver<Reply>> {
    PROBES.with_borrow(|slot| {
        let probes = slot.as_ref()?;
        let (send, receive) = async_channel::bounded(1);
        probes.lock().unwrap().push(Some(send));
        Some(receive)
    })
}

struct Probes(Pending);

impl Probes {
    fn install() -> Self {
        let pending = Arc::new(Mutex::new(Vec::new()));
        PROBES.with_borrow_mut(|slot| {
            assert!(slot.replace(pending.clone()).is_none());
        });
        Self(pending)
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn fail(&self, index: usize, message: &str) {
        self.0.lock().unwrap()[index]
            .take()
            .unwrap()
            .try_send(Err(anyhow::anyhow!(message.to_string())))
            .expect("probe still awaits its reply");
    }
}

impl Drop for Probes {
    fn drop(&mut self) {
        PROBES.with_borrow_mut(|slot| *slot = None);
    }
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>, &Probes)) {
    let root = std::env::temp_dir().join(format!("muxel-remote-reconnect-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    let probes = Probes::install();
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    let app = window.entity(&cx).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut cx, window, &probes)
    }));
    app.update(&mut cx, |app, cx| {
        for view in app.terminals.values() {
            view.read(cx).session().kill();
        }
    });
    drop(app);
    drop(cx);
    drop(probes);
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn exited_remote_pane(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    let pane = window
        .update(cx, |app, window, cx| {
            // Keep unrelated branch polling from launching SSH Git queries
            // while these ticks exercise the controlled owner-probe replies.
            app.remote_branch_poll_inflight = true;
            let root = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&root).unwrap();
            let pid = app.workspace.add_project(Project::new("reconnect", root));
            let instance = Instance::shell(pid);
            let iid = instance.id;
            app.workspace.add_instance(instance);
            app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(iid));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.main_window = Some(window.window_handle());
            app.use_tmux = false;
            app.use_worktree = false;
            let (meta, _, size) = app.prepare_terminal_spawn(iid, window, cx).unwrap();
            let command =
                CommandSpec::program("cmd.exe", vec!["/d".into(), "/c".into(), "exit 37".into()]);
            let launch = TerminalLaunch::spawn_for_instance(command, size, iid).unwrap();
            app.finish_terminal_spawn(meta, Ok(launch), window, cx);
            iid
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Output coalescing may be waiting on GPUI's virtual paint timer even
        // though the real child has already exited.
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        let exited = window
            .read_with(cx, |app, cx| app.terminals[&pane].read(cx).exited())
            .unwrap();
        if exited {
            break;
        }
        assert!(Instant::now() < deadline, "owned fixture did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    window
        .update(cx, |app, window, cx| {
            let host = RemoteHost::new("fixture", "unused.invalid");
            let instance = app.workspace.instance_mut(pane).unwrap();
            instance.use_tmux = true;
            instance.tmux_session = Some("pending-owner".into());
            instance.tmux_socket = None;
            let pid = instance.project_id;
            app.workspace.project_mut(pid).unwrap().remote = Some(RemoteRef {
                host_id: host.id,
                remote_root: "/fixture".into(),
            });
            app.remotes.push(host);
            app.terminal_launches
                .insert(pane, (Instant::now() - Duration::from_secs(120), false));
            app.reconnecting.insert(pane, 64);
            app.spawn_terminal(pane, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    pane
}

#[test]
fn remote_reconnect_owner_probe_slower_than_tick_keeps_its_launch_token() {
    with_app(|cx, window, probes| {
        let iid = exited_remote_pane(cx, window);
        assert_eq!(probes.count(), 1);
        let token = window
            .read_with(cx, |app, _| app.terminal_launching[&iid].token)
            .unwrap();
        window
            .update(cx, |app, _, _| {
                // Releasing admission for other restored panes at 20s must not
                // revoke this probe's ownership of its own recovery.
                app.terminal_launching.get_mut(&iid).unwrap().since =
                    Instant::now() - Duration::from_secs(25);
            })
            .unwrap();
        for _ in 0..3 {
            window
                .update(cx, |app, window, cx| app.tick(window, cx))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                probes.count(),
                1,
                "a tick must not dispatch another owner probe"
            );
            window
                .read_with(cx, |app, _| {
                    assert!(launch_token_current(&app.terminal_launching, iid, token));
                    assert_eq!(app.reconnecting[&iid], 64);
                })
                .unwrap();
        }
        probes.fail(0, "slow owner probe landed");
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(app.failed_launches[&iid].contains("slow owner probe landed"));
                assert!(!app.terminal_launching.contains_key(&iid));
            })
            .unwrap();
    });
}

#[test]
fn remote_reconnect_hung_owner_probe_eventually_yields_to_a_new_launch() {
    with_app(|cx, window, probes| {
        let iid = exited_remote_pane(cx, window);
        let old = window
            .update(cx, |app, window, cx| {
                let launch = app.terminal_launching.get_mut(&iid).unwrap();
                launch.since = Instant::now() - Duration::from_secs(65);
                let token = launch.token;
                app.tick(window, cx);
                token
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            probes.count(),
            2,
            "a hung probe must not hold recovery forever"
        );
        window
            .read_with(cx, |app, _| {
                assert!(!launch_token_current(&app.terminal_launching, iid, old));
            })
            .unwrap();
        probes.fail(0, "stale owner reply");
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(!app.failed_launches.contains_key(&iid))
            })
            .unwrap();
        probes.fail(1, "replacement owner reply landed");
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(app.failed_launches[&iid].contains("replacement owner reply landed"));
            })
            .unwrap();
    });
}
