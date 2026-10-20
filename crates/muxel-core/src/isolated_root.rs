//! Pure control-endpoint policy for an isolated muxel profile.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxEnvironmentError {
    SessionEnvironmentUnsupported,
    NonUnicodeRoot,
}

/// A local tmux server may predate this profile. Pass the effective root and its
/// endpoint per session, never through the shared server's global environment.
/// `root` is the absolute startup-resolved override; empty means no override.
pub fn local_tmux_environment(
    root: Option<&Path>,
    instance_id: uuid::Uuid,
    session_env: bool,
) -> Result<Vec<(String, String)>, TmuxEnvironmentError> {
    let root = root.filter(|root| !root.as_os_str().is_empty());
    if root.is_some() && !session_env {
        return Err(TmuxEnvironmentError::SessionEnvironmentUnsupported);
    }
    let mut env = Vec::new();
    if session_env {
        env.push((
            crate::control::INSTANCE_ID_ENV.into(),
            instance_id.to_string(),
        ));
    }
    if let Some(root) = root {
        let endpoint = root.join("data").join(crate::control::ENDPOINT_FILE);
        let root = root.to_str().ok_or(TmuxEnvironmentError::NonUnicodeRoot)?;
        let endpoint = endpoint
            .to_str()
            .ok_or(TmuxEnvironmentError::NonUnicodeRoot)?;
        env.push(("MUXEL_USER_DATA_DIR".into(), root.into()));
        env.push(("MUXEL_CONTROL".into(), endpoint.into()));
    }
    Ok(env)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointConflict {
    pub expected: PathBuf,
    pub supplied: PathBuf,
}

impl std::fmt::Display for EndpointConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "isolated_endpoint_conflict: MUXEL_CONTROL '{}' conflicts with isolated endpoint '{}'",
            self.supplied.display(),
            self.expected.display()
        )
    }
}

impl std::error::Error for EndpointConflict {}

/// Paths must already be resolved by the caller. An isolated profile never uses
/// an endpoint outside that profile, including when its data directory is absent.
pub fn select_control_endpoint(
    default_endpoint: Option<PathBuf>,
    isolated: bool,
    override_endpoint: Option<PathBuf>,
) -> Result<Option<PathBuf>, EndpointConflict> {
    match (isolated, default_endpoint, override_endpoint) {
        (true, Some(expected), Some(supplied)) if expected != supplied => {
            Err(EndpointConflict { expected, supplied })
        }
        (true, endpoint, _) => Ok(endpoint),
        (false, _, Some(endpoint)) | (false, Some(endpoint), None) => Ok(Some(endpoint)),
        (false, None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{TmuxEnvironmentError, local_tmux_environment, select_control_endpoint};
    use std::path::PathBuf;

    #[test]
    fn isolated_tmux_requires_per_session_environment_even_for_unknown_versions() {
        let id = uuid::Uuid::new_v4();
        for version in ["tmux 3.1c", "tmux 2.9", "", "unknown"] {
            assert_eq!(
                local_tmux_environment(
                    Some(std::path::Path::new("/isolated")),
                    id,
                    crate::tmux::session_env_supported(version)
                ),
                Err(TmuxEnvironmentError::SessionEnvironmentUnsupported),
                "{version}"
            );
        }
        assert_eq!(local_tmux_environment(None, id, false), Ok(vec![]));
        assert_eq!(
            local_tmux_environment(Some(std::path::Path::new("")), id, false),
            Ok(vec![])
        );
    }

    #[test]
    fn isolated_tmux_gets_its_root_endpoint_and_pane_identity() {
        let id = uuid::Uuid::new_v4();
        let root = PathBuf::from("/isolated root/quote'and;equals=dir");
        for version in ["tmux 3.2", "tmux 3.2a", "tmux 3.10", "tmux master"] {
            let env = local_tmux_environment(
                Some(&root),
                id,
                crate::tmux::session_env_supported(version),
            )
            .unwrap();
            assert_eq!(
                env,
                vec![
                    (crate::control::INSTANCE_ID_ENV.into(), id.to_string()),
                    ("MUXEL_USER_DATA_DIR".into(), root.to_str().unwrap().into()),
                    (
                        "MUXEL_CONTROL".into(),
                        root.join("data")
                            .join(crate::control::ENDPOINT_FILE)
                            .to_str()
                            .unwrap()
                            .into()
                    ),
                ]
            );
        }
        assert_eq!(
            local_tmux_environment(None, id, true).unwrap(),
            vec![(crate::control::INSTANCE_ID_ENV.into(), id.to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn tmux_never_changes_a_non_unicode_root_by_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt as _;
        let root = PathBuf::from(std::ffi::OsString::from_vec(b"/root\xff".to_vec()));
        assert_eq!(
            local_tmux_environment(Some(&root), uuid::Uuid::new_v4(), true),
            Err(TmuxEnvironmentError::NonUnicodeRoot)
        );
    }

    #[test]
    fn isolated_root_refuses_a_foreign_control_endpoint() {
        let expected = PathBuf::from("root/data/control.json");
        let supplied = PathBuf::from("foreign/control.json");
        let error = select_control_endpoint(Some(expected.clone()), true, Some(supplied.clone()))
            .unwrap_err();
        assert_eq!(error.expected, expected);
        assert_eq!(error.supplied, supplied);
        let message = error.to_string();
        assert!(message.contains("isolated_endpoint_conflict"));
        assert!(message.contains("root/data/control.json"));
        assert!(message.contains("foreign/control.json"));
    }

    #[test]
    fn isolated_root_accepts_its_own_endpoint_override() {
        let endpoint = PathBuf::from("root/data/control.json");
        assert_eq!(
            select_control_endpoint(Some(endpoint.clone()), true, Some(endpoint.clone())),
            Ok(Some(endpoint))
        );
    }

    #[test]
    fn isolated_root_without_override_uses_its_endpoint() {
        let endpoint = PathBuf::from("root/data/control.json");
        assert_eq!(
            select_control_endpoint(Some(endpoint.clone()), true, None),
            Ok(Some(endpoint))
        );
    }

    #[test]
    fn isolated_root_without_data_never_uses_an_override() {
        assert_eq!(
            select_control_endpoint(None, true, Some("foreign/control.json".into())),
            Ok(None)
        );
    }

    #[test]
    fn normal_profile_preserves_the_control_override() {
        let endpoint = PathBuf::from("foreign/control.json");
        assert_eq!(
            select_control_endpoint(
                Some("default/control.json".into()),
                false,
                Some(endpoint.clone())
            ),
            Ok(Some(endpoint))
        );
        assert_eq!(select_control_endpoint(None, false, None), Ok(None));
    }
}
