# Current provider state fixtures

Captured on 2026-10-26 through headless ConPTY 1.25.260930003 at 120 columns by
40 rows. The JSON retains the rendered text, title, cursor and paste-mode facts
from Muxel's own terminal parser, plus each original stream prefix's byte cutoff
and SHA-256. A 200 ms wait followed by an empty drain expired incomplete DEC 2026
updates before capture. Raw streams remain private and are not committed.

| Provider | Actual CLI | Model / invocation |
| --- | --- | --- |
| Claude Code | 2.1.310 | Sonnet 5.5, low effort; no 7501 capability answer |
| Codex | 0.164.0 | Daybreak Blue, low effort; read-only sandbox |
| Grok | 1.0.70 stable, build f50904eaee6c | Grok 4.7, xhigh |

Each provider's normal turn began at an idle composer, received one exact
`B1_OK` task and completed it. Done fixtures replay the observed Working sample
and the submitted turn before the completed grid; an idle-looking final grid
alone is not treated as evidence of completion. Codex's idle grid comes from
its question run's settled startup, before that run submitted its task.

Each provider separately displayed a real unanswered Alpha/Beta question.
Claude exposed only AskUserQuestion with manual permission mode. Codex enabled
`default_mode_request_user_input` for that invocation; its default is false.
Grok exposed only ask_user_question. No answers or approvals were submitted.
The ordinary runs disabled tools where supported and requested no tool use.

A separate Codex command-approval capture was added after review exposed a
completion hazard. Its native report was `blocked:kind=permission`, while its
title changed from a bare UUID to `Request approval for marker`, with no
Action Required title. The journal contains one escalated exec request and no
result or completed turn; its target file stayed absent. The per-invocation
reviewer was the manual user path, leaving the approval pending. No global
setting changed and no approval was sent. This thirteenth sample tests the
actual approval form; an additional synthetic unknown-modal/repaint regression
requires the live editor to return before a screen-only Codex completion.

The captures exposed Codex's bare UUID title, live Working row and question
footer, and Grok's question card while its title still said Working. These
fixtures therefore exercise screen fallback rather than an assumed title.
The optional 7501 capability reply was disabled for all captures; report bytes
that providers emitted anyway do not appear in these rendered-screen fixtures.

Sanitization replaces local project paths with a padded `[project]`, replaces
session UUIDs with a fixed fixture UUID, removes the personal status-bar row,
and removes trailing blank cells. Row order, cursor coordinates, provider
chrome and literal captured content otherwise remain intact. Grok's question
capture includes a stray `[?6c` before the user prompt from this helper's DA
reply leakage; it is preserved and does not belong to the question card.

Every native child inherited an environment stripped of parent/control hints;
the captures recorded key names only and an empty remaining-key list. Nine
bounded jobs were retained, including initial startup refusals. All exited,
all reader threads joined, and final root/owned-console checks were empty.
No trust/auth configuration, global setting or GUI was changed.
