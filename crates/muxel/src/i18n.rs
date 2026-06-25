//! UI localization (i18n).
//!
//! English-string-as-key: [`t`] looks up the active locale's translation of an
//! English source string and falls back to the source on any miss, so partial
//! catalogs degrade gracefully and the English literal doubles as the catalog
//! key. [`tf`]/[`tn`] handle interpolation and simple plurals. Catalogs are JSON
//! bundled under `assets/i18n/<lang>.json` (embedded like themes) and can be
//! switched at runtime. Regenerate them with `scripts/translate.py`.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use gpui::{Action, SharedString};
use muxel_core::Settings;
use rust_embed::RustEmbed;

/// The bundled translation catalogs (`<lang>.json`: English source → translation).
#[derive(RustEmbed)]
#[folder = "assets/i18n"]
struct I18nAssets;

struct Catalog {
    lang: String,
    entries: HashMap<String, String>,
}

static CATALOG: OnceLock<RwLock<Catalog>> = OnceLock::new();

/// Load + activate `lang`'s catalog, falling back to its base subtag
/// (`zh-CN` → `zh`) and then to an empty catalog (English source strings).
/// Called once at startup and again whenever the user switches language.
pub fn set_language(lang: &str) {
    let catalog = Catalog {
        lang: lang.to_string(),
        entries: load_catalog(lang).unwrap_or_default(),
    };
    match CATALOG.get() {
        Some(lock) => *lock.write().unwrap() = catalog,
        None => {
            let _ = CATALOG.set(RwLock::new(catalog));
        }
    }
}

fn load_catalog(lang: &str) -> Option<HashMap<String, String>> {
    let read = |tag: &str| -> Option<HashMap<String, String>> {
        let file = I18nAssets::get(&format!("{tag}.json"))?;
        let text = std::str::from_utf8(&file.data).ok()?;
        serde_json::from_str(text).ok()
    };
    read(lang).or_else(|| {
        let base = lang.split('-').next().unwrap_or(lang);
        (base != lang).then(|| read(base)).flatten()
    })
}

/// Translate a static English string (the source string is the lookup key).
/// Returns the English source unchanged when there is no translation, so the UI
/// stays readable for missing/partial catalogs.
pub fn t(en: &'static str) -> SharedString {
    if let Some(lock) = CATALOG.get()
        && let Ok(cat) = lock.read()
        && let Some(tr) = cat.entries.get(en)
    {
        return SharedString::from(tr.clone());
    }
    SharedString::from(en)
}

/// Translate an interpolated template, replacing each `{key}` token with its
/// value. The English template (with `{key}` tokens) is the catalog key, e.g.
/// `tf("Connected to {name}", &[("name", &name)])`.
pub fn tf(template: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = t(template).to_string();
    for (key, val) in args {
        s = s.replace(&format!("{{{key}}}"), val);
    }
    s
}

/// Singular/plural selection, then interpolation. v1 only distinguishes one vs.
/// other; languages with more CLDR plural forms get an approximation.
// TODO(i18n-v2): real per-locale plural rules (e.g. `icu_plurals`).
pub fn tn(one: &'static str, other: &'static str, n: usize, args: &[(&str, &str)]) -> String {
    tf(if n == 1 { one } else { other }, args)
}

/// `(BCP-47 tag, native display name)` for the language picker. Display names are
/// intentionally NOT run through `t()` — each reads in its own language.
pub fn available_languages() -> &'static [(&'static str, &'static str)] {
    &[
        ("en", "English"),
        ("es", "Español"),
        ("fr", "Français"),
        ("de", "Deutsch"),
        ("it", "Italiano"),
        ("pt", "Português"),
        ("pt-BR", "Português (Brasil)"),
        ("nl", "Nederlands"),
        ("sv", "Svenska"),
        ("pl", "Polski"),
        ("cs", "Čeština"),
        ("uk", "Українська"),
        ("ru", "Русский"),
        ("zh-CN", "简体中文"),
        ("zh-TW", "繁體中文"),
        ("ja", "日本語"),
        ("ko", "한국어"),
        ("id", "Bahasa Indonesia"),
        ("hi", "हिन्दी"),
        ("ar", "العربية"),
        ("tr", "Türkçe"),
        ("vi", "Tiếng Việt"),
        ("th", "ไทย"),
        ("fa", "فارسی"),
        ("el", "Ελληνικά"),
    ]
}

/// The active language's BCP-47 tag (defaults to `"en"`).
pub fn current_language() -> String {
    CATALOG
        .get()
        .and_then(|lock| lock.read().ok().map(|c| c.lang.clone()))
        .unwrap_or_else(|| "en".to_string())
}

/// Native display name for a tag (tries the base subtag, else the tag itself).
pub fn display_name(tag: &str) -> SharedString {
    let lookup = |want: &str| {
        available_languages()
            .iter()
            .find(|e| e.0 == want)
            .map(|e| e.1)
    };
    lookup(tag)
        .or_else(|| lookup(tag.split('-').next().unwrap_or(tag)))
        .map(SharedString::from)
        .unwrap_or_else(|| SharedString::from(tag.to_owned()))
}

/// Choose the startup language: explicit setting → OS locale → English.
pub fn detect_language(settings: &Settings) -> String {
    if let Some(lang) = &settings.language
        && !lang.is_empty()
    {
        return lang.clone();
    }
    sys_locale::get_locale().unwrap_or_else(|| "en".to_string())
}

/// Dispatched by the settings language picker; carries the chosen BCP-47 tag.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = muxel, no_json)]
pub struct SetLanguage(pub String);
