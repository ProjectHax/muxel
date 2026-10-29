# Current provider state fixtures

Captured on 2026-10-26 through headless ConPTY 1.25.260930003 at 120 columns by
40 rows. The JSON retains the rendered text, title, cursor and paste-mode facts
from Muxel's own terminal parser, plus each original stream prefix's byte cutoff
and SHA-256. A 200 ms wait followed by an empty drain expired incomplete DEC 2026
updates before capture. Raw streams remain private and are not committed.

| Provider | Actual CLI | Model / invocation |
| --- | --- | --- |
| Claude Code | 2.1.310 | Sonnet 5.5, low effort; no 7501 capability answer |
| Grok | 1.0.70 stable, build f50904eaee6c | Grok 4.7, xhigh |

Each provider's normal turn began at an idle composer, received one exact
`B1_OK` task and completed it. Done fixtures replay the observed Working sample
and the submitted turn before the completed grid; an idle-looking final grid
alone is not treated as evidence of completion.

Each provider separately displayed a real unanswered Alpha/Beta question.
Claude exposed only AskUserQuestion with manual permission mode. Grok exposed
only ask_user_question. No answers or approvals were submitted. The ordinary
runs disabled tools where supported and requested no tool use.

The captures exposed Grok's question card while its title still said Working,
so the Grok blocked fixture exercises screen fallback rather than an assumed
title. Codex 0.164.0 was captured in the same session; its samples were dropped
with the Codex screen parsing they served, and Codex's status reads are
unchanged from master. The optional 7501 capability reply was disabled for all
captures; report bytes that providers emitted anyway do not appear in these
rendered-screen fixtures.

Sanitization replaces local project paths with a padded `[project]`, removes
the personal status-bar row, and removes trailing blank cells. Row order,
cursor coordinates, provider chrome and literal captured content otherwise
remain intact. Grok's question capture includes a stray `[?6c` before the user
prompt from this helper's DA reply leakage; it is preserved and does not belong
to the question card.

Every native child inherited an environment stripped of parent/control hints;
the captures recorded key names only and an empty remaining-key list. Nine
bounded jobs were retained across the session, including initial startup
refusals. All exited, all reader threads joined, and final root/owned-console
checks were empty. No trust/auth configuration, global setting or GUI was
changed.
