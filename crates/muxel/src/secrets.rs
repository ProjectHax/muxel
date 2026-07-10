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

// --- Shared login identities ---------------------------------------------------
// Identities keep their password in a distinct keychain namespace (`identity:{id}`)
// so a host's inline secret and an identity's shared secret never collide.

fn identity_entry(identity_id: Uuid) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("identity:{identity_id}")).context("open keychain entry")
}

/// Store (or replace) the SSH password for a login identity.
pub fn set_identity_password(identity_id: Uuid, password: &str) -> Result<()> {
    identity_entry(identity_id)?
        .set_password(password)
        .context("save password to keychain")
}

/// Fetch the stored SSH password for a login identity, if any.
pub fn get_identity_password(identity_id: Uuid) -> Option<String> {
    identity_entry(identity_id).ok()?.get_password().ok()
}

/// Whether a password is stored for this identity.
pub fn has_identity_password(identity_id: Uuid) -> bool {
    get_identity_password(identity_id).is_some()
}

/// Remove the stored password for a login identity (best-effort).
pub fn delete_identity_password(identity_id: Uuid) -> Result<()> {
    match identity_entry(identity_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete password from keychain"),
    }
}

// --- Speech-to-text provider API key -------------------------------------------
// A single shared key (there's one provider at a time), namespaced `stt:provider`.

fn stt_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, "stt:provider").context("open keychain entry")
}

/// Store (or replace) the speech-to-text provider API key.
pub fn set_stt_api_key(key: &str) -> Result<()> {
    stt_entry()?
        .set_password(key)
        .context("save API key to keychain")
}

/// Fetch the stored speech-to-text provider API key, if any.
pub fn get_stt_api_key() -> Option<String> {
    stt_entry().ok()?.get_password().ok()
}

/// Whether a provider API key is stored.
pub fn has_stt_api_key() -> bool {
    get_stt_api_key().is_some_and(|k| !k.is_empty())
}

/// Remove the stored provider API key (best-effort).
pub fn delete_stt_api_key() -> Result<()> {
    match stt_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete API key from keychain"),
    }
}
