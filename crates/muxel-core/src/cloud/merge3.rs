//! Three-way merge of the synced settings fields between windows.
//!
//! Each muxel window is its own process and saves its whole settings. When the
//! sync coordinator writes pulled changes to `config.toml`, another window's
//! in-memory copy is stale until it reloads — and if it saved in between, it
//! would write the pre-pull items back (and the coordinator would then push
//! that revert everywhere as a "local edit"). To prevent that, every window
//! remembers the synced fields as it last loaded or saved them (the *base*),
//! and before reloading or saving merges per item:
//!
//! - changed locally since the base → keep the local version;
//! - otherwise → take the disk version (including additions and deletions).
//!
//! The Cloud settings themselves (sync on/off, what to sync) ride along: they
//! never leave the computer, but every window must agree on them.

use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::CloudSettings;
use super::items::{Appearance, Keybindings};
use crate::{Identity, Loop, RemoteHost, Runner, Settings, Snippet};

/// The settings fields cloud sync writes, plus the Cloud settings.
#[derive(Clone, Debug)]
pub struct SyncedFields {
    pub cloud: CloudSettings,
    pub runners: Vec<Runner>,
    pub snippets: Vec<Snippet>,
    pub loops: Vec<Loop>,
    pub remotes: Vec<RemoteHost>,
    pub identities: Vec<Identity>,
    pub appearance: Appearance,
    pub keybindings: Keybindings,
}

impl SyncedFields {
    pub fn of(s: &Settings) -> Self {
        Self {
            cloud: s.cloud.clone(),
            runners: s.runners.clone(),
            snippets: s.snippets.clone(),
            loops: s.loops.clone(),
            remotes: s.remotes.clone(),
            identities: s.identities.clone(),
            appearance: Appearance::from_settings(s),
            keybindings: Keybindings {
                keybindings: s.keybindings.clone(),
                terminal_passthrough_keys: s.terminal_passthrough_keys.clone(),
            },
        }
    }

    pub fn write_to(&self, s: &mut Settings) {
        s.cloud = self.cloud.clone();
        s.runners = self.runners.clone();
        s.snippets = self.snippets.clone();
        s.loops = self.loops.clone();
        s.remotes = self.remotes.clone();
        s.identities = self.identities.clone();
        self.appearance.apply_to(s);
        s.keybindings = self.keybindings.keybindings.clone();
        s.terminal_passthrough_keys = self.keybindings.terminal_passthrough_keys.clone();
    }
}

fn same<T: Serialize>(a: &T, b: &T) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// Per-item merge of one list keyed by id. Order follows `disk`, with items
/// only the local side added appended in their local order.
fn merge_list<T: Clone + Serialize>(
    base: &[T],
    local: &[T],
    disk: &[T],
    id: impl Fn(&T) -> Uuid,
) -> Vec<T> {
    let find = |list: &[T], k: Uuid| list.iter().find(|x| id(x) == k).cloned();
    let mut out = Vec::new();
    for d in disk {
        let k = id(d);
        match (find(base, k), find(local, k)) {
            // Changed here since the base: ours wins.
            (Some(b), Some(l)) if !same(&b, &l) => out.push(l),
            // Deleted here since the base: stays deleted unless disk changed it.
            (Some(b), None) if same(&b, d) => {}
            _ => out.push(d.clone()),
        }
    }
    for l in local {
        let k = id(l);
        if out.iter().any(|x| id(x) == k) || disk.iter().any(|x| id(x) == k) {
            continue;
        }
        match find(base, k) {
            // Added here: keep.
            None => out.push(l.clone()),
            // Deleted on disk; keep only if we changed it since.
            Some(b) if !same(&b, l) => out.push(l.clone()),
            Some(_) => {}
        }
    }
    out
}

/// Field by field (recursing into objects): a field changed here since the
/// base keeps the local value, any other takes disk's.
fn merge_value<T: Clone + Serialize + DeserializeOwned>(base: &T, local: &T, disk: &T) -> T {
    fn json(
        base: &serde_json::Value,
        local: &serde_json::Value,
        disk: &serde_json::Value,
    ) -> serde_json::Value {
        use serde_json::Value::Object;
        match (base, local, disk) {
            (Object(b), Object(l), Object(d)) => {
                let mut out = d.clone();
                for (k, lv) in l {
                    let merged = match (b.get(k), d.get(k)) {
                        (Some(bv), Some(dv)) => json(bv, lv, dv),
                        // New here, or gone on disk: keep ours.
                        _ => lv.clone(),
                    };
                    out.insert(k.clone(), merged);
                }
                Object(out)
            }
            _ if base == local => disk.clone(),
            _ => local.clone(),
        }
    }
    let (Ok(b), Ok(l), Ok(d)) = (
        serde_json::to_value(base),
        serde_json::to_value(local),
        serde_json::to_value(disk),
    ) else {
        return local.clone();
    };
    serde_json::from_value(json(&b, &l, &d)).unwrap_or_else(|_| local.clone())
}

/// Merge `local` (this window) with `disk` (what's saved now), given the
/// `base` both started from.
pub fn merge3(base: &SyncedFields, local: &SyncedFields, disk: &SyncedFields) -> SyncedFields {
    SyncedFields {
        cloud: merge_value(&base.cloud, &local.cloud, &disk.cloud),
        runners: merge_list(&base.runners, &local.runners, &disk.runners, |x| x.id),
        snippets: merge_list(&base.snippets, &local.snippets, &disk.snippets, |x| x.id),
        loops: merge_list(&base.loops, &local.loops, &disk.loops, |x| x.id),
        remotes: merge_list(&base.remotes, &local.remotes, &disk.remotes, |x| x.id),
        identities: merge_list(&base.identities, &local.identities, &disk.identities, |x| {
            x.id
        }),
        appearance: merge_value(&base.appearance, &local.appearance, &disk.appearance),
        keybindings: merge_value(
            &base.keybindings.keybindings,
            &local.keybindings.keybindings,
            &disk.keybindings.keybindings,
        )
        .pipe(|keybindings| Keybindings {
            keybindings,
            terminal_passthrough_keys: merge_value(
                &base.keybindings.terminal_passthrough_keys,
                &local.keybindings.terminal_passthrough_keys,
                &disk.keybindings.terminal_passthrough_keys,
            ),
        }),
    }
}

/// One window's view of `config.toml` for the merge: the synced fields as it
/// last loaded or saved them.
#[derive(Clone, Debug, Default)]
pub struct WindowSync {
    base: Option<SyncedFields>,
}

impl WindowSync {
    /// The window loaded or saved `s`.
    pub fn saw(&mut self, s: &Settings) {
        self.base = Some(SyncedFields::of(s));
    }

    pub fn has_base(&self) -> bool {
        self.base.is_some()
    }

    /// Fold what's on `disk` into `live`, keeping this window's changes since
    /// it last saw the file. Returns `(changed, appearance_changed)`. Without
    /// a base (never loaded), `live` wins, as a plain save always did.
    pub fn merge_disk(&mut self, live: &mut Settings, disk: &Settings) -> (bool, bool) {
        let disk_fields = SyncedFields::of(disk);
        let Some(base) = self.base.take() else {
            self.base = Some(disk_fields);
            return (false, false);
        };
        let local = SyncedFields::of(live);
        let merged = merge3(&base, &local, &disk_fields);
        let changed = !same_fields(&local, &merged);
        let appearance = !same(&local.appearance, &merged.appearance);
        merged.write_to(live);
        self.base = Some(disk_fields);
        (changed, appearance)
    }
}

fn same_fields(a: &SyncedFields, b: &SyncedFields) -> bool {
    same(&a.cloud, &b.cloud)
        && same(&a.runners, &b.runners)
        && same(&a.snippets, &b.snippets)
        && same(&a.loops, &b.loops)
        && same(&a.remotes, &b.remotes)
        && same(&a.identities, &b.identities)
        && same(&a.appearance, &b.appearance)
        && same(&a.keybindings, &b.keybindings)
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::{SyncedFields, WindowSync, merge3};
    use crate::{Settings, Snippet};

    fn snip(name: &str, text: &str) -> Snippet {
        let mut s = Snippet::defaults().remove(0);
        s.id = uuid::Uuid::new_v4();
        s.name = name.into();
        s.text = text.into();
        s
    }

    #[test]
    fn pulled_changes_survive_a_stale_window_saving() {
        let base = Settings {
            snippets: vec![snip("a", "old")],
            ..Default::default()
        };
        // Disk got a pulled edit; this window only changed the theme.
        let mut disk = base.clone();
        disk.snippets[0].text = "pulled".into();
        let mut local = base.clone();
        local.theme = "Nord".into();
        let m = merge3(
            &SyncedFields::of(&base),
            &SyncedFields::of(&local),
            &SyncedFields::of(&disk),
        );
        assert_eq!(m.snippets[0].text, "pulled");
        assert_eq!(m.appearance.theme, "Nord");
    }

    #[test]
    fn local_edits_adds_and_deletes_win_over_untouched_disk() {
        let base = Settings {
            snippets: vec![snip("a", "1"), snip("b", "2"), snip("c", "3")],
            ..Default::default()
        };
        let mut local = base.clone();
        local.snippets[0].text = "edited here".into();
        local.snippets.remove(1);
        local.snippets.push(snip("d", "added here"));
        let mut disk = base.clone();
        disk.snippets.push(snip("e", "added there"));
        let m = merge3(
            &SyncedFields::of(&base),
            &SyncedFields::of(&local),
            &SyncedFields::of(&disk),
        );
        let names: Vec<_> = m.snippets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c", "e", "d"]);
        assert_eq!(m.snippets[0].text, "edited here");
    }

    #[test]
    fn deleted_on_disk_stays_deleted_unless_edited_here() {
        let base = Settings {
            snippets: vec![snip("a", "1"), snip("b", "2")],
            ..Default::default()
        };
        let mut disk = base.clone();
        disk.snippets.clear();
        let mut local = base.clone();
        local.snippets[1].text = "edited".into();
        let m = merge3(
            &SyncedFields::of(&base),
            &SyncedFields::of(&local),
            &SyncedFields::of(&disk),
        );
        let names: Vec<_> = m.snippets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b"]);
    }

    /// Two windows of one profile sharing `config.toml` (`disk`). A window
    /// saves by merging with disk, then writing; it reloads (on a stamp
    /// change) by merging without writing.
    struct Window {
        live: Settings,
        sync: WindowSync,
    }

    impl Window {
        fn open(disk: &Settings) -> Self {
            let mut sync = WindowSync::default();
            sync.saw(disk);
            Window {
                live: disk.clone(),
                sync,
            }
        }
        fn save(&mut self, disk: &mut Settings) {
            self.sync.merge_disk(&mut self.live, disk);
            *disk = self.live.clone();
            self.sync.saw(disk);
        }
        fn reload(&mut self, disk: &Settings) {
            self.sync.merge_disk(&mut self.live, disk);
        }
    }

    #[test]
    fn a_stale_window_never_reverts_what_another_saved() {
        let mut disk = Settings::default();
        let mut a = Window::open(&disk);
        let mut b = Window::open(&disk);
        // A signs in and its coordinator pulls a snippet and a theme.
        a.live.cloud.enabled = true;
        a.live.snippets.push(snip("pulled", "x"));
        a.live.theme = "Nord".into();
        a.save(&mut disk);
        // B missed the stamp and saves an unrelated edit.
        b.live.font_size += 1.0;
        b.save(&mut disk);
        assert!(disk.cloud.enabled, "B turned sync back off");
        assert!(disk.snippets.iter().any(|s| s.name == "pulled"));
        assert_eq!(disk.theme, "Nord");
        assert_eq!(disk.font_size, Settings::default().font_size + 1.0);
        // A reloads and sees B's change.
        a.reload(&disk);
        assert_eq!(a.live.font_size, disk.font_size);
    }

    #[test]
    fn pausing_in_one_window_reaches_the_coordinator() {
        let mut disk = Settings::default();
        disk.cloud.enabled = true;
        let mut coord = Window::open(&disk);
        let mut other = Window::open(&disk);
        other.live.cloud.enabled = false;
        other.save(&mut disk);
        coord.reload(&disk);
        assert!(!coord.live.cloud.enabled);
        // And the coordinator's next save doesn't switch it back on.
        coord.live.snippets.push(snip("new", "y"));
        coord.save(&mut disk);
        assert!(!disk.cloud.enabled);
    }
}
