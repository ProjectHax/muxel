//! Client-side sync bookkeeping and the pure push/pull decisions.
//!
//! A cycle is: pull `/changes` → [`decide_remote`] per item → apply → then
//! [`plan_push`] from the local settings. Pulling first means a fresh device
//! takes the synced appearance instead of pushing its defaults over it.
//!
//! Conflicts are last-writer-wins by HLC, with one rule for edits made between
//! cycles: a local change not yet pushed wins over a remote one that arrives in
//! the same cycle (it is the user's most recent action on this device and was
//! made on top of what they saw). It is pushed with a stamp newer than anything
//! seen, so every device converges on it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::hlc::Hlc;
use super::items::{Carry, Collection, LocalItem};

/// What this device last synced for one item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    /// Server version (for `base_version` on the next write).
    pub version: u64,
    /// Newest HLC applied or pushed; older remote stamps are rejected.
    pub hlc: Hlc,
    /// Content hash of the item as last synced (see [`content_hash`]).
    pub hash: u64,
    #[serde(default)]
    pub deleted: bool,
    /// Who signed the copy last synced. A team item whose author is no longer
    /// a writer there gets re-sealed by one who is.
    #[serde(default)]
    pub author: Option<Uuid>,
    /// Key generation that copy was sealed under (0 = not recorded).
    #[serde(default)]
    pub generation: u32,
}

/// One scope's bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeState {
    /// Keyed `"<collection>:<item uuid>"` (JSON object keys must be strings).
    #[serde(default)]
    pub items: HashMap<String, ItemState>,
    #[serde(default)]
    pub carry: HashMap<Uuid, Carry>,
}

pub fn item_key(c: Collection, id: Uuid) -> String {
    format!("{}:{}", c.as_str(), id)
}

impl ScopeState {
    pub fn get(&self, c: Collection, id: Uuid) -> Option<&ItemState> {
        self.items.get(&item_key(c, id))
    }

    pub fn set(&mut self, c: Collection, id: Uuid, s: ItemState) {
        self.items.insert(item_key(c, id), s);
    }

    /// Whether `id` has ever been synced in any settings collection.
    pub fn knows(&self, id: Uuid) -> bool {
        let suffix = format!(":{id}");
        self.items.keys().any(|k| k.ends_with(&suffix))
    }
}

/// Everything the device persists about sync (in the data dir, not config).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// `/changes` cursor.
    #[serde(default)]
    pub cursor: u64,
    /// Newest HLC this device has produced or observed.
    #[serde(default)]
    pub last_hlc: Hlc,
    /// Keyed by scope id.
    #[serde(default)]
    pub scopes: HashMap<Uuid, ScopeState>,
}

impl SyncState {
    /// A fresh stamp for a local write, after everything seen so far.
    pub fn stamp(&mut self, now_ms: u64, device: Uuid) -> Hlc {
        self.last_hlc = Hlc::tick(self.last_hlc, now_ms, device);
        self.last_hlc
    }

    /// Record a stamp seen from elsewhere so later local stamps sort after it.
    pub fn observe(&mut self, hlc: Hlc) {
        if hlc > self.last_hlc {
            self.last_hlc = hlc;
        }
    }

    pub fn scope(&mut self, id: Uuid) -> &mut ScopeState {
        self.scopes.entry(id).or_default()
    }
}

/// FNV-1a over the canonical JSON text: a cheap change detector, not a MAC.
pub fn content_hash(v: Option<&Value>) -> u64 {
    let text = match v {
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
        None => String::from("\u{0}deleted"),
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteDecision {
    /// Apply the remote item locally.
    Apply,
    /// Older than (or equal to) what this device already has: a replay or a
    /// server rollback. Ignore it.
    Stale,
    /// The local item changed since the last sync; keep it (it is pushed next).
    LocalWins,
}

/// Decide what to do with a remote item stamped `remote` given this device's
/// state for it and the item's current local content hash (`None` = absent).
pub fn decide_remote(
    state: Option<&ItemState>,
    local_hash: Option<u64>,
    remote: Hlc,
) -> RemoteDecision {
    let Some(st) = state else {
        return RemoteDecision::Apply;
    };
    if remote <= st.hlc {
        return RemoteDecision::Stale;
    }
    let local_dirty = match local_hash {
        Some(h) => st.deleted || h != st.hash,
        None => !st.deleted,
    };
    if local_dirty {
        RemoteDecision::LocalWins
    } else {
        RemoteDecision::Apply
    }
}

/// One write to send.
#[derive(Clone, Debug, PartialEq)]
pub struct Push {
    pub collection: Collection,
    pub id: Uuid,
    pub base_version: u64,
    /// `None` = delete.
    pub data: Option<Value>,
    pub hash: u64,
}

/// The writes needed to bring the server up to date with `local`, limited to
/// `collections` (those whose mode pushes). Singletons are never deleted.
pub fn plan_push(scope: &ScopeState, local: &[LocalItem], collections: &[Collection]) -> Vec<Push> {
    let mut out = Vec::new();
    for item in local.iter().filter(|i| collections.contains(&i.collection)) {
        let hash = content_hash(Some(&item.data));
        match scope.get(item.collection, item.id) {
            Some(st) if !st.deleted && st.hash == hash => {}
            st => out.push(Push {
                collection: item.collection,
                id: item.id,
                base_version: st.map_or(0, |s| s.version),
                data: Some(item.data.clone()),
                hash,
            }),
        }
    }
    for (key, st) in &scope.items {
        if st.deleted {
            continue;
        }
        let Some((c, id)) = key
            .split_once(':')
            .and_then(|(c, id)| Some((Collection::parse(c)?, Uuid::parse_str(id).ok()?)))
        else {
            continue;
        };
        if !collections.contains(&c)
            || matches!(
                c,
                Collection::Appearance | Collection::Keybindings | Collection::Tasks
            )
        {
            continue;
        }
        if !local.iter().any(|i| i.collection == c && i.id == id) {
            out.push(Push {
                collection: c,
                id,
                base_version: st.version,
                data: None,
                hash: content_hash(None),
            });
        }
    }
    out.sort_by_key(|a| (a.collection, a.id));
    out
}

#[cfg(test)]
mod tests {
    use super::{
        ItemState, Push, RemoteDecision, ScopeState, SyncState, content_hash, decide_remote,
        plan_push,
    };
    use crate::cloud::hlc::Hlc;
    use crate::cloud::items::{Collection, LocalItem};
    use serde_json::json;
    use uuid::Uuid;

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            counter: 0,
            device: Uuid::from_u128(1),
        }
    }

    fn item(id: u128, v: serde_json::Value) -> LocalItem {
        LocalItem {
            collection: Collection::Snippets,
            id: Uuid::from_u128(id),
            data: v,
        }
    }

    #[test]
    fn remote_decisions() {
        assert_eq!(decide_remote(None, None, hlc(1)), RemoteDecision::Apply);
        let st = ItemState {
            version: 1,
            hlc: hlc(10),
            hash: 5,
            deleted: false,
            ..Default::default()
        };
        assert_eq!(
            decide_remote(Some(&st), Some(5), hlc(9)),
            RemoteDecision::Stale
        );
        assert_eq!(
            decide_remote(Some(&st), Some(5), hlc(10)),
            RemoteDecision::Stale
        );
        assert_eq!(
            decide_remote(Some(&st), Some(5), hlc(11)),
            RemoteDecision::Apply
        );
        assert_eq!(
            decide_remote(Some(&st), Some(6), hlc(11)),
            RemoteDecision::LocalWins
        );
        assert_eq!(
            decide_remote(Some(&st), None, hlc(11)),
            RemoteDecision::LocalWins
        );
    }

    #[test]
    fn push_plan_covers_new_changed_deleted() {
        let mut scope = ScopeState::default();
        let same = json!({"name": "same"});
        scope.set(
            Collection::Snippets,
            Uuid::from_u128(1),
            ItemState {
                version: 3,
                hlc: hlc(1),
                hash: content_hash(Some(&same)),
                deleted: false,
                ..Default::default()
            },
        );
        scope.set(
            Collection::Snippets,
            Uuid::from_u128(2),
            ItemState {
                version: 4,
                hlc: hlc(1),
                hash: 0,
                deleted: false,
                ..Default::default()
            },
        );
        scope.set(
            Collection::Snippets,
            Uuid::from_u128(3),
            ItemState {
                version: 7,
                hlc: hlc(1),
                hash: 0,
                deleted: false,
                ..Default::default()
            },
        );
        let local = vec![
            item(1, same),
            item(2, json!({"name": "edited"})),
            item(4, json!({"name": "new"})),
        ];
        let plan = plan_push(&scope, &local, &[Collection::Snippets]);
        let summary: Vec<(u128, u64, bool)> = plan
            .iter()
            .map(|p: &Push| (p.id.as_u128(), p.base_version, p.data.is_some()))
            .collect();
        assert_eq!(summary, vec![(2, 4, true), (3, 7, false), (4, 0, true)]);
        assert!(plan_push(&scope, &local, &[Collection::Loops]).is_empty());
    }

    #[test]
    fn stamps_follow_observed() {
        let mut s = SyncState::default();
        let d = Uuid::from_u128(2);
        s.observe(hlc(1_000));
        let mine = s.stamp(10, d);
        assert!(mine > hlc(1_000));
    }

    #[test]
    fn state_roundtrips_as_json() {
        let mut s = SyncState {
            cursor: 9,
            ..Default::default()
        };
        s.scope(Uuid::from_u128(5)).set(
            Collection::Loops,
            Uuid::from_u128(6),
            ItemState {
                version: 1,
                hlc: hlc(2),
                hash: 3,
                deleted: false,
                ..Default::default()
            },
        );
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SyncState>(&text).unwrap(), s);
        assert!(s.scopes[&Uuid::from_u128(5)].knows(Uuid::from_u128(6)));
    }
}
