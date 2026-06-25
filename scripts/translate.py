#!/usr/bin/env python3
"""
scripts/translate.py — muxel i18n catalog generator.

Extract English UI strings from the Rust source (the `t("…")`, `tf("…", …)`, and
`tn("…", "…", …)` calls) and translate them into per-language JSON catalogs under
crates/muxel/assets/i18n/ via an LLM CLI (claude/sonnet by default; opencode too).

Usage:
  python3 scripts/translate.py                 # extract en.json + translate all langs
  python3 scripts/translate.py --extract-only  # just refresh en.json (the key list)
  python3 scripts/translate.py --check         # CI: exit 1 if en.json is stale
  python3 scripts/translate.py --lang es,fr    # only these languages
  python3 scripts/translate.py --backend opencode
  python3 scripts/translate.py --force         # re-translate existing entries too

The English source string IS the catalog key, so untranslated strings fall back to
English at runtime. Technical terms / product names and {placeholder} tokens are
kept verbatim (see PROMPT). stdlib-only; no pip dependencies.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

# BCP-47 tag -> language name handed to the model. Mirrors i18n::available_languages.
SUPPORTED_LANGS = {
    "es": "Spanish",
    "fr": "French",
    "de": "German",
    "it": "Italian",
    "pt": "Portuguese",
    "pt-BR": "Brazilian Portuguese",
    "nl": "Dutch",
    "sv": "Swedish",
    "pl": "Polish",
    "cs": "Czech",
    "uk": "Ukrainian",
    "ru": "Russian",
    "zh-CN": "Simplified Chinese",
    "zh-TW": "Traditional Chinese",
    "ja": "Japanese",
    "ko": "Korean",
    "id": "Indonesian",
    "hi": "Hindi",
    "ar": "Arabic",
    "tr": "Turkish",
    "vi": "Vietnamese",
    "th": "Thai",
    "fa": "Persian",
    "el": "Greek",
}

# Terms/product names the model must NOT translate.
DO_NOT_TRANSLATE = (
    "muxel, tmux, SSH, git, worktree, AppImage, PTY, Claude, opencode, Amp, gh, "
    "sshpass, GPL-3.0, ProxyJump, ServerAliveInterval, StrictHostKeyChecking, "
    "Ctrl, Shift, Alt, Tab, {{input}}, KEY=VALUE, and any file-path-like token "
    "(e.g. .muxel/MEMORY.md, ~/.config)"
)

REPO_ROOT = Path(__file__).resolve().parent.parent
CATALOG_DIR = REPO_ROOT / "crates" / "muxel" / "assets" / "i18n"

# `t("…")` / `tf("…", …)`: first string-literal arg. (tn handled separately.)
_T_RE = re.compile(r'\b(?:tf|t)\s*\(\s*"((?:[^"\\]|\\.)*)"', re.DOTALL)
# `tn("one", "other", …)`: the two leading string-literal args.
_TN_RE = re.compile(
    r'\btn\s*\(\s*"((?:[^"\\]|\\.)*)"\s*,\s*"((?:[^"\\]|\\.)*)"', re.DOTALL
)
_PLACEHOLDER_RE = re.compile(r"\{[^}]+\}")
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def _unescape(s: str) -> str:
    """Turn a Rust string-literal body into its actual text (\\\" -> ", \\n -> NL)."""
    out, i = [], 0
    simple = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "'": "'", "\\": "\\", "0": "\0"}
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            out.append(simple.get(s[i + 1], s[i + 1]))
            i += 2
        else:
            out.append(s[i])
            i += 1
    return "".join(out)


def extract_keys(repo_root: Path) -> list[str]:
    keys: set[str] = set()
    for rs in repo_root.glob("crates/**/*.rs"):
        if rs.name == "i18n.rs":  # the module's own doc examples aren't UI strings
            continue
        text = rs.read_text(encoding="utf-8", errors="replace")
        for m in _T_RE.finditer(text):
            keys.add(_unescape(m.group(1)))
        for m in _TN_RE.finditer(text):
            keys.add(_unescape(m.group(1)))
            keys.add(_unescape(m.group(2)))
    return sorted(keys)


def load_catalog(path: Path) -> dict[str, str]:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        sys.exit(f"error: {path} is not valid JSON: {e}")


def save_catalog(path: Path, catalog: dict[str, str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(catalog, ensure_ascii=False, indent=2, sort_keys=True)
    path.write_text(text + "\n", encoding="utf-8")


def batched(items: list, size: int):
    for i in range(0, len(items), size):
        yield items[i : i + size]


def build_prompt(keys: list[str], lang_name: str, lang_code: str) -> str:
    return f"""\
You are a professional software localizer. Translate these English UI strings for \
a terminal multiplexer app into {lang_name} ({lang_code}).

Rules — follow every one:
1. Return ONLY a JSON object mapping each English string to its translation. No \
prose, no markdown, no code fences.
2. Every input key must appear exactly once as a key in the output.
3. Keep every {{placeholder}} token (e.g. {{name}}, {{n}}, {{branch}}) EXACTLY as \
written. A curly-quoted token like “{{name}}” is a VARIABLE SLOT, not a word to \
translate — your output for that string MUST still contain “{{name}}” unchanged. \
Never translate, drop, or reorder a token.
4. Do NOT translate these terms/product names — keep them verbatim: {DO_NOT_TRANSLATE}.
5. Preserve leading/trailing whitespace, punctuation, and the "…" ellipsis.

Input (JSON array of English strings):
{json.dumps(keys, ensure_ascii=False, indent=2)}
"""


def call_backend(prompt: str, backend: str, model: str) -> str:
    if backend == "claude":
        cmd = ["claude", "-p", "--model", model, prompt]
        timeout = 120
    elif backend == "opencode":
        cmd = ["opencode", "run", prompt]
        timeout = 180
    else:
        raise ValueError(f"unknown backend {backend!r}")
    proc = subprocess.run(
        cmd, capture_output=True, text=True, encoding="utf-8", timeout=timeout
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"{backend} exited {proc.returncode}: {proc.stderr.strip()[:500]}"
        )
    return _ANSI_RE.sub("", proc.stdout)


def parse_response(raw: str) -> dict[str, str]:
    """Extract the first JSON object from an LLM response (handles fences/prose)."""
    text = raw.strip()
    candidates = [text]
    fenced = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.DOTALL)
    if fenced:
        candidates.append(fenced.group(1))
    start, end = text.find("{"), text.rfind("}")
    if start != -1 and end > start:
        candidates.append(text[start : end + 1])
    for c in candidates:
        try:
            obj = json.loads(c)
            if isinstance(obj, dict):
                return {k: v for k, v in obj.items() if isinstance(v, str)}
        except json.JSONDecodeError:
            continue
    return {}


def _dropped(key: str, val: str) -> set:
    """Placeholders present in the English key but missing from a translation."""
    want = set(_PLACEHOLDER_RE.findall(key))
    return want - set(_PLACEHOLDER_RE.findall(val)) if want else set()


def _norm(s: str) -> str:
    """Fold typographic quotes/apostrophes to ASCII, for tolerant key matching."""
    return s.replace("“", '"').replace("”", '"').replace("‘", "'").replace("’", "'")


def check_placeholders(keys: list[str], translations: dict[str, str]) -> set:
    """Warn about, and return, keys whose translation lost a {placeholder}."""
    bad = set()
    for key in keys:
        miss = _dropped(key, translations.get(key, ""))
        if miss:
            print(f"    warn: {key!r} dropped {sorted(miss)} — keeping English")
            bad.add(key)
    return bad


def translate_lang(code, name, keys, backend, model, batch_size, force, retries=2):
    path = CATALOG_DIR / f"{code}.json"
    catalog = load_catalog(path)
    # Drop any saved entry that lost a {placeholder} so it is re-translated (and, if
    # still broken, falls back to English — which keeps the placeholder).
    catalog = {k: v for k, v in catalog.items() if not _dropped(k, v)}
    todo = keys if force else [k for k in keys if k not in catalog]
    if not todo:
        print(f"  [{code}] up to date ({len(catalog)} keys)")
        return
    nbatches = (len(todo) + batch_size - 1) // batch_size
    print(f"  [{code}] {name}: {len(todo)} string(s) in {nbatches} batch(es)")
    for i, chunk in enumerate(batched(todo, batch_size), 1):
        result = {}
        for attempt in range(1, retries + 1):
            try:
                result = parse_response(call_backend(build_prompt(chunk, name, code), backend, model))
            except Exception as e:  # noqa: BLE001 - report and retry/skip
                print(f"    batch {i}/{nbatches} attempt {attempt} failed: {e}")
                result = {}
            if result:
                break
            print(f"    batch {i}/{nbatches} attempt {attempt}: empty/unparseable, retrying…")
        if not result:
            print(f"    batch {i}/{nbatches} skipped (will retry on next run)")
            continue
        # The model sometimes echoes a key with normalized typography (curly → straight
        # quotes/apostrophes), so resolve each source key by exact match then by a
        # typography-insensitive match before merging.
        norm = {_norm(rk): rv for rk, rv in result.items()}
        resolved = {k: (result[k] if k in result else norm.get(_norm(k))) for k in chunk}
        resolved = {k: v for k, v in resolved.items() if v is not None}
        bad = check_placeholders(chunk, resolved)
        for k in chunk:
            if k in resolved and k not in bad and (force or k not in catalog):
                catalog[k] = resolved[k]
        save_catalog(path, catalog)  # incremental save survives interruption
    print(f"  [{code}] saved {len(catalog)} keys -> {path.relative_to(REPO_ROOT)}")


def main() -> None:
    ap = argparse.ArgumentParser(description="Generate muxel i18n catalogs via an LLM CLI.")
    ap.add_argument("--backend", choices=["claude", "opencode"], default="claude")
    ap.add_argument("--model", default="sonnet", help="claude model alias (claude backend)")
    ap.add_argument("--lang", default="all", help='comma-separated codes, or "all"')
    ap.add_argument("--batch-size", type=int, default=25, dest="batch_size")
    ap.add_argument("--check", action="store_true", help="exit 1 if en.json is stale")
    ap.add_argument("--force", action="store_true", help="re-translate existing entries")
    ap.add_argument("--extract-only", action="store_true", dest="extract_only")
    args = ap.parse_args()

    keys = extract_keys(REPO_ROOT)
    en_path = CATALOG_DIR / "en.json"
    en_catalog = {k: k for k in keys}  # identity map = authoritative key list

    if args.check:
        if load_catalog(en_path) != en_catalog:
            print("en.json is out of sync with the source. Run: scripts/translate.py --extract-only")
            sys.exit(1)
        print(f"en.json is up to date ({len(keys)} keys).")
        return

    # Only rewrite en.json when it actually changed, so parallel `--lang` runs
    # don't race on it (the per-language catalogs they write are distinct files).
    if load_catalog(en_path) != en_catalog:
        save_catalog(en_path, en_catalog)
        print(f"extracted {len(keys)} string(s) -> {en_path.relative_to(REPO_ROOT)}")
    if args.extract_only:
        return

    if args.lang == "all":
        langs = SUPPORTED_LANGS
    else:
        langs = {}
        for code in args.lang.split(","):
            code = code.strip()
            if code not in SUPPORTED_LANGS:
                sys.exit(f"error: unknown language {code!r}; known: {', '.join(SUPPORTED_LANGS)}")
            langs[code] = SUPPORTED_LANGS[code]

    for code, name in langs.items():
        translate_lang(code, name, keys, args.backend, args.model, args.batch_size, args.force)


if __name__ == "__main__":
    main()
