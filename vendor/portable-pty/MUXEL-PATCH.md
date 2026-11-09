# Muxel's portable-pty 0.9.0 patch

This is the crates.io portable-pty 0.9.0 source package, with its original MIT
license. The Windows change in `src/win/psuedocon.rs` removes
`PSEUDOCONSOLE_INHERIT_CURSOR` from creation flags and its unused constant.
The resize quirk and Win32 input mode flags remain intact. On Unix,
`CommandBuilder::as_command` (`src/cmdbuilder.rs`) is `pub` instead of
`pub(crate)`, so Muxel can resolve the executable a launch will run, with the
PTY's own PATH and cwd resolution, before it spawns. Nothing else changes on
Unix.

Muxel creates a fresh terminal with its cursor at the origin. Inheriting a
parent cursor adds a startup query whose missing reply can deadlock console
shutdown when an agent exits before the UI parses that query. Ordinary program
cursor queries still go through Muxel's terminal emulator.

Microsoft documents this shutdown dependency in
[Creating a Pseudoconsole session](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session#ending-the-pseudoconsole-session).

## Job objects

`CommandBuilder::set_job_object` (Windows) names a job object for the child.
`src/win/psuedocon.rs` adds it to the process's attribute list with
`PROC_THREAD_ATTRIBUTE_JOB_LIST` (`src/win/procthreadattr.rs`), so the child is
in the job from its first instruction and nothing it starts at once escapes.
Assigning the job after `CreateProcessW` returns would leave that window open.
If Windows refuses the job, the spawn logs it and retries once without it, so a
job never keeps a program from starting; callers check `IsProcessInJob`. Muxel
uses this for its per-pane memory limit.

The workspace patch is local and pinned to the checked-in package. Remove it
when portable-pty exposes creation flags, a job-list attribute and its Unix
command resolution, so Muxel can select no cursor inheritance and a
creation-time job, and know the launched executable, without changing the
dependency. Keep source changes bounded to these three hooks and their tests.
