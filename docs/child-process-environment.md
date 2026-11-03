# Child process environment

Muxel's profiler settings belong to the current application process. Passing them
to a pane can turn on profiling in a test window or cause a nested muxel to write
to the parent's logs.

All helper commands use `muxel_core::child_env::command`. PTY commands apply the
same removal policy after configured environment overrides. Every key beginning
with `MUXEL_PROFILE` is removed, including future knobs and configured overrides.
Examples include the profiler switches, log paths, `MUXEL_PROFILE_KEEP_DAYS`, and
`MUXEL_PROFILE_MAX_BYTES`.

Tmux launches also unset these names from the server's global environment before
creating a session, using known profiler names plus matching keys in muxel's
environment or the launch overrides. The iOS companion applies the same cleanup
to its remote tmux launches. This covers servers started by an earlier muxel or
another program. Existing attached programs retain their original environment.

Pane context such as `MUXEL_INSTANCE_ID`, `MUXEL_USER_DATA_DIR`, and `MUXEL_CONTROL`
keeps its existing launch policy. Profiler removal does not clear `MUXEL_*` as a
group. To profile a nested muxel, set its profiler variables inside the pane when
launching it.
