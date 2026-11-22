use muxel_core::control::{TASK_LAUNCH_TOKEN_ENV, TaskLaunchHash, TaskLaunchToken};
use std::ffi::OsString;
use std::process::{Command, ExitStatus};

pub(crate) fn new_proof() -> anyhow::Result<(TaskLaunchToken, TaskLaunchHash)> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("could not generate a task launch proof"))?;
    let proof = TaskLaunchToken(hex::encode(bytes));
    let hash = crate::control::task_launch_hash(&proof);
    Ok((proof, hash))
}

/// The launch wrapper invokes this only on session creation. Publish the
/// verifier from inside that tmux pane before its program receives the proof.
pub(crate) fn run_in_tmux(mut args: impl Iterator<Item = OsString>) -> i32 {
    let Some(id) = args.next().and_then(|id| id.into_string().ok()) else {
        return 2;
    };
    let Some(program) = args.next() else {
        return 2;
    };
    // The interactive child belongs to the pane's terminal, including on
    // Windows. Only the short tmux publication command runs without a console.
    let mut child = muxel_core::child_env::command(program);
    muxel_terminal::sanitize_appimage_command_env(&mut child);
    child.args(args).env_remove(TASK_LAUNCH_TOKEN_ENV);
    if let Some(proof) = publish(&id) {
        child.env(TASK_LAUNCH_TOKEN_ENV, proof.0);
    } else {
        eprintln!("muxel: this tmux pane could not register for task-only control");
    }
    match exec(child) {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("muxel: could not start the task pane's program: {error}");
            1
        }
    }
}

fn publish(id: &str) -> Option<TaskLaunchToken> {
    // TMUX contains the socket path followed by the server pid and session id.
    let tmux = std::env::var("TMUX").ok()?;
    let socket = tmux
        .rsplitn(3, ',')
        .nth(2)
        .filter(|path| !path.is_empty())?;
    let pane = std::env::var("TMUX_PANE").ok()?;
    register(&pane, id, |args| {
        crate::integrations::publish_tmux_task_launch(socket, args)
    })
}

fn register(
    pane: &str,
    id: &str,
    publish: impl FnOnce(&[String]) -> bool,
) -> Option<TaskLaunchToken> {
    let (proof, hash) = new_proof().ok()?;
    let args = muxel_core::tmux::task_launch::publish_args(pane, id, &hash)?;
    publish(&args).then_some(proof)
}

#[cfg(unix)]
fn exec(mut child: Command) -> std::io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt as _;
    Err(child.exec())
}

#[cfg(not(unix))]
fn exec(mut child: Command) -> std::io::Result<ExitStatus> {
    child.status()
}

#[cfg(test)]
mod tests {
    #[test]
    fn task_launch_registers_only_the_digest_before_releasing_the_proof() {
        let id = format!("{}.1", uuid::Uuid::from_u128(1));
        let mut publication = Vec::new();
        let proof = super::register("%12", &id, |args| {
            publication = args.to_vec();
            true
        })
        .expect("a successful publication releases the proof");
        let digest = crate::control::task_launch_hash(&proof);
        assert!(publication.iter().all(|arg| !arg.contains(&proof.0)));
        assert_eq!(
            publication.last().unwrap(),
            &format!("{id}:{}:#{{pane_id}}:#{{pane_pid}}", hex::encode(digest))
        );
        let fresh = super::register("%13", &id, |_| true).unwrap();
        assert!(fresh != proof, "a new pane reused a launch proof");
        assert!(super::register("%12", &id, |_| false).is_none());
    }
}
