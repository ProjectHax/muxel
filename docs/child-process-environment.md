# Child process environment

Muxel's profiler settings belong to the current application process. Passing them
to a pane can turn on profiling in a test window or cause a nested muxel to write
to the parent's logs.

All helper commands use `muxel_core::child_env::command`. PTY commands apply the
same removal policy after configured environment overrides. The shared list is
`child_env::PROFILER_VARS`: `MUXEL_PROFILE`, `MUXEL_PROFILE_TERMINAL`,
`MUXEL_PROFILE_UI`, `MUXEL_PROFILE_LOG`, `MUXEL_PROFILE_UI_LOG`,
`MUXEL_PROFILE_STDERR`, and `MUXEL_PROFILE_CHAINS`.

Tmux launches also unset these names from the server's global environment before
creating a session. This covers servers started by an earlier muxel or another
program. Existing attached programs retain their original environment.

Pane context such as `MUXEL_INSTANCE_ID`, `MUXEL_USER_DATA_DIR`, and `MUXEL_CONTROL`
keeps its existing launch policy. Profiler removal does not clear `MUXEL_*` as a
group. To profile a nested muxel, set its profiler variables inside the pane when
launching it.
