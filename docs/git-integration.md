# Git reads and writes

Background polling and display commands run with optional git locking disabled.
They also set `diff.autoRefreshIndex=false`: porcelain `git diff` can otherwise
run `update-index --refresh` even with optional locks disabled. See the
[`git diff` configuration documentation](https://git.github.io/htmldocs/git-diff.html).
This includes status/change counts, diffs, branch labels, revision counts,
remote file listings, and library checks. Git may otherwise refresh the index during
`status`, briefly taking `index.lock` in a repository an agent is also using.

Local reads use the shared `git_read_command` / `read_only_git` policy. Reads at a
`RepoLoc` use `git_output`, which applies the same policy locally and passes
`GIT_OPTIONAL_LOCKS=0` and `-c diff.autoRefreshIndex=false` on an SSH host. The
environment form keeps commands compatible with Git versions predating the
optional-locks command-line flag. Those versions ignore the environment setting;
optional-lock suppression requires host Git support. Remote file listings use the
same policy. New display commands should use these read helpers.

The iOS companion uses GitCommands.readOnly for its remote diff viewer and
repository probes, with the same optional-lock and diff-refresh settings.

Writes use `git_write_output` or an ordinary git command. Staging, committing,
merging, checking out branches, fetching/pulling/pushing, and creating or removing
worktrees retain git's normal locks. Disabling optional locks never bypasses the
required locks that protect these operations.
