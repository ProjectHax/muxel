//! Finding agents that are running — or have run — outside muxel, so a project can
//! take them over.
//!
//! Three kinds of thing turn up, and they are not equally good to import:
//!
//! 1. **A live tmux session.** The best case by far. muxel attaches a pane to it,
//!    the agent keeps running untouched, and the pane survives a muxel restart.
//!    This works for *any* program, because muxel never has to know what is inside
//!    — which is the only way opencode or Amp, neither of which can resume a
//!    conversation, can be imported at all.
//! 2. **An agent running outside tmux.** Its terminal belongs to whatever started
//!    it and cannot be taken over: a running process's PTY can't be moved into a
//!    tmux session. The most muxel can do is resume its *conversation* in a new
//!    pane, which leaves the original still running on the same conversation. That
//!    is worth offering, but only with the warning attached.
//! 3. **A conversation on disk with nothing running.** Clean: muxel starts the
//!    agent with its resume flag, in a tmux session when the project uses them.
//!
//! Everything here is pure and unit-tested. The `muxel` crate runs the commands
//! ([`process_probe_command`], `tmux::list_sessions_args`) and reads the
//! conversation directories; this module decides what the results *mean*.

use crate::PresetKind;
use crate::agent::AgentPreset;
use crate::tmux::RemoteSession;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Where an importable agent was found, and what importing it will actually do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportSource {
    /// A live tmux session: attach to it. The agent is untouched.
    TmuxSession {
        /// The session's own name, which the pane attaches to verbatim.
        session: String,
    },
    /// A process running outside tmux. Its conversation can be resumed, but the
    /// process itself keeps running — `session_id` is `None` when muxel couldn't
    /// work out which conversation is its, in which case it can only be reported,
    /// not imported.
    Running {
        pid: u32,
        session_id: Option<String>,
    },
    /// A conversation on disk with nothing running.
    Conversation {
        session_id: String,
        /// Unix seconds the conversation was last written, for ordering.
        modified: i64,
    },
}

impl ImportSource {
    /// Whether importing this actually attaches to the running agent, rather than
    /// starting a second one against the same conversation.
    pub fn is_attach(&self) -> bool {
        matches!(self, Self::TmuxSession { .. })
    }

    /// Whether this can be imported at all. A process muxel can't tie to a
    /// conversation is listed to explain itself, but there is nothing to import.
    pub fn is_importable(&self) -> bool {
        match self {
            Self::TmuxSession { .. } | Self::Conversation { .. } => true,
            Self::Running { session_id, .. } => session_id.is_some(),
        }
    }
}

/// One row in the Import window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportCandidate {
    pub source: ImportSource,
    /// The program muxel will run (or that is already running): `claude`, `zsh`, …
    pub program: Option<String>,
    /// The preset this matched, when one did.
    pub preset_id: Option<Uuid>,
    /// Display name for the preset, or the raw command when nothing matched.
    pub preset_name: String,
    /// Working directory the agent is (or was) running in.
    pub cwd: String,
    /// Whether `cwd` is the project root or beneath it.
    pub in_tree: bool,
}

/// Whether `path` is `root` or sits beneath it (a worktree, say). Mirrors the
/// containment rule `tmux::orphan_sessions` uses, so a session is attributed to a
/// project the same way wherever the question is asked.
pub fn in_tree(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    path == root || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// The terminal preset whose program is `command`, matched on the basename so a
/// `pane_current_command` of `claude` matches a preset that runs
/// `/usr/local/bin/claude`. Shell presets match too — a plain shell session is a
/// perfectly good thing to import.
pub fn preset_for_command<'a>(
    presets: &'a [AgentPreset],
    command: &str,
) -> Option<&'a AgentPreset> {
    let want = basename(command);
    presets.iter().find(|p| {
        p.kind == PresetKind::Terminal
            && p.program
                .as_deref()
                .is_some_and(|program| basename(program) == want)
    })
}

/// The last path component, so `/usr/local/bin/claude` and `claude` compare equal.
fn basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

/// Every tmux session that isn't already a muxel pane, as import candidates.
///
/// Unlike `tmux::orphan_sessions` — which adopts only muxel's *own* abandoned
/// sessions — this deliberately includes sessions muxel never created. That is the
/// whole point: the user started something in their own tmux and wants it in here.
/// Sessions a pane already owns are excluded, because importing one would put two
/// panes on one session.
pub fn tmux_candidates(
    sessions: &[RemoteSession],
    owned: &[String],
    root: &str,
    presets: &[AgentPreset],
) -> Vec<ImportCandidate> {
    sessions
        .iter()
        .filter(|s| !owned.iter().any(|o| o == &s.name))
        .map(|s| {
            let preset = preset_for_command(presets, &s.command);
            ImportCandidate {
                source: ImportSource::TmuxSession {
                    session: s.name.clone(),
                },
                program: Some(s.command.clone()),
                preset_id: preset.map(|p| p.id),
                preset_name: preset.map_or_else(|| s.command.clone(), |p| p.name.clone()),
                cwd: s.path.clone(),
                in_tree: in_tree(&s.path, root),
            }
        })
        .collect()
}

/// One process from [`process_probe_command`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    /// Working directory, empty when it couldn't be read (a process owned by
    /// another user, or a platform without `/proc`).
    pub cwd: String,
    /// The executable's name, as the OS reports it.
    pub comm: String,
}

/// A shell snippet listing every process of this user as `pid|ppid|cwd|comm`.
///
/// `ps` alone can't answer the question — it has no cwd column — so each pid's cwd
/// comes from `/proc/<pid>/cwd` where that exists and `lsof` where it doesn't
/// (macOS). A pid whose cwd can't be read still gets a line with an empty cwd
/// rather than being dropped, so it can be shown and explained. Written as one
/// snippet because it has to run over `ssh` for a remote project just as it does
/// locally.
pub fn process_probe_command() -> String {
    // `ps -eo` is POSIX; the `=` suffixes suppress headers on both Linux and BSD.
    // The subshell keeps a missing `lsof` from failing the whole listing.
    r#"ps -eo pid=,ppid=,comm= | while read -r p pp c; do
  d=""
  if [ -r "/proc/$p/cwd" ]; then d=$(readlink "/proc/$p/cwd" 2>/dev/null || true); fi
  if [ -z "$d" ] && command -v lsof >/dev/null 2>&1; then
    d=$(lsof -a -p "$p" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1 || true)
  fi
  printf '%s|%s|%s|%s\n' "$p" "$pp" "$d" "$c"
done"#
        .to_string()
}

/// Parse [`process_probe_command`] output. Malformed lines are skipped rather than
/// failing the lot — this runs against whatever shell the far side happens to have.
pub fn parse_processes(out: &str) -> Vec<ProcessRow> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '|');
            let (Some(pid), Some(ppid), Some(cwd), Some(comm)) =
                (f.next(), f.next(), f.next(), f.next())
            else {
                return None;
            };
            let comm = comm.trim();
            if comm.is_empty() {
                return None;
            }
            Some(ProcessRow {
                pid: pid.trim().parse().ok()?,
                ppid: ppid.trim().parse().ok()?,
                cwd: cwd.trim().to_string(),
                comm: comm.to_string(),
            })
        })
        .collect()
}

/// Whether `pid` has an ancestor in `exclude`, walking up `ppid` links.
///
/// This is what keeps the list honest. A process inside tmux is already offered as
/// its tmux session — the good kind of import — so listing it again as a bare
/// process would offer the *worse* way to take over the very same agent. muxel's
/// own panes are excluded the same way: they are already in muxel.
///
/// The walk is bounded by the number of rows, so a corrupt ppid cycle (pid 1
/// reparenting during the probe) terminates instead of hanging.
pub fn has_ancestor(rows: &[ProcessRow], pid: u32, exclude: &[u32]) -> bool {
    let parent_of = |p: u32| rows.iter().find(|r| r.pid == p).map(|r| r.ppid);
    let mut current = pid;
    for _ in 0..rows.len() {
        if exclude.contains(&current) {
            return true;
        }
        match parent_of(current) {
            // pid 0/1 is the top: nothing above it to find.
            Some(parent) if parent != 0 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// Agent processes running outside tmux and outside muxel, as import candidates.
///
/// `tmux_pids` and `muxel_pids` are the roots whose descendants are skipped — see
/// [`has_ancestor`]. `session_for` is asked to name the conversation each process
/// is on; returning `None` still yields a row, so the user can see the agent was
/// found and why it can't be taken over.
pub fn process_candidates(
    rows: &[ProcessRow],
    tmux_pids: &[u32],
    muxel_pids: &[u32],
    root: &str,
    presets: &[AgentPreset],
    mut session_for: impl FnMut(&ProcessRow, &AgentPreset) -> Option<String>,
) -> Vec<ImportCandidate> {
    let mut excluded: Vec<u32> = Vec::with_capacity(tmux_pids.len() + muxel_pids.len());
    excluded.extend_from_slice(tmux_pids);
    excluded.extend_from_slice(muxel_pids);
    rows.iter()
        .filter(|r| !r.cwd.is_empty())
        .filter_map(|r| {
            // Only things muxel knows how to be: a preset's program. A bare `zsh`
            // outside tmux is not worth importing — there is no conversation to
            // resume and no session to attach to.
            let preset = preset_for_command(presets, &r.comm)?;
            // opencode, Amp, a shell: nothing to resume, and we can't attach to a
            // process's own terminal, so there is no way to take it over at all.
            preset.resume_flag.as_ref()?;
            if has_ancestor(rows, r.pid, &excluded) {
                return None;
            }
            Some(ImportCandidate {
                source: ImportSource::Running {
                    pid: r.pid,
                    session_id: session_for(r, preset),
                },
                program: preset.program.clone(),
                preset_id: Some(preset.id),
                preset_name: preset.name.clone(),
                cwd: r.cwd.clone(),
                in_tree: in_tree(&r.cwd, root),
            })
        })
        .collect()
}

/// Claude's project directory for an agent running in `cwd`:
/// `<home>/.claude/projects/<slug>`, where `slug` is `cwd` with every
/// non-ASCII-alphanumeric character replaced by `-`. The per-conversation
/// transcripts are the `*.jsonl` files inside it.
///
/// This is the directory half of [`crate::claude_session_path`]; the two must agree
/// or an imported conversation would resume against a path Claude never wrote.
pub fn claude_project_dir(home: &Path, cwd: &Path) -> PathBuf {
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(".claude").join("projects").join(slug)
}

/// Turn `(session_id, modified)` pairs — read out of a conversation directory — into
/// candidates, dropping any conversation a pane is already on.
///
/// `live` is the set of session ids muxel panes already hold. Importing one of those
/// would put a second agent on a conversation that is already open in muxel, which
/// is the one outcome this window must never produce.
pub fn conversation_candidates(
    conversations: &[(String, i64)],
    live: &[String],
    cwd: &str,
    root: &str,
    preset: &AgentPreset,
) -> Vec<ImportCandidate> {
    conversations
        .iter()
        .filter(|(id, _)| !live.iter().any(|l| l == id))
        .map(|(id, modified)| ImportCandidate {
            source: ImportSource::Conversation {
                session_id: id.clone(),
                modified: *modified,
            },
            program: preset.program.clone(),
            preset_id: Some(preset.id),
            preset_name: preset.name.clone(),
            cwd: cwd.to_string(),
            in_tree: in_tree(cwd, root),
        })
        .collect()
}

/// Order the window's rows: the project's own first, then the ones muxel can truly
/// attach to, then the most recently used, then by name so the list never shuffles
/// between two refreshes that found the same things.
pub fn sort_candidates(candidates: &mut [ImportCandidate]) {
    candidates.sort_by(|a, b| {
        b.in_tree
            .cmp(&a.in_tree)
            .then_with(|| b.source.is_attach().cmp(&a.source.is_attach()))
            .then_with(|| b.source.is_importable().cmp(&a.source.is_importable()))
            .then_with(|| recency(&b.source).cmp(&recency(&a.source)))
            .then_with(|| a.preset_name.cmp(&b.preset_name))
            .then_with(|| a.cwd.cmp(&b.cwd))
            .then_with(|| key(&a.source).cmp(&key(&b.source)))
    });
}

/// How recently a candidate was used, where that is known (conversations only).
fn recency(source: &ImportSource) -> i64 {
    match source {
        ImportSource::Conversation { modified, .. } => *modified,
        _ => 0,
    }
}

/// A stable tiebreak, so equal rows keep a fixed order across refreshes.
fn key(source: &ImportSource) -> String {
    match source {
        ImportSource::TmuxSession { session } => session.clone(),
        ImportSource::Running { pid, .. } => pid.to_string(),
        ImportSource::Conversation { session_id, .. } => session_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ImportCandidate, ImportSource, ProcessRow, claude_project_dir, conversation_candidates,
        has_ancestor, in_tree, parse_processes, preset_for_command, process_candidates,
        process_probe_command, sort_candidates,
    };
    use crate::agent::AgentPreset;
    use crate::tmux::RemoteSession;
    use std::path::Path;

    fn presets() -> Vec<AgentPreset> {
        vec![
            AgentPreset::claude(),
            AgentPreset::opencode(),
            AgentPreset::shell(),
        ]
    }

    fn session(name: &str, path: &str, command: &str) -> RemoteSession {
        RemoteSession {
            name: name.to_string(),
            path: path.to_string(),
            command: command.to_string(),
        }
    }

    fn row(pid: u32, ppid: u32, cwd: &str, comm: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            cwd: cwd.to_string(),
            comm: comm.to_string(),
        }
    }

    #[test]
    fn a_program_matches_its_preset_by_basename() {
        let presets = presets();
        let found = preset_for_command(&presets, "/usr/local/bin/claude").expect("claude");
        assert_eq!(found.program.as_deref(), Some("claude"));
        // An unknown program matches nothing rather than falling back to a shell.
        assert!(preset_for_command(&presets, "vim").is_none());
    }

    #[test]
    fn a_users_own_tmux_session_is_offered_but_a_pane_s_own_is_not() {
        let sessions = vec![
            session("work", "/home/u/proj", "claude"),
            session("muxel_proj_1a2b3c4d", "/home/u/proj", "claude"),
        ];
        // The muxel-owned one is already a pane; the user's own is the point.
        let got = tmux_only(
            &sessions,
            &["muxel_proj_1a2b3c4d".to_string()],
            "/home/u/proj",
        );
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].source,
            ImportSource::TmuxSession {
                session: "work".to_string()
            }
        );
        assert!(got[0].in_tree);
        assert!(got[0].source.is_attach(), "a tmux session is a real attach");
    }

    #[test]
    fn a_session_outside_the_project_is_listed_but_marked_out_of_tree() {
        let sessions = vec![session("elsewhere", "/home/u/other", "claude")];
        let got = tmux_only(&sessions, &[], "/home/u/proj");
        assert_eq!(got.len(), 1, "everything is offered, wherever it lives");
        assert!(!got[0].in_tree);
    }

    fn tmux_only(sessions: &[RemoteSession], owned: &[String], root: &str) -> Vec<ImportCandidate> {
        super::tmux_candidates(sessions, owned, root, &presets())
    }

    #[test]
    fn containment_matches_the_rule_orphan_sessions_uses() {
        assert!(in_tree("/home/u/proj", "/home/u/proj"));
        assert!(in_tree("/home/u/proj/wt", "/home/u/proj"));
        assert!(in_tree("/home/u/proj/", "/home/u/proj"));
        // A sibling that merely shares a prefix is not inside it.
        assert!(!in_tree("/home/u/project2", "/home/u/proj"));
        assert!(!in_tree("/home/u", "/home/u/proj"));
    }

    #[test]
    fn the_process_probe_reads_a_cwd_ps_cannot_give_it() {
        let cmd = process_probe_command();
        assert!(cmd.contains("ps -eo"), "still a process listing");
        assert!(cmd.contains("/proc/$p/cwd"), "Linux cwd");
        assert!(cmd.contains("lsof"), "macOS fallback");
    }

    #[test]
    fn process_rows_parse_and_bad_lines_are_skipped() {
        let out = "\
 101|1|/home/u/proj|claude
 102|101|/home/u/proj|node
nonsense
 103|1||claude
 |1|/x|claude
";
        let rows = parse_processes(out);
        assert_eq!(rows.len(), 3, "three well-formed lines");
        assert_eq!(rows[0], row(101, 1, "/home/u/proj", "claude"));
        assert_eq!(rows[2].cwd, "", "an unreadable cwd still yields a row");
    }

    #[test]
    fn a_process_under_tmux_or_muxel_is_found_by_walking_parents() {
        let rows = vec![
            row(10, 1, "/", "tmux: server"),
            row(20, 10, "/home/u/proj", "zsh"),
            row(30, 20, "/home/u/proj", "claude"),
            row(40, 1, "/home/u/proj", "claude"),
        ];
        assert!(has_ancestor(&rows, 30, &[10]), "grandchild of tmux");
        assert!(!has_ancestor(&rows, 40, &[10]), "started from a shell");
    }

    #[test]
    fn a_parent_cycle_terminates_instead_of_hanging() {
        // pid 1 reparenting mid-probe can produce a loop; it must not spin.
        let rows = vec![row(5, 6, "/x", "claude"), row(6, 5, "/x", "claude")];
        assert!(!has_ancestor(&rows, 5, &[99]));
    }

    #[test]
    fn an_agent_inside_tmux_is_not_also_offered_as_a_bare_process() {
        // It is already offered as its tmux session, which is the better import.
        let rows = vec![
            row(10, 1, "/", "tmux: server"),
            row(30, 10, "/home/u/proj", "claude"),
            row(40, 1, "/home/u/proj", "claude"),
        ];
        let got = process_candidates(&rows, &[10], &[], "/home/u/proj", &presets(), |r, _| {
            Some(format!("conv-{}", r.pid))
        });
        assert_eq!(got.len(), 1, "only the one outside tmux");
        assert_eq!(
            got[0].source,
            ImportSource::Running {
                pid: 40,
                session_id: Some("conv-40".to_string()),
            }
        );
        assert!(
            !got[0].source.is_attach(),
            "resuming a conversation is not attaching"
        );
    }

    #[test]
    fn an_agent_that_cannot_resume_is_not_offered_as_a_bare_process() {
        // opencode has no resume flag: there is no way to take it over at all.
        let rows = vec![row(40, 1, "/home/u/proj", "opencode")];
        let got = process_candidates(&rows, &[], &[], "/home/u/proj", &presets(), |_, _| None);
        assert!(got.is_empty());
    }

    #[test]
    fn a_process_whose_conversation_is_unknown_is_listed_but_not_importable() {
        let rows = vec![row(40, 1, "/home/u/proj", "claude")];
        let got = process_candidates(&rows, &[], &[], "/home/u/proj", &presets(), |_, _| None);
        assert_eq!(got.len(), 1, "shown, so the user knows it was found");
        assert!(!got[0].source.is_importable(), "but nothing to import");
    }

    #[test]
    fn muxels_own_panes_are_never_offered_back_to_it() {
        let rows = vec![
            row(7, 1, "/", "muxel"),
            row(30, 7, "/home/u/proj", "claude"),
        ];
        let got = process_candidates(&rows, &[], &[7], "/home/u/proj", &presets(), |_, _| {
            Some("c".to_string())
        });
        assert!(got.is_empty());
    }

    #[test]
    fn the_claude_project_dir_is_the_transcript_paths_parent() {
        let dir = claude_project_dir(Path::new("/home/u"), Path::new("/home/u/Proj"));
        let file =
            crate::claude_session_path(Path::new("/home/u"), Path::new("/home/u/Proj"), "id");
        assert_eq!(
            file.parent(),
            Some(dir.as_path()),
            "must agree, or a resume aims at a path Claude never wrote"
        );
        assert!(dir.ends_with("-home-u-Proj"));
    }

    #[test]
    fn a_conversation_a_pane_is_already_on_is_not_offered() {
        let convs = vec![("open".to_string(), 20), ("closed".to_string(), 10)];
        let got = conversation_candidates(
            &convs,
            &["open".to_string()],
            "/home/u/proj",
            "/home/u/proj",
            &AgentPreset::claude(),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].source,
            ImportSource::Conversation {
                session_id: "closed".to_string(),
                modified: 10,
            }
        );
    }

    #[test]
    fn rows_are_ordered_by_project_then_attachability_then_recency() {
        let claude = AgentPreset::claude();
        let mk = |source: ImportSource, in_tree: bool, cwd: &str| ImportCandidate {
            source,
            program: claude.program.clone(),
            preset_id: Some(claude.id),
            preset_name: claude.name.clone(),
            cwd: cwd.to_string(),
            in_tree,
        };
        let mut rows = vec![
            mk(
                ImportSource::Conversation {
                    session_id: "old".into(),
                    modified: 1,
                },
                true,
                "/p",
            ),
            mk(
                ImportSource::TmuxSession {
                    session: "outside".into(),
                },
                false,
                "/other",
            ),
            mk(
                ImportSource::Conversation {
                    session_id: "new".into(),
                    modified: 9,
                },
                true,
                "/p",
            ),
            mk(
                ImportSource::TmuxSession {
                    session: "mine".into(),
                },
                true,
                "/p",
            ),
        ];
        sort_candidates(&mut rows);
        let order: Vec<String> = rows.iter().map(|r| super::key(&r.source)).collect();
        assert_eq!(
            order,
            vec!["mine", "new", "old", "outside"],
            "in-tree first, attachable before resumable, newest conversation first"
        );
    }
}
