//! Read aloud: the words an agent pane's last reply is spoken as.
//!
//! The accessibility read-aloud button (and its auto-read-when-finished option)
//! speaks the *model's* words — the summary an agent writes when it is done — and
//! never the code it changed. This module is the pure half: given a pane's text it
//! finds the reply and turns it into plain sentences. The app gathers the text and
//! hands the result to `muxel::tts`.
//!
//! Two ways in, one way out:
//!
//! - [`reply_from_claude_transcript`] reads Claude's on-disk session transcript. It
//!   is exact: the reply's own markdown, fences and all, and every tool call — each
//!   edit, each diff, each command — is a separate entry that is simply never read.
//! - [`reply_from_screen`] serves every other agent, from the terminal's text. Agent
//!   TUIs render a turn as bullet blocks (`⏺` Claude, `•` Codex, `✦` Gemini); a tool
//!   call is recognisable by its shape (`Read(src/app.rs)`, `Edited x (+3 -1)`) or by
//!   the `⎿`/`└` connectors its output hangs from; and the composer and its footer
//!   sit at the bottom, from the last prompt glyph down.
//! - [`speakable`] turns either into sentences: code blocks, diffs, tables and
//!   code-looking lines go; bullets, emphasis, links and emoji are flattened.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much of the last reply to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadAloudScope {
    /// The agent's final message — what it wrote after its last tool call. For a
    /// coding agent, that is its summary of what it did.
    #[default]
    FinalMessage,
    /// Everything the agent wrote in its last turn — still never its tool output.
    WholeTurn,
}

/// When replies are read without the button being pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadAloudAuto {
    /// Only on request (the toolbar button, its shortcut, the palette).
    #[default]
    Off,
    /// When the agent in the focused pane finishes.
    Focused,
    /// When any agent finishes — one reply at a time, in the order they finished.
    All,
}

/// What [`speakable`] keeps, drops and rewrites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakOptions {
    /// Say "link" instead of spelling out a URL.
    pub skip_urls: bool,
    /// Say only a path's file name (`src/app.rs:120` → `app.rs`).
    pub shorten_paths: bool,
    /// Read each table row as a list of its cells, rather than skipping tables.
    pub read_tables: bool,
    /// Stop after about this many characters, at a sentence boundary (0 = no limit).
    pub max_chars: usize,
}

impl Default for SpeakOptions {
    fn default() -> Self {
        Self {
            skip_urls: true,
            shorten_paths: true,
            read_tables: false,
            max_chars: 0,
        }
    }
}

/// Whether a pane running `program` is an agent worth auto-reading when it
/// finishes. A shell "finishes" every command it runs, and its output is not a
/// reply; `None` is the user's default shell.
pub fn is_agent_program(program: Option<&str>) -> bool {
    const SHELLS: &[&str] = &[
        "sh",
        "bash",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "tcsh",
        "csh",
        "nu",
        "elvish",
        "xonsh",
        "pwsh",
        "powershell",
        "cmd",
    ];
    let Some(program) = program.map(str::trim).filter(|p| !p.is_empty()) else {
        return false;
    };
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let name = name
        .strip_suffix(".exe")
        .unwrap_or(name)
        .to_ascii_lowercase();
    !SHELLS.contains(&name.as_str())
}

// --- The reply, from a Claude transcript -------------------------------------

/// The last reply in a Claude session transcript
/// (`~/.claude/projects/<slug>/<session>.jsonl`), as the markdown the model wrote.
/// `jsonl` may be just the file's tail: a torn first line is skipped like any other
/// line that doesn't parse.
///
/// A turn runs from one prompt the user sent to the next. The assistant's `text`
/// blocks are the reply; its `tool_use` blocks (edits, commands, reads) are never
/// read, and nor is its thinking. A turn with no text yet — a slash command, a
/// prompt only just sent — reads the turn before it instead.
pub fn reply_from_claude_transcript(jsonl: &str, scope: ReadAloudScope) -> Option<String> {
    #[derive(PartialEq)]
    enum Item {
        Text(String),
        Tool,
    }
    fn has_text(turn: &[Item]) -> bool {
        turn.iter().any(|item| matches!(item, Item::Text(_)))
    }
    fn text(item: &Item) -> Option<&str> {
        match item {
            Item::Text(text) => Some(text),
            Item::Tool => None,
        }
    }

    let mut turn: Vec<Item> = Vec::new();
    let mut previous: Vec<Item> = Vec::new();
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Subagents' work is interleaved in the same file; it isn't the reply.
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = &entry["message"]["content"];
        match entry.get("type").and_then(Value::as_str) {
            Some("user") => {
                if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                // Tool results come back as user entries too; only a prompt the
                // user sent starts a new turn.
                let prompt = content.is_string()
                    || content
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] != "tool_result"));
                if prompt {
                    if has_text(&turn) {
                        previous = std::mem::take(&mut turn);
                    } else {
                        turn.clear();
                    }
                }
            }
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = block["text"]
                                .as_str()
                                .map(str::trim)
                                .filter(|t| !t.is_empty())
                            {
                                turn.push(Item::Text(text.to_string()));
                            }
                        }
                        Some("tool_use") | Some("server_tool_use") => turn.push(Item::Tool),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let turn = if has_text(&turn) { turn } else { previous };
    let texts: Vec<&str> = match scope {
        ReadAloudScope::WholeTurn => turn.iter().filter_map(text).collect(),
        ReadAloudScope::FinalMessage => {
            // Back from the last text to the tool call before it: a turn that ends
            // on a tool call (still working) reads what it said before that call.
            let last = turn
                .iter()
                .rposition(|item| matches!(item, Item::Text(_)))?;
            let first = turn[..=last]
                .iter()
                .rposition(|item| *item == Item::Tool)
                .map_or(0, |tool| tool + 1);
            turn[first..=last].iter().filter_map(text).collect()
        }
    };
    let reply = texts.join("\n\n");
    (!reply.is_empty()).then_some(reply)
}

/// Whether the end of `reply` can be seen in `screen` — the check that a transcript
/// still belongs to the conversation in the pane, and not to one the agent has
/// since switched away from. Letters and digits only: rendering reflows,
/// re-indents and restyles everything else, and drops a link's target.
pub fn reply_on_screen(reply: &str, screen: &str) -> bool {
    const TAIL: usize = 40;
    fn letters(s: &str) -> Vec<char> {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
    let reply = letters(&strip_link_targets(reply));
    let tail: String = reply[reply.len().saturating_sub(TAIL)..].iter().collect();
    let screen: String = letters(screen).into_iter().collect();
    !tail.is_empty() && screen.contains(&tail)
}

/// `[label](target)` → `[label]`: the target is never rendered on screen.
fn strip_link_targets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        out.push_str(&rest[..=at]);
        rest = &rest[at + 2..];
        match rest.find(')') {
            Some(close) => rest = &rest[close + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

// --- The reply, from the terminal --------------------------------------------

/// Glyphs that open a prompt line — the composer, and every prompt already sent:
/// `❯`/`>` Claude, `›` Codex, `▌` older Codex, `>` Gemini (inside its box).
const PROMPT_GLYPHS: &[char] = &['❯', '>', '›', '▌'];
/// Glyphs agent TUIs put in front of each message and each tool call.
const BULLETS: &[char] = &['⏺', '●', '•', '✦', '◆'];
/// Spinner and status glyphs: a block that opens with one is agent chrome
/// (`✻ Worked for 1m 3s`, `✶ Thinking…`, `※ Tip: …`).
const STATUS_GLYPHS: &[char] = &['✻', '✶', '✳', '✢', '✽', '·', '※', '∗'];
/// Connectors a tool call's output hangs from (`⎿` Claude, `└` Codex).
const TOOL_OUTPUT: &[char] = &['⎿', '└'];
/// How far up from the bottom the composer can sit, in non-blank lines: past its
/// own footer (mode line, hints, context meter, a tmux status bar) and no further.
const COMPOSER_REACH: usize = 12;
/// A line indented this far is right-aligned chrome (`◉ xhigh · /effort`), not part
/// of the message it happens to sit under.
const RIGHT_ALIGNED: usize = 24;

/// The last reply in an agent pane's text (scrollback and screen, oldest first),
/// with tool calls, their output and the TUI's chrome left out. Still in whatever
/// markdown the agent rendered — pass it through [`speakable`].
///
/// A pane with no bullets to go by — an agent that prints plain text — has no
/// message boundaries to find, so its final message is its last paragraph.
pub fn reply_from_screen(screen: &str, scope: ReadAloudScope) -> Option<String> {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    // Right-aligned chrome (`◉ xhigh · /effort`) belongs to no message.
    let region: Vec<&str> = last_turn(&lines)
        .iter()
        .copied()
        .filter(|line| indent(line) < RIGHT_ALIGNED)
        .collect();
    let region = region.as_slice();
    let blocks = split_blocks(region);
    let structured = blocks
        .iter()
        .any(|b| b.head.trim_start().starts_with(BULLETS));
    let text = if structured {
        let kinds: Vec<Kind> = blocks.iter().map(kind).collect();
        let chosen: Vec<&Block> = match scope {
            ReadAloudScope::WholeTurn => blocks
                .iter()
                .zip(&kinds)
                .filter(|(_, kind)| **kind == Kind::Message)
                .map(|(block, _)| block)
                .collect(),
            ReadAloudScope::FinalMessage => final_message(&blocks, &kinds),
        };
        chosen
            .iter()
            .map(|&block| block_text(block))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        match scope {
            ReadAloudScope::WholeTurn => region.join("\n"),
            ReadAloudScope::FinalMessage => last_paragraph(region),
        }
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Whether `line` is a prompt line. The glyph only counts at the left edge — or
/// just inside a box edge, where Gemini draws its composer — so a `>` quoted inside
/// an indented message never does.
fn prompt_line(line: &str) -> bool {
    let s = match line.strip_prefix(['│', '┃']) {
        Some(inside) => inside.trim_start(),
        None => line,
    };
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if PROMPT_GLYPHS.contains(&c))
        && chars.next().is_none_or(char::is_whitespace)
}

/// Box-drawing characters: rules, frames, table borders.
fn is_box_char(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// Whether `line` is only rules and box edges (`────`, `╭──╮`, `│      │`).
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| is_box_char(c) || c.is_whitespace())
}

fn indent(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// `line` without its first `n` whitespace characters.
fn dedent(line: &str, n: usize) -> &str {
    let mut rest = line;
    for _ in 0..n {
        match rest.chars().next() {
            Some(c) if c.is_whitespace() => rest = &rest[c.len_utf8()..],
            _ => break,
        }
    }
    rest
}

/// The last prompt sent in an agent pane's text — the one that opened the turn on
/// screen — without its glyph, with the lines it wrapped onto. `None` when it has
/// scrolled out of the buffer, or the agent draws no prompt glyphs to go by. The
/// prompt still being typed in the composer is not one that was sent.
pub fn prompt_from_screen(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let end = composer_start(&lines);
    let at = lines[..end].iter().rposition(|line| prompt_line(line))?;
    let first = lines[at]
        .trim_start_matches(['│', '┃'])
        .trim_start()
        .trim_start_matches(PROMPT_GLYPHS)
        .trim_end_matches(['│', '┃'])
        .trim();
    let mut text = vec![first];
    for line in &lines[at + 1..end] {
        if line.trim().is_empty() || !line.starts_with(char::is_whitespace) {
            break;
        }
        text.push(line.trim());
    }
    let text = text.join("\n").trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The lines of the last turn: after the last prompt that was sent, up to the
/// composer. Either end may be missing — no composer in view, or a turn so long its
/// prompt scrolled out of the buffer — and then the turn runs to that edge.
fn last_turn<'l, 'a>(lines: &'l [&'a str]) -> &'l [&'a str] {
    let end = composer_start(lines);
    // The prompt that opened the turn, and the lines it wrapped onto.
    let start = match lines[..end].iter().rposition(|line| prompt_line(line)) {
        Some(prompt) => {
            let mut start = prompt + 1;
            while start < end
                && !lines[start].trim().is_empty()
                && lines[start].starts_with(char::is_whitespace)
            {
                start += 1;
            }
            start
        }
        None => 0,
    };
    &lines[start..end]
}

/// Where the composer begins — the rules framing it included — or the end of the
/// buffer when no composer is in view.
fn composer_start(lines: &[&str]) -> usize {
    let mut end = lines.len();
    let mut seen = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        if line.trim().is_empty() {
            continue;
        }
        if prompt_line(line) {
            if is_composer(lines, i) {
                end = i;
                // …and the rules framing it.
                while end > 0 && (is_rule(lines[end - 1]) || lines[end - 1].trim().is_empty()) {
                    end -= 1;
                }
            }
            break;
        }
        seen += 1;
        if seen >= COMPOSER_REACH {
            break;
        }
    }
    end
}

/// Whether the prompt line at `i` is the composer rather than a prompt already
/// sent. Under a sent prompt comes the reply, at the margin; under the composer
/// only its own wrapped lines, its frame and its (indented) footer.
fn is_composer(lines: &[&str], i: usize) -> bool {
    lines[i + 1..]
        .iter()
        .find(|line| !line.trim().is_empty() && !line.starts_with(char::is_whitespace))
        .is_none_or(|line| line.starts_with(is_box_char))
}

/// One message or tool call as a TUI draws it: a header at the left margin and the
/// indented lines under it.
struct Block<'a> {
    /// Empty for lines that precede any header — the tail of a message whose own
    /// header scrolled out of the buffer.
    head: &'a str,
    body: Vec<&'a str>,
}

fn split_blocks<'a>(region: &[&'a str]) -> Vec<Block<'a>> {
    let mut out: Vec<Block<'a>> = Vec::new();
    for &line in region {
        let continues = line.is_empty() || line.starts_with(char::is_whitespace);
        match out.last_mut() {
            Some(block) if continues => block.body.push(line),
            _ if line.is_empty() => {}
            _ if continues => out.push(Block {
                head: "",
                body: vec![line],
            }),
            _ => out.push(Block {
                head: line,
                body: Vec::new(),
            }),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Words the model wrote.
    Message,
    /// A tool call and its output.
    Tool,
    /// Spinners, timers, tips, separators.
    Chrome,
}

fn kind(block: &Block) -> Kind {
    let output_hangs = block
        .body
        .iter()
        .any(|line| line.trim_start().starts_with(TOOL_OUTPUT));
    let head = block.head.trim_start();
    let Some(first) = head.chars().next() else {
        return if output_hangs {
            Kind::Tool
        } else {
            Kind::Message
        };
    };
    if STATUS_GLYPHS.contains(&first) || head.contains("esc to interrupt") || prompt_line(head) {
        return Kind::Chrome;
    }
    if is_box_char(first) {
        // `─ Worked for 38s ───` is a separator; a box drawn at the margin is a
        // tool panel (Gemini draws each call in one).
        return if matches!(first, '─' | '━' | '═') {
            Kind::Chrome
        } else {
            Kind::Tool
        };
    }
    if output_hangs || is_tool_call(strip_bullet(head)) {
        Kind::Tool
    } else {
        Kind::Message
    }
}

fn strip_bullet(line: &str) -> &str {
    let t = line.trim_start();
    match t.strip_prefix(BULLETS) {
        Some(rest) => rest.trim_start(),
        None => t,
    }
}

/// Whether a block's header is a tool call rather than prose: `Read(src/app.rs)`,
/// `mcp__github__search (MCP)(…)`, a collapsed `Read 3 files (ctrl+o to expand)`,
/// or a Codex edit summary `Edited src/app.rs (+3 -1)`.
fn is_tool_call(text: &str) -> bool {
    let name_len: usize = text
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        .map(char::len_utf8)
        .sum();
    (name_len > 0 && text[name_len..].starts_with('('))
        || text.contains("(MCP)")
        || text.contains("to expand)")
        || has_edit_stat(text)
}

/// `… (+3 -1)` — the line counts Codex prints after a file it edited, added or
/// deleted.
fn has_edit_stat(text: &str) -> bool {
    let Some((_, inner)) = text
        .trim_end()
        .strip_suffix(')')
        .and_then(|t| t.rsplit_once('('))
    else {
        return false;
    };
    let signed = |part: Option<&str>, sign: char| {
        part.and_then(|p| p.strip_prefix(sign))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    let mut parts = inner.split_whitespace();
    signed(parts.next(), '+') && signed(parts.next(), '-') && parts.next().is_none()
}

/// The final message: the run of message blocks at the end of the turn, skipping
/// any chrome between them. A turn that ends on a tool call — an agent still at
/// work — reads the run before that call.
fn final_message<'b, 'a>(blocks: &'b [Block<'a>], kinds: &[Kind]) -> Vec<&'b Block<'a>> {
    let mut run = Vec::new();
    for (block, kind) in blocks.iter().zip(kinds).rev() {
        match kind {
            Kind::Message => run.push(block),
            Kind::Chrome => {}
            Kind::Tool if run.is_empty() => {}
            Kind::Tool => break,
        }
    }
    run.reverse();
    run
}

/// A block's words: the header without its bullet, then the body dedented.
fn block_text(block: &Block) -> String {
    let body = &block.body;
    let margin = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indent(line))
        .min()
        .unwrap_or(0);
    let head = strip_bullet(block.head);
    let mut lines: Vec<&str> = Vec::with_capacity(body.len() + 1);
    if !head.is_empty() {
        lines.push(head);
    }
    lines.extend(body.iter().map(|line| dedent(line, margin)));
    lines.join("\n").trim().to_string()
}

/// The last paragraph with any words in it.
fn last_paragraph(region: &[&str]) -> String {
    region
        .split(|line| line.trim().is_empty())
        .rev()
        .find(|paragraph| {
            paragraph
                .iter()
                .any(|line| line.chars().any(char::is_alphabetic))
        })
        .map(|paragraph| paragraph.join("\n"))
        .unwrap_or_default()
}

// --- Speakable text ------------------------------------------------------------

/// Turn a reply's markdown (or its rendered text) into sentences worth hearing.
///
/// Code never makes it through: fenced blocks, diff and patch lines, tables (unless
/// [`SpeakOptions::read_tables`]) and paragraphs that are mostly code are dropped
/// whole, and a stray code line inside prose is dropped on its own. What's left is
/// flattened — bullets, headings, emphasis, links, checkboxes and emoji go — and
/// every item ends in punctuation, so the voice pauses between them. One item per
/// line; empty when nothing speakable is left.
pub fn speakable(markdown: &str, opts: &SpeakOptions) -> String {
    let mut items: Vec<String> = Vec::new();
    for paragraph in paragraphs(markdown) {
        let code = paragraph
            .iter()
            .filter(|line| looks_like_code(line))
            .count();
        // Mostly code is a code block that lost (or never had) its fences.
        if code >= 2 && code * 2 >= paragraph.len() {
            continue;
        }
        let mut current: Option<String> = None;
        let mut quoting = false;
        for line in paragraph {
            let trimmed = line.trim();
            if looks_like_code(trimmed) || is_markdown_rule(trimmed) {
                flush(&mut current, &mut items);
                continue;
            }
            if is_table_line(trimmed) {
                flush(&mut current, &mut items);
                if opts.read_tables && !is_table_separator(trimmed) {
                    items.push(table_row(trimmed));
                }
                continue;
            }
            let kind = line_kind(trimmed);
            let was_quoting = std::mem::replace(&mut quoting, matches!(kind, LineKind::Quote(_)));
            match kind {
                LineKind::Quote(text) if !was_quoting => {
                    flush(&mut current, &mut items);
                    current = Some(text.to_string());
                }
                LineKind::Heading(text) => {
                    flush(&mut current, &mut items);
                    items.push(text.to_string());
                }
                LineKind::Item(text) => {
                    flush(&mut current, &mut items);
                    current = Some(text.to_string());
                }
                LineKind::Quote(text) | LineKind::Text(text) => match current.as_mut() {
                    Some(item) => {
                        item.push(' ');
                        item.push_str(text);
                    }
                    None => current = Some(text.to_string()),
                },
            }
        }
        flush(&mut current, &mut items);
    }
    let items: Vec<String> = items
        .iter()
        .map(|item| clean_inline(item, opts))
        .filter(|item| item.chars().any(char::is_alphanumeric))
        .map(end_sentence)
        .collect();
    truncate(items, opts.max_chars).join("\n")
}

fn flush(current: &mut Option<String>, items: &mut Vec<String>) {
    if let Some(item) = current.take() {
        items.push(item);
    }
}

/// Paragraphs (runs of non-blank lines), with fenced code blocks cut out. An
/// unclosed fence drops everything after it: that is code still being written.
fn paragraphs(text: &str) -> Vec<Vec<&str>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| t.starts_with(*m));
        match (fence, marker) {
            (Some(open), Some(close)) if open == close => {
                fence = None;
                continue;
            }
            (Some(_), _) => continue,
            (None, Some(open)) => {
                fence = Some(open);
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                continue;
            }
            (None, None) => {}
        }
        if t.trim().is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether a line reads as code rather than prose. Scored rather than matched,
/// because prose quotes code all the time (`Calls foo::bar() now.`) and a line of
/// prose lost costs more than a line of code heard.
fn looks_like_code(line: &str) -> bool {
    const DEFINITE: &[&str] = &["$ ", ">>> ", "#!", "//", "/*", "*/", "@@", "diff --git"];
    const KEYWORDS: &[&str] = &[
        "fn ",
        "pub ",
        "let ",
        "const ",
        "var ",
        "use ",
        "import ",
        "def ",
        "struct ",
        "enum ",
        "impl ",
        "trait ",
        "mod ",
        "return",
        "async ",
        "await ",
        "export ",
        "function ",
        "package ",
        "namespace ",
        "#include",
        "#[",
        "if ",
        "for ",
        "while ",
        "else",
        "match ",
        "self.",
        "this.",
        "SELECT ",
        "INSERT ",
    ];
    const OPERATORS: &[&str] = &[
        "::", "->", "=>", "==", "!=", "&&", "||", "+=", "-=", ":=", " = ", "()", "</", "/>",
    ];
    const SYMBOLS: &[char] = &[
        '{', '}', '[', ']', '(', ')', '<', '>', '=', ';', '|', '&', '\\', '$', '^', '%', '#', '@',
        '*', '~',
    ];

    let s = line.trim();
    if s.is_empty() {
        return false;
    }
    if DEFINITE.iter().any(|p| s.starts_with(p)) || s.starts_with("+++ ") || s.starts_with("--- ") {
        return true;
    }
    // Diff lines: `+added`, `-removed`, and `-    indented` removals. A markdown
    // bullet has exactly one space after its dash, and its words are judged below.
    if let Some(sign) = s.chars().next().filter(|c| matches!(c, '+' | '-')) {
        let rest = &s[1..];
        let next = rest.chars().next();
        if next.is_some_and(|c| !c.is_whitespace() && !c.is_ascii_digit() && c != sign)
            || rest.starts_with("  ")
        {
            return true;
        }
    }
    // A line-numbered patch gutter: `41 +    self.len…`, `12      fn main() {`.
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &s[digits..];
        if rest.starts_with("  ") || rest.starts_with(" + ") || rest.starts_with(" - ") {
            return true;
        }
    }
    // Only brackets and punctuation: `}`, `});`, `],`.
    if s.chars().all(|c| "{}[]();,".contains(c)) {
        return true;
    }

    let body = s.trim_start_matches(['+', '-']).trim_start();
    let mut score = 0i32;
    if KEYWORDS.iter().any(|k| body.starts_with(k)) {
        score += 2;
    }
    if body.ends_with([';', '{', '}']) {
        score += 2;
    }
    if OPERATORS.iter().any(|op| body.contains(op)) {
        score += 1;
    }
    // A call standing alone: `println!("hi")`, `run(args);`.
    let name_len: usize = body
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '!'))
        .map(char::len_utf8)
        .sum();
    if name_len > 0
        && body[name_len..].starts_with('(')
        && body.trim_end_matches(';').ends_with(')')
    {
        score += 3;
    }
    // A markup tag, `<div className="x">`, or a JSON key, `"name": "muxel",`.
    if (body.starts_with('<') && body.ends_with('>'))
        || (body.starts_with('"') && body.contains("\":"))
    {
        score += 3;
    }
    let symbols = body.chars().filter(|c| SYMBOLS.contains(c)).count();
    let words = body
        .split(|c: char| !c.is_alphabetic() && c != '\'')
        .filter(|w| w.chars().count() >= 2)
        .count();
    if symbols >= 3 && symbols >= words {
        score += 2;
    }
    // Sentences read as prose, whatever they quote.
    if body.ends_with(['.', '!', '?', ':']) && words >= 4 {
        score -= 2;
    }
    if words >= 8 {
        score -= 1;
    }
    score >= 3
}

/// `---`, `***`, `___`, or a drawn rule.
fn is_markdown_rule(t: &str) -> bool {
    let mut chars = t.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_' | '─' | '━' | '═')
        && t.chars().filter(|c| !c.is_whitespace()).count() >= 3
        && chars.all(|c| c == first)
}

fn is_table_line(t: &str) -> bool {
    t.contains(['│', '┃'])
        || (t.starts_with('|') && t.matches('|').count() >= 2)
        || t.starts_with(['┌', '├', '└', '╭', '╰', '┏', '┣', '┗'])
}

/// `|---|:--:|`, `├──┼──┤`: a table's rules, never worth reading.
fn is_table_separator(t: &str) -> bool {
    t.chars()
        .all(|c| matches!(c, '|' | '-' | ':' | '+' | ' ') || is_box_char(c))
}

fn table_row(t: &str) -> String {
    t.split(['|', '│', '┃'])
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

enum LineKind<'a> {
    Heading(&'a str),
    Item(&'a str),
    /// A `> quoted` line: an item of its own, continued by the quote's next line.
    Quote(&'a str),
    Text(&'a str),
}

/// A trimmed line's markdown structure: a heading, the start of a list item, or
/// text that continues whatever came before it.
fn line_kind(t: &str) -> LineKind<'_> {
    const MARKERS: &[&str] = &["- ", "* ", "+ ", "• ", "◦ ", "▪ ", "‣ ", "⁃ ", "○ ", "■ "];
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        return LineKind::Heading(t[hashes..].trim());
    }
    if let Some(quoted) = t.strip_prefix('>') {
        return LineKind::Quote(quoted.trim_start());
    }
    if let Some(rest) = MARKERS.iter().find_map(|m| t.strip_prefix(m)) {
        return LineKind::Item(strip_checkbox(rest));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits)
        && let Some(rest) = t[digits..]
            .strip_prefix(". ")
            .or_else(|| t[digits..].strip_prefix(") "))
    {
        return LineKind::Item(strip_checkbox(rest));
    }
    let unchecked = strip_checkbox(t);
    if unchecked.len() != t.len() {
        return LineKind::Item(unchecked);
    }
    LineKind::Text(t)
}

/// Drop a leading task checkbox: `[ ]`, `[x]`, or a todo glyph (`☐`, `☒`, `✔`).
fn strip_checkbox(t: &str) -> &str {
    const BOXES: &[&str] = &[
        "[ ] ", "[x] ", "[X] ", "☐ ", "☒ ", "☑ ", "✓ ", "✔ ", "✗ ", "✘ ", "✅ ", "❌ ", "⬜ ",
    ];
    BOXES
        .iter()
        .find_map(|b| t.strip_prefix(b))
        .map_or(t, str::trim_start)
}

/// Flatten one item's inline markdown into plain words for the voice.
fn clean_inline(item: &str, opts: &SpeakOptions) -> String {
    let text = replace_links(item)
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "");
    let text = text
        .split_whitespace()
        .map(|word| clean_word(word, opts))
        .collect::<Vec<_>>()
        .join(" ");
    let text: String = text
        .replace(" -> ", " to ")
        .replace(" => ", " to ")
        .replace(['→', '⇒', '⟶', '➜'], " to ")
        .replace(['`', '*'], "")
        // `snake_case` names, which rendered markdown shows without backticks.
        .replace('_', " ")
        .replace("()", "")
        .chars()
        .filter(|&c| !is_decoration(c))
        .collect();
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for (from, to) in [
        (" ,", ","),
        (" .", "."),
        (" :", ":"),
        (" ;", ";"),
        ("( ", "("),
        (" )", ")"),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// `[label](target)` and `![alt](src)` → the label.
fn replace_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(mid) = rest[open..].find("](").map(|m| open + m) else {
            break;
        };
        let Some(close) = rest[mid..].find(')').map(|c| mid + c) else {
            break;
        };
        out.push_str(rest[..open].trim_end_matches('!'));
        out.push_str(&rest[open + 1..mid]);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// One whitespace-separated word: a URL or a path, shortened as asked; anything
/// else untouched. Surrounding punctuation is kept around the shortened form.
fn clean_word(word: &str, opts: &SpeakOptions) -> String {
    let lead_len = word.len()
        - word
            .trim_start_matches(['(', '[', '<', '"', '\'', '`'])
            .len();
    let core_and_tail = &word[lead_len..];
    let core = core_and_tail
        .trim_end_matches([')', ']', '>', '"', '\'', '`', ',', '.', ';', ':', '!', '?']);
    let (lead, tail) = (&word[..lead_len], &core_and_tail[core.len()..]);
    let lead = lead.trim_matches('<');
    let tail = tail.trim_start_matches('>');
    if core.starts_with("http://") || core.starts_with("https://") || core.starts_with("www.") {
        if opts.skip_urls {
            return format!("{lead}link{tail}");
        }
        return word.to_string();
    }
    if opts.shorten_paths
        && let Some(name) = file_name(core)
    {
        return format!("{lead}{name}{tail}");
    }
    word.to_string()
}

/// The file name a path-looking word ends in: `src/app.rs:120:4` → `app.rs`,
/// `~/.config/muxel` → `muxel`. `and/or`-style pairs are not paths.
fn file_name(word: &str) -> Option<&str> {
    // Drop a `:line` / `:line:col` suffix.
    let mut path = word;
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':')
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            path = head;
        }
    }
    let path = path.trim_end_matches(['/', '\\']);
    let segments: Vec<&str> = path.split(['/', '\\']).collect();
    if segments.len() < 2 {
        return None;
    }
    let last = *segments.last()?;
    let rooted = ["/", "./", "../", "~/", "~\\", ".\\"]
        .iter()
        .any(|p| path.starts_with(p))
        || path.as_bytes().get(1) == Some(&b':');
    let has_extension = last
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty());
    let pathlike = rooted || has_extension || segments.len() >= 3;
    (pathlike && last.chars().any(char::is_alphabetic)).then_some(last)
}

/// Characters a voice would read out loud as their names ("check mark button",
/// "box drawings light horizontal") and that carry nothing a listener needs:
/// emoji, dingbats, arrows, box drawing, geometric shapes, and the joiners and
/// selectors that glue emoji together.
fn is_decoration(c: char) -> bool {
    matches!(c as u32,
        0x2022 // •
        | 0x2190..=0x21FF // arrows
        | 0x2300..=0x23FF // miscellaneous technical (⏺ ⎿ ⏵ ⌘)
        | 0x2500..=0x25FF // box drawing, blocks, geometric shapes
        | 0x2600..=0x27BF // miscellaneous symbols, dingbats (✓ ✔ ★ ☐)
        | 0x2B00..=0x2BFF // miscellaneous symbols and arrows (⬜)
        | 0x1F000..=0x1FAFF // emoji
        | 0x200D | 0xFE0E | 0xFE0F | 0x20E3)
}

/// End an item with punctuation, so the voice pauses before the next one.
fn end_sentence(item: String) -> String {
    match item.chars().last() {
        Some(c) if c.is_alphanumeric() || matches!(c, ')' | '"' | '\'' | '’' | '”') => {
            format!("{item}.")
        }
        _ => item,
    }
}

/// Longest piece read-aloud speaks in one go. A paused reading resumes from the
/// start of the piece it stopped in, so this bounds how much a resume repeats —
/// while short sentences grouped together spare the voice a restart for each.
/// A sentence is never split, so one longer than this is a piece of its own.
pub const CHUNK_CHARS: usize = 160;

/// [`speakable`] text as the pieces it is spoken in, so a reading can be paused
/// and resumed (and a pane can say how far it got): whole sentences, with short
/// neighbours grouped up to [`CHUNK_CHARS`]. Pieces of one item are joined by a
/// space, different items by a line break, which the voice pauses on.
pub fn chunks(spoken: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in spoken.lines().map(str::trim).filter(|l| !l.is_empty()) {
        for (k, sentence) in split_sentences(line).into_iter().enumerate() {
            let mut sep = if k == 0 { "\n" } else { " " };
            if current.is_empty() {
                sep = "";
            } else if current.chars().count() + 1 + sentence.chars().count() > CHUNK_CHARS {
                out.push(std::mem::take(&mut current));
                sep = "";
            }
            current.push_str(sep);
            current.push_str(&sentence);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One line's sentences: it breaks after a run of `.` `!` `?` `…` (and any closing
/// quote or bracket) that is followed by a space — so `app.rs`, `v1.2` and `3.14`
/// stay whole, which a split at every full stop would read as two halves.
fn split_sentences(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '.' | '!' | '?' | '…') {
            let mut end = i + 1;
            while end < chars.len()
                && matches!(
                    chars[end],
                    '.' | '!' | '?' | '…' | '"' | '\'' | '”' | '’' | ')' | ']'
                )
            {
                end += 1;
            }
            if end == chars.len() || chars[end].is_whitespace() {
                let sentence: String = chars[start..end].iter().collect();
                if !sentence.trim().is_empty() {
                    out.push(sentence.trim().to_string());
                }
                start = end;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    let tail: String = chars[start..].iter().collect();
    if !tail.trim().is_empty() {
        out.push(tail.trim().to_string());
    }
    out
}

/// Keep whole items while they fit in `max` characters; of the item that crosses
/// the line, keep the sentences that fit. The very first item is cut at a word if
/// it has to be, so a limit never leaves nothing to say.
fn truncate(items: Vec<String>, max: usize) -> Vec<String> {
    if max == 0 {
        return items;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for item in items {
        let len = item.chars().count() + 1;
        if used + len <= max {
            used += len;
            out.push(item);
            continue;
        }
        let room = max.saturating_sub(used);
        let cut = cut_at_sentence(&item, room)
            .or_else(|| out.is_empty().then(|| cut_at_word(&item, room)).flatten());
        if let Some(cut) = cut {
            out.push(cut);
        }
        break;
    }
    out
}

/// The longest run of whole sentences in `item` that fits in `room` characters.
fn cut_at_sentence(item: &str, room: usize) -> Option<String> {
    let mut best = None;
    let chars: Vec<char> = item.chars().collect();
    for (i, pair) in chars.windows(2).enumerate() {
        if i + 1 > room {
            break;
        }
        if matches!(pair[0], '.' | '!' | '?') && pair[1] == ' ' {
            best = Some(i + 1);
        }
    }
    best.map(|end| chars[..end].iter().collect())
}

/// As many whole words of `item` as fit in `room` characters, marked as cut off.
fn cut_at_word(item: &str, room: usize) -> Option<String> {
    let mut out = String::new();
    for word in item.split_whitespace() {
        let next = if out.is_empty() {
            word.chars().count()
        } else {
            out.chars().count() + 1 + word.chars().count()
        };
        if next + 1 > room {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    (!out.is_empty()).then(|| format!("{out}…"))
}

#[cfg(test)]
mod tests {
    use super::{
        CHUNK_CHARS, ReadAloudScope, SpeakOptions, chunks, is_agent_program, looks_like_code,
        prompt_from_screen, reply_from_claude_transcript, reply_from_screen, reply_on_screen,
        speakable, split_sentences,
    };
    use serde_json::json;

    const CLAUDE_SCREEN: &str = "\
❯ fix the off-by-one in the pager

⏺ I'll look at the pager first.

⏺ Read(src/pager.rs)
  ⎿  Read 120 lines

⏺ Update(src/pager.rs)
  ⎿  Updated src/pager.rs with 1 addition and 1 removal
       40      fn last_page(&self) -> usize {
       41 -        self.len / self.per_page
       41 +        self.len.div_ceil(self.per_page)
       42      }

⏺ Bash(cargo test -p pager)
  ⎿  running 12 tests
     test result: ok. 12 passed; 0 failed

⏺ Fixed the off-by-one in last_page: it now rounds up with div_ceil, so a
  partial final page is counted.

  - src/pager.rs: last_page uses div_ceil
  - All 12 pager tests pass

✻ Worked for 42s

                                                                   ◉ xhigh · /effort
────────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle)
";

    const CODEX_SCREEN: &str = "\
› fix the off-by-one in the pager

• I'll inspect the pager.

• Explored
  └ Read pager.rs

• Edited src/pager.rs (+1 -1)
    41 -        self.len / self.per_page
    41 +        self.len.div_ceil(self.per_page)

• Ran cargo test -p pager
  └ running 12 tests
    test result: ok. 12 passed

─ Worked for 38s ─────────────────────────────────────────────

• Fixed last_page to round up with div_ceil; all 12 pager tests pass.

› Ask Codex to do anything

  ? for shortcuts                                   98% context left
";

    const GEMINI_SCREEN: &str = "\
> fix the pager

✦ I'll fix the pager.

╭──────────────────────────────────╮
│ ✓  Edit src/pager.rs             │
│                                  │
│ 41 - self.len / self.per_page    │
╰──────────────────────────────────╯

✦ Fixed the pager rounding.

╭──────────────────────────────────╮
│ > Type your message              │
╰──────────────────────────────────╯
";

    fn final_msg(screen: &str) -> Option<String> {
        reply_from_screen(screen, ReadAloudScope::FinalMessage)
    }

    #[test]
    fn sentences_break_between_sentences_not_inside_words() {
        assert_eq!(
            split_sentences("Fixed app.rs in v1.2 today. Tests pass! Really? \"Yes.\" Done…"),
            vec![
                "Fixed app.rs in v1.2 today.",
                "Tests pass!",
                "Really?",
                "\"Yes.\"",
                "Done…"
            ]
        );
        assert_eq!(split_sentences("No terminator"), vec!["No terminator"]);
        assert!(split_sentences("   ").is_empty());
    }

    #[test]
    fn a_reply_is_spoken_in_sentence_sized_pieces() {
        // Short sentences share a piece; items stay on their own lines within it.
        assert_eq!(
            chunks("Fixed the pager. Tests pass.\nDocs updated."),
            vec!["Fixed the pager. Tests pass.\nDocs updated."]
        );
        // A long reply breaks between sentences, never inside one, and no piece
        // outgrows the limit unless a single sentence does.
        let long = "This sentence is long enough to matter here. ".repeat(12);
        let pieces = chunks(long.trim());
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| p.chars().count() <= CHUNK_CHARS));
        assert!(pieces.iter().all(|p| p.ends_with("matter here.")));
        assert_eq!(pieces.join(" "), long.trim());
        let huge = format!("{}.", "word ".repeat(60).trim());
        assert_eq!(chunks(&huge), vec![huge.clone()]);
        assert!(chunks("").is_empty());
    }

    #[test]
    fn only_agents_are_auto_read() {
        assert!(is_agent_program(Some("claude")));
        assert!(is_agent_program(Some("/usr/local/bin/codex")));
        assert!(!is_agent_program(None));
        assert!(!is_agent_program(Some("zsh")));
        assert!(!is_agent_program(Some(
            "C:\\Program Files\\Git\\bin\\bash.exe"
        )));
        assert!(!is_agent_program(Some("pwsh.exe")));
    }

    #[test]
    fn claude_final_message_skips_tools_and_chrome() {
        let reply = final_msg(CLAUDE_SCREEN).unwrap();
        assert_eq!(
            reply,
            "Fixed the off-by-one in last_page: it now rounds up with div_ceil, so a\n\
             partial final page is counted.\n\n\
             - src/pager.rs: last_page uses div_ceil\n\
             - All 12 pager tests pass"
        );
        // No diff, no command output, no status or footer chrome.
        for leaked in ["self.len", "running 12", "Worked for", "xhigh", "auto mode"] {
            assert!(!reply.contains(leaked), "{leaked:?} leaked into {reply:?}");
        }
    }

    #[test]
    fn claude_whole_turn_reads_every_message_but_no_tool() {
        let reply = reply_from_screen(CLAUDE_SCREEN, ReadAloudScope::WholeTurn).unwrap();
        assert!(reply.starts_with("I'll look at the pager first.\n\nFixed the off-by-one"));
        assert!(!reply.contains("Read 120 lines"));
        assert!(!reply.contains("Update("));
    }

    #[test]
    fn codex_final_message_skips_edits_and_commands() {
        assert_eq!(
            final_msg(CODEX_SCREEN).as_deref(),
            Some("Fixed last_page to round up with div_ceil; all 12 pager tests pass.")
        );
        let whole = reply_from_screen(CODEX_SCREEN, ReadAloudScope::WholeTurn).unwrap();
        assert_eq!(
            whole,
            "I'll inspect the pager.\n\n\
             Fixed last_page to round up with div_ceil; all 12 pager tests pass."
        );
    }

    #[test]
    fn gemini_boxed_tools_and_composer_are_skipped() {
        assert_eq!(
            final_msg(GEMINI_SCREEN).as_deref(),
            Some("Fixed the pager rounding.")
        );
    }

    #[test]
    fn an_agent_still_working_reads_what_it_last_said() {
        let screen = "\
❯ run the tests

⏺ Running the suite now.

⏺ Bash(cargo test)
  ⎿  Running…

✶ Thinking… (esc to interrupt)

────────
❯
────────
";
        assert_eq!(final_msg(screen).as_deref(), Some("Running the suite now."));
    }

    #[test]
    fn collapsed_and_mcp_tool_calls_are_tools() {
        let screen = "\
❯ look around

⏺ Read 3 files (ctrl+o to expand)

⏺ github - search_issues (MCP)(query: \"pager\")

⏺ Nothing in the tracker mentions the pager.
";
        assert_eq!(
            final_msg(screen).as_deref(),
            Some("Nothing in the tracker mentions the pager.")
        );
    }

    #[test]
    fn a_turn_whose_prompt_scrolled_away_starts_at_the_top() {
        let screen = "\
  with its header already gone.

⏺ Edit(src/a.rs)
  ⎿  Updated src/a.rs

⏺ All done.
";
        assert_eq!(final_msg(screen).as_deref(), Some("All done."));
        let whole = reply_from_screen(screen, ReadAloudScope::WholeTurn).unwrap();
        assert_eq!(whole, "with its header already gone.\n\nAll done.");
    }

    #[test]
    fn a_long_prompt_is_not_part_of_the_reply() {
        let screen = "\
❯ please fix the pager and then
  also update the changelog

⏺ Both done.
";
        assert_eq!(final_msg(screen).as_deref(), Some("Both done."));
    }

    #[test]
    fn the_sent_prompt_is_found_but_not_the_one_being_typed() {
        assert_eq!(
            prompt_from_screen(CLAUDE_SCREEN).as_deref(),
            Some("fix the off-by-one in the pager")
        );
        let screen = "\
❯ please fix the pager and then
  also update the changelog

⏺ Both done.

────────
❯ a half-typed second
────────
  ? for shortcuts
";
        assert_eq!(
            prompt_from_screen(screen).as_deref(),
            Some("please fix the pager and then\nalso update the changelog")
        );
        // Only the composer on screen: nothing has been sent yet.
        assert_eq!(prompt_from_screen("────────\n❯ draft\n────────\n"), None);
        // No prompt glyphs at all.
        assert_eq!(prompt_from_screen("plain output\nmore output\n"), None);
    }

    #[test]
    fn the_prompt_being_typed_is_not_read() {
        let screen = "\
❯ first question

⏺ First answer.

────────
❯ a half-typed second
  question on two lines
────────
  ? for shortcuts
";
        assert_eq!(final_msg(screen).as_deref(), Some("First answer."));
    }

    #[test]
    fn right_aligned_chrome_is_never_a_reply() {
        // Claude's idle screen: a welcome, then an effort indicator pinned right.
        let screen = format!(
            "  Get to finished work sooner.\n\n{:>70}\n────\n❯ \n────\n  ? for shortcuts\n",
            "◉ xhigh · /effort"
        );
        assert_eq!(
            final_msg(&screen).as_deref(),
            Some("Get to finished work sooner.")
        );
    }

    #[test]
    fn plain_text_agents_read_their_last_paragraph() {
        let screen = "\
> explain

Here is some context.
It spans two lines.

And this is the conclusion.
";
        assert_eq!(
            final_msg(screen).as_deref(),
            Some("And this is the conclusion.")
        );
        assert!(final_msg("").is_none());
        assert!(final_msg("❯ \n").is_none());
    }

    // --- Claude transcripts ----------------------------------------------------

    fn jsonl(entries: &[serde_json::Value]) -> String {
        entries
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prompt(text: &str) -> serde_json::Value {
        json!({"type": "user", "message": {"role": "user", "content": text}})
    }

    fn said(text: &str) -> serde_json::Value {
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
    }

    fn tool() -> serde_json::Value {
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Edit", "input": {}}]}})
    }

    fn result() -> serde_json::Value {
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}})
    }

    #[test]
    fn transcript_final_message_is_the_text_after_the_last_tool() {
        let log = jsonl(&[
            prompt("old question"),
            said("Old answer."),
            prompt("fix the pager"),
            said("Looking at the pager."),
            tool(),
            result(),
            json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "hmm"}]}}),
            said("Fixed it."),
            said("```rust\nlet x = 1;\n```\nTests pass."),
        ]);
        assert_eq!(
            reply_from_claude_transcript(&log, ReadAloudScope::FinalMessage).as_deref(),
            Some("Fixed it.\n\n```rust\nlet x = 1;\n```\nTests pass.")
        );
        assert_eq!(
            reply_from_claude_transcript(&log, ReadAloudScope::WholeTurn).as_deref(),
            Some("Looking at the pager.\n\nFixed it.\n\n```rust\nlet x = 1;\n```\nTests pass.")
        );
    }

    #[test]
    fn transcript_skips_meta_sidechains_and_torn_lines() {
        let mut log = String::from("{\"type\":\"assist"); // torn first line of a tail read
        log.push('\n');
        log.push_str(&jsonl(&[
            prompt("go"),
            said("Main answer."),
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "Subagent chatter."}]}}),
            json!({"type": "user", "isMeta": true, "message": {"content": "<system-reminder>"}}),
        ]));
        assert_eq!(
            reply_from_claude_transcript(&log, ReadAloudScope::FinalMessage).as_deref(),
            Some("Main answer.")
        );
    }

    #[test]
    fn transcript_turn_without_text_reads_the_previous_turn() {
        let log = jsonl(&[
            prompt("explain"),
            said("The explanation."),
            prompt("<command-name>/effort</command-name>"),
        ]);
        assert_eq!(
            reply_from_claude_transcript(&log, ReadAloudScope::FinalMessage).as_deref(),
            Some("The explanation.")
        );
        // Mid-turn, before any text: still the last thing said.
        let working = jsonl(&[
            prompt("explain"),
            said("The explanation."),
            prompt("more"),
            tool(),
        ]);
        assert_eq!(
            reply_from_claude_transcript(&working, ReadAloudScope::FinalMessage).as_deref(),
            Some("The explanation.")
        );
        assert!(reply_from_claude_transcript("", ReadAloudScope::FinalMessage).is_none());
    }

    #[test]
    fn transcript_is_trusted_only_when_its_reply_is_on_screen() {
        let reply = "Fixed the **pager** — see [the docs](https://example.com/pager).";
        let screen = "⏺ Fixed the pager — see the docs.\n\n────\n❯ \n";
        assert!(reply_on_screen(reply, screen));
        assert!(!reply_on_screen(reply, "⏺ Something else entirely.\n"));
        assert!(!reply_on_screen("", screen));
    }

    // --- speakable -------------------------------------------------------------

    fn speak(text: &str) -> String {
        speakable(text, &SpeakOptions::default())
    }

    #[test]
    fn fenced_code_is_never_read() {
        let md =
            "Here's the fix:\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nThat's all.";
        assert_eq!(speak(md), "Here's the fix:\nThat's all.");
        // An unclosed fence is code still being written.
        assert_eq!(speak("Start.\n```\nlet x = 1;"), "Start.");
    }

    #[test]
    fn unfenced_code_and_diffs_are_dropped() {
        let md = "I changed the loop.\n\n    for i in 0..n {\n        total += i;\n    }\n\n\
                  @@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;\n\nDone.";
        assert_eq!(speak(md), "I changed the loop.\nDone.");
        // A stray code line inside prose goes on its own.
        assert_eq!(
            speak("The call is now:\nself.len.div_ceil(self.per_page);\nwhich rounds up."),
            "The call is now:\nwhich rounds up."
        );
    }

    #[test]
    fn prose_that_mentions_code_is_kept() {
        for line in [
            "Fixed the bug in foo::bar() so it no longer panics.",
            "Added read_aloud settings (scope, rate, voice)",
            "Run cargo test to check it.",
            "Use the new flag when you want quieter output.",
            "- All 12 pager tests pass",
        ] {
            assert!(!looks_like_code(line), "{line:?} judged as code");
        }
        for line in [
            "fn main() {",
            "let x = foo(bar);",
            "}",
            "});",
            "use std::io;",
            "self.settings.tts_rate = 1.0;",
            "#[derive(Clone)]",
            "$ cargo build",
            "41 +        self.len.div_ceil(self.per_page)",
            "-    let old = 1;",
            "+let new = 2;",
            "println!(\"hi\");",
            "<div className=\"x\">",
        ] {
            assert!(looks_like_code(line), "{line:?} judged as prose");
        }
    }

    #[test]
    fn markdown_is_flattened_into_sentences() {
        let md = "## Summary\n\nFixed the **pager** and the `last_page` helper.\n\n\
                  - [x] Tests pass\n- Updated [the docs](https://example.com)\n\
                  1. First step\n2) Second step\n> A quoted note";
        assert_eq!(
            speak(md),
            "Summary.\nFixed the pager and the last page helper.\nTests pass.\n\
             Updated the docs.\nFirst step.\nSecond step.\nA quoted note."
        );
    }

    #[test]
    fn wrapped_lines_join_back_into_one_item() {
        let md = "Fixed the off-by-one in last_page: it now rounds up, so a\n\
                  partial final page is counted.\n\n- one item that\n  wraps\n- another";
        assert_eq!(
            speak(md),
            "Fixed the off-by-one in last page: it now rounds up, so a partial final page is counted.\n\
             one item that wraps.\nanother."
        );
    }

    #[test]
    fn urls_and_paths_are_shortened_unless_asked_not_to() {
        let md = "See https://example.com/a/b and src/pager.rs:41 (or ~/notes/todo.md).";
        assert_eq!(speak(md), "See link and pager.rs (or todo.md).");
        let verbatim = SpeakOptions {
            skip_urls: false,
            shorten_paths: false,
            ..SpeakOptions::default()
        };
        assert_eq!(
            speakable(md, &verbatim),
            "See https://example.com/a/b and src/pager.rs:41 (or ~/notes/todo.md)."
        );
        // `and/or` is a word, not a path.
        assert_eq!(
            speak("Use one and/or the other."),
            "Use one and/or the other."
        );
    }

    #[test]
    fn tables_are_skipped_or_read_as_cells() {
        let md = "Results:\n\n| File | Change |\n|------|--------|\n| a.rs | fixed |\n\nDone.";
        assert_eq!(speak(md), "Results:\nDone.");
        let tables = SpeakOptions {
            read_tables: true,
            ..SpeakOptions::default()
        };
        assert_eq!(
            speakable(md, &tables),
            "Results:\nFile, Change.\na.rs, fixed.\nDone."
        );
        // Tables as a TUI draws them.
        let drawn = "Status:\n┌──────┬───────┐\n│ a.rs │ fixed │\n└──────┴───────┘\nOk.";
        assert_eq!(speak(drawn), "Status:\nOk.");
    }

    #[test]
    fn emoji_and_symbols_are_not_read_as_their_names() {
        assert_eq!(speak("✅ All green 🎉"), "All green.");
        assert_eq!(speak("old → new"), "old to new.");
        assert_eq!(speak("a -> b"), "a to b.");
        assert_eq!(speak("⏺ ───"), "");
    }

    #[test]
    fn max_chars_stops_at_a_sentence() {
        let opts = SpeakOptions {
            max_chars: 30,
            ..SpeakOptions::default()
        };
        assert_eq!(
            speakable("First sentence here. Second one is long.", &opts),
            "First sentence here."
        );
        assert_eq!(
            speakable("Short.\nAnother item that does not fit at all.", &opts),
            "Short."
        );
        // Even one long sentence says something.
        assert_eq!(
            speakable("An extremely long opening sentence that never ends", &opts),
            "An extremely long opening…"
        );
    }
}
