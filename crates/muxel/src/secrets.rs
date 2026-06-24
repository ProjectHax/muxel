//! Secure storage of remote-host passwords in the OS keychain (Secret Service on
//! Linux, Keychain on macOS, Credential Manager on Windows) via the `keyring`
//! crate. Passwords are never written to muxel's config or workspace files —
//! only a reference (the host id) is, and the secret is fetched on demand.

use anyhow::{Context, Result};
use uuid::Uuid;

const SERVICE: &str = "muxel";

fn entry(host_id: Uuid) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("remote:{host_id}")).context("open keychain entry")
}

/// Store (or replace) the SSH password for a remote host.
pub fn set_remote_password(host_id: Uuid, password: &str) -> Result<()> {
    entry(host_id)?
        .set_password(password)
        .context("save password to keychain")
}

/// Fetch the stored SSH password for a remote host, if any.
pub fn get_remote_password(host_id: Uuid) -> Option<String> {
    entry(host_id).ok()?.get_password().ok()
}

/// Whether a password is stored for this host.
pub fn has_remote_password(host_id: Uuid) -> bool {
    get_remote_password(host_id).is_some()
}

/// Remove the stored password for a remote host (best-effort; a missing entry is
/// not an error).
pub fn delete_remote_password(host_id: Uuid) -> Result<()> {
    match entry(host_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete password from keychain"),
    }
}
