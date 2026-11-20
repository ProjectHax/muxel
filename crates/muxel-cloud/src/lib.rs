//! muxel Cloud client: end-to-end encryption, the HTTP API client and the sync
//! engine. The protocol is specified in `docs/cloud-sync-protocol.md`.

pub mod account;
pub mod api;
pub mod crypto;
pub mod engine;
pub mod store;
