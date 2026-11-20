//! Team permission atoms and built-in roles, mirroring the server (protocol §5)
//! so the UI can hide actions a member isn't allowed to take. The server is
//! authoritative; this only shapes the interface.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    #[serde(rename = "billing")]
    Billing,
    #[serde(rename = "members.manage")]
    MembersManage,
    #[serde(rename = "roles.assign")]
    RolesAssign,
    #[serde(rename = "settings.read")]
    SettingsRead,
    #[serde(rename = "settings.write")]
    SettingsWrite,
    #[serde(rename = "tasks.read_all")]
    TasksReadAll,
    #[serde(rename = "tasks.read_assigned")]
    TasksReadAssigned,
    #[serde(rename = "tasks.create")]
    TasksCreate,
    #[serde(rename = "tasks.assign")]
    TasksAssign,
    #[serde(rename = "tasks.take")]
    TasksTake,
    #[serde(rename = "tasks.update")]
    TasksUpdate,
    /// Permanently delete a team task (closing one is `tasks.update`).
    #[serde(rename = "tasks.delete")]
    TasksDelete,
    #[serde(rename = "activity.read")]
    ActivityRead,
    #[serde(rename = "activity.publish")]
    ActivityPublish,
}

impl Permission {
    pub const ALL: [Permission; 14] = [
        Permission::Billing,
        Permission::MembersManage,
        Permission::RolesAssign,
        Permission::SettingsRead,
        Permission::SettingsWrite,
        Permission::TasksReadAll,
        Permission::TasksReadAssigned,
        Permission::TasksCreate,
        Permission::TasksAssign,
        Permission::TasksTake,
        Permission::TasksUpdate,
        Permission::TasksDelete,
        Permission::ActivityRead,
        Permission::ActivityPublish,
    ];

    /// Parse the wire name; unknown atoms (from a newer server) are skipped.
    pub fn from_wire(s: &str) -> Option<Permission> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }
}

/// The built-in roles. Teams may later define custom roles; those arrive as a
/// role name plus an explicit permission list.
pub const BUILTIN_ROLES: [&str; 8] = [
    "owner",
    "admin",
    "manager",
    "member",
    "assignee",
    "settings_only",
    "observer",
    "billing",
];

/// Permissions of a built-in role (`None` for an unknown or custom role).
pub fn builtin_permissions(role: &str) -> Option<Vec<Permission>> {
    use Permission::*;
    let all = Permission::ALL.to_vec();
    Some(match role {
        "owner" => all,
        "admin" => all.into_iter().filter(|p| *p != Billing).collect(),
        "manager" => vec![
            SettingsRead,
            SettingsWrite,
            TasksReadAll,
            TasksReadAssigned,
            TasksCreate,
            TasksAssign,
            TasksTake,
            TasksUpdate,
            TasksDelete,
            ActivityRead,
            ActivityPublish,
        ],
        "member" => vec![
            SettingsRead,
            TasksReadAll,
            TasksCreate,
            TasksTake,
            TasksUpdate,
            ActivityRead,
            ActivityPublish,
        ],
        "assignee" => vec![TasksReadAssigned, TasksUpdate],
        "settings_only" => vec![SettingsRead],
        "observer" => vec![TasksReadAll, ActivityRead],
        "billing" => vec![Billing],
        _ => return None,
    })
}

/// A member's effective permissions as the client knows them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants(Vec<Permission>);

impl Grants {
    pub fn new(perms: impl IntoIterator<Item = Permission>) -> Self {
        let mut v: Vec<Permission> = perms.into_iter().collect();
        v.dedup();
        Grants(v)
    }

    /// From the server's permission strings (authoritative), falling back to
    /// the built-in table for `role` when the list is empty.
    pub fn from_wire(role: &str, perms: &[String]) -> Self {
        if perms.is_empty() {
            return Grants::new(builtin_permissions(role).unwrap_or_default());
        }
        Grants::new(perms.iter().filter_map(|p| Permission::from_wire(p)))
    }

    pub fn has(&self, p: Permission) -> bool {
        self.0.contains(&p)
    }

    pub fn can_see_task(&self, assignee_is_me: bool) -> bool {
        self.has(Permission::TasksReadAll)
            || (assignee_is_me && self.has(Permission::TasksReadAssigned))
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILTIN_ROLES, Grants, Permission, builtin_permissions};

    #[test]
    fn every_builtin_role_resolves_and_owner_has_all() {
        for r in BUILTIN_ROLES {
            assert!(builtin_permissions(r).is_some(), "{r}");
        }
        assert_eq!(
            builtin_permissions("owner").unwrap().len(),
            Permission::ALL.len()
        );
        assert!(
            !builtin_permissions("admin")
                .unwrap()
                .contains(&Permission::Billing)
        );
        assert_eq!(builtin_permissions("custom"), None);
    }

    #[test]
    fn wire_names_roundtrip() {
        for p in Permission::ALL {
            let s = serde_json::to_value(p).unwrap();
            assert_eq!(Permission::from_wire(s.as_str().unwrap()), Some(p));
        }
        assert_eq!(Permission::from_wire("future.thing"), None);
    }

    #[test]
    fn assignee_sees_only_own_tasks() {
        let g = Grants::from_wire("assignee", &[]);
        assert!(g.can_see_task(true));
        assert!(!g.can_see_task(false));
        let g = Grants::from_wire("observer", &[]);
        assert!(g.can_see_task(false));
        let g = Grants::from_wire("whatever", &["tasks.read_all".into()]);
        assert!(g.can_see_task(false));
    }
}
