# Muxel's portable-pty 0.9.0 patch

This is the crates.io portable-pty 0.9.0 source package, with its original MIT
license. The Windows change in `src/win/psuedocon.rs` removes
`PSEUDOCONSOLE_INHERIT_CURSOR` from creation flags and its unused constant.
The resize quirk and Win32 input mode flags remain intact. Unix is unchanged.

Muxel creates a fresh terminal with its cursor at the origin. Inheriting a
parent cursor adds a startup query whose missing reply can deadlock console
shutdown when an agent exits before the UI parses that query. Ordinary program
cursor queries still go through Muxel's terminal emulator.

Microsoft documents this shutdown dependency in
[Creating a Pseudoconsole session](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session#ending-the-pseudoconsole-session).

The workspace patch is local and pinned to the checked-in package. Remove it
when portable-pty exposes creation flags and Muxel can select no cursor
inheritance without changing the dependency. Keep source changes bounded to
the creation flag and associated tests.
