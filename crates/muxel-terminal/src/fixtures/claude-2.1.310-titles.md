# Claude 2.1.310 title fixtures

Captured on 2026-10-26 with the real Windows Claude Code 2.1.310 executable,
Sonnet and low effort, in two headless ConPTY runs. Each used a positional
prompt and system prompt requesting exactly `OK`, disabled built-in tools,
empty strict MCP configuration, a 1 MiB capture bound and a 35-second deadline.
Ordinary existing provider lifecycle hooks remained enabled. Global settings,
authentication and trust were unchanged; no Muxel GUI was launched.

These files retain the actual OSC 0/2/7501 frames in wire order, byte-for-byte,
including BEL and ST terminators. They contain no paths or session identifiers.
Full terminal output and private capture metadata are excluded.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| claude-2.1.310-7501.osc | 171 | d4b3ca24503372e8737957c59979cf3c917439f7014380546a24d38ce9284a9f |
| claude-2.1.310-no-7501.osc | 156 | 8a88e621a8b4c16573602e181cb5d45dd85154f4e72b748331e6637cd5fc6a8a |

The first run answered the capability query before DA using the protocol's
advertised states and kinds. Claude then emitted `state=working:app=claude-code`,
glyph-less `Claude Code`, glyph-less `Exact OK reply`, and
`state=done:app=claude-code`. Neither report contains `title=`; the title-field
precedence test uses a separately identified synthetic protocol report.
The paired run did not answer the query, produced no reports, and retained the
Claude glyph through its final `✳ Exact OK reply` title.

The capture helper used the pinned ConPTY 1.25.260930003 x64 pair at 120x40,
with its reply scanner on the reader thread. It is a scoped C# headless helper,
not Muxel's production parser. Both owned jobs were terminated at the deadline,
their exits observed, readers joined, and owned processes verified absent.
Parent environment hints were not explicitly scrubbed for this capture;
the fixtures establish the observed title protocol, not session isolation.
