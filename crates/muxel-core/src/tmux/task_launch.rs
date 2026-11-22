use crate::control::TaskLaunchHash;
use crate::tmux::SessionTarget;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const OPTION: &str = "@muxel_task_launch";

/// Short-lived public permits for commands that may create or attach. The
/// range retains unobserved launches across retries without a growing list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLaunch {
    target: SessionTarget,
    namespace: Uuid,
    issued: u64,
    pin: Option<Pin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Pin {
    sequence: u64,
    session: Session,
    pane: u64,
    pid: u32,
    hash: TaskLaunchHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    server_pid: u32,
    server_started: u64,
    id: u64,
}

struct Pane<'a> {
    session: Session,
    id: u64,
    pid: Option<u32>,
    original: bool,
    record: &'a str,
}

impl TaskLaunch {
    pub fn new(target: SessionTarget) -> Self {
        Self {
            target,
            namespace: Uuid::new_v4(),
            issued: 0,
            pin: None,
        }
    }

    pub fn issue(&mut self, target: &SessionTarget) -> Option<String> {
        if &self.target != target {
            *self = Self::new(target.clone());
        }
        self.issued = self.issued.checked_add(1)?;
        Some(format!("{}.{}", self.namespace, self.issued))
    }

    pub fn has_pin(&self) -> bool {
        self.pin.is_some()
    }

    /// Pin the creation receipt once. An attach preserves the exact running
    /// pane and its pin even if the option later changes. A respawn or split
    /// in that session never replaces the original pane's pin.
    pub fn observe(&mut self, target: &SessionTarget, output: &str) -> Option<TaskLaunchHash> {
        if target != &self.target || target.socket.is_none() {
            return None;
        }
        let panes: Vec<_> = output
            .lines()
            .map(|line| parse_pane(&target.name, line))
            .collect::<Option<_>>()?;
        let session = panes.first()?.session;
        if panes.iter().any(|pane| pane.session != session) {
            return None;
        }
        if let Some(pin) = &self.pin
            && pin.session == session
        {
            let matches = panes
                .iter()
                .filter(|pane| pane.id == pin.pane && pane.pid == Some(pin.pid))
                .count();
            return (matches == 1).then_some(pin.hash);
        }
        let mut originals = panes.iter().filter(|pane| pane.original);
        let pane = originals.next()?;
        if originals.next().is_some() {
            return None;
        }
        let pid = pane.pid?;
        let [id, hash, receipt_pane, receipt_pid] =
            pane.record.split(':').collect::<Vec<_>>().try_into().ok()?;
        let (namespace, sequence) = parse_launch_id(id)?;
        if namespace != self.namespace
            || sequence > self.issued
            || self
                .pin
                .as_ref()
                .is_some_and(|pin| sequence <= pin.sequence)
            || tagged_number(receipt_pane, '%')? != pane.id
            || receipt_pid.parse::<u32>().ok()? != pid
        {
            return None;
        }
        let mut digest = [0; 32];
        hex::decode_to_slice(hash, &mut digest).ok()?;
        self.pin = Some(Pin {
            sequence,
            session,
            pane: pane.id,
            pid,
            hash: digest,
        });
        Some(digest)
    }
}

fn parse_launch_id(id: &str) -> Option<(Uuid, u64)> {
    let (namespace, sequence) = id.split_once('.')?;
    let namespace: Uuid = namespace.parse().ok()?;
    let sequence: u64 = sequence.parse().ok()?;
    (id == format!("{namespace}.{sequence}") && sequence > 0).then_some((namespace, sequence))
}

fn tagged_number(text: &str, tag: char) -> Option<u64> {
    let digits = text.strip_prefix(tag)?;
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

fn parse_pane<'a>(expected: &str, line: &'a str) -> Option<Pane<'a>> {
    let [
        name,
        server_pid,
        server_started,
        session,
        pane,
        pid,
        dead,
        original,
        record,
    ] = line.split('\t').collect::<Vec<_>>().try_into().ok()?;
    if name != expected {
        return None;
    }
    let pid = match dead {
        "0" => Some(pid.parse::<u32>().ok().filter(|&pid| pid != 0)?),
        "1" => None,
        _ => return None,
    };
    Some(Pane {
        session: Session {
            server_pid: server_pid.parse().ok()?,
            server_started: server_started.parse().ok()?,
            id: tagged_number(session, '$')?,
        },
        id: tagged_number(pane, '%')?,
        pid,
        original: pane == original,
        record,
    })
}

pub fn observation_args(session: &str) -> Vec<String> {
    vec![
        "list-panes".into(),
        "-s".into(),
        "-t".into(),
        format!("={session}:"),
        "-F".into(),
        format!(
            "#{{session_name}}\t#{{pid}}\t#{{start_time}}\t#{{session_id}}\t#{{pane_id}}\t#{{pane_pid}}\t#{{pane_dead}}\t#{{@muxel_control_pane}}\t#{{{OPTION}}}"
        ),
    ]
}

/// `-p` binds the receipt to the pane and `-o` refuses to overwrite one on
/// a manual respawn. Expand pane and process ids in the same server command.
pub fn publish_args(pane: &str, id: &str, hash: &TaskLaunchHash) -> Option<Vec<String>> {
    tagged_number(pane, '%')?;
    parse_launch_id(id)?;
    Some(vec![
        "set-option".into(),
        "-p".into(),
        "-o".into(),
        "-F".into(),
        "-t".into(),
        pane.into(),
        OPTION.into(),
        format!("{id}:{}:#{{pane_id}}:#{{pane_pid}}", hex::encode(hash)),
    ])
}

#[cfg(test)]
mod tests {
    use super::TaskLaunch;
    use crate::tmux::{SessionTarget, Socket};

    const HASH: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const OTHER: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    fn target() -> SessionTarget {
        SessionTarget::new("task", Some(Socket::Private))
    }

    fn pane(session: u64, pane: u64, pid: u32, record: &str) -> String {
        format!("task\t321\t1000\t${session}\t%{pane}\t{pid}\t0\t%{pane}\t{record}\n")
    }

    #[test]
    fn tmux_task_launch_pins_across_restart_and_rotates_only_for_a_new_session() {
        let target = target();
        let mut launch = TaskLaunch::new(target.clone());
        let first = launch.issue(&target).unwrap();
        let first_record = pane(1, 9, 400, &format!("{first}:{HASH}:%9:400"));
        let attached = launch.issue(&target).unwrap();
        assert_ne!(first, attached);
        let mut launch: TaskLaunch =
            serde_json::from_str(&serde_json::to_string(&launch).unwrap()).unwrap();
        assert_eq!(launch.observe(&target, &first_record), Some([1; 32]));

        let mut launch: TaskLaunch =
            serde_json::from_str(&serde_json::to_string(&launch).unwrap()).unwrap();
        let changed = pane(1, 9, 400, &format!("{attached}:{OTHER}:%9:400"));
        assert_eq!(launch.observe(&target, &changed), Some([1; 32]));
        assert_eq!(launch.observe(&target, &pane(1, 9, 401, "")), None);
        assert_eq!(
            launch.observe(
                &target,
                &pane(1, 11, 401, &format!("{attached}:{OTHER}:%11:401"))
            ),
            None
        );

        let replacement = pane(2, 10, 500, &format!("{attached}:{OTHER}:%10:500"));
        assert_eq!(launch.observe(&target, &replacement), Some([2; 32]));
        assert_eq!(launch.observe(&target, &first_record), None);
    }

    #[test]
    fn tmux_task_launch_enrols_only_one_expected_live_pane() {
        let target = target();
        let mut launch = TaskLaunch::new(target.clone());
        let id = launch.issue(&target).unwrap();
        let correct = pane(2, 10, 500, &format!("{id}:{HASH}:%10:500"));
        let duplicate = pane(2, 11, 501, &format!("{id}:{OTHER}:%11:501"));
        assert_eq!(
            launch.observe(&target, &format!("{correct}{duplicate}")),
            None
        );
        let copied = pane(2, 11, 501, &format!("{id}:{HASH}:%10:500"));
        assert_eq!(launch.observe(&target, &copied), None);
        let other_server = SessionTarget::new("task", Some(Socket::Default));
        assert_eq!(launch.observe(&other_server, &correct), None);
        let dead = correct.replace("\t0\t", "\t1\t");
        assert_eq!(launch.observe(&target, &dead), None);
        assert_eq!(launch.observe(&target, &correct), Some([1; 32]));
    }
}
