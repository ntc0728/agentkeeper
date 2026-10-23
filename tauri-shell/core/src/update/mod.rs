//! `core/update/*` — the software-update subsystem.
//!
//! Split from the module that performs IO so the pure parts (version
//! comparison, release picking, download hardening) can be tested without a
//! network or a filesystem. `version.rs` has no dependencies beyond the
//! standard library plus `serde_json`; `http.rs` owns the transport and
//! `manager.rs` the state machine that sits on top.
pub mod http;
pub mod manager;
pub mod token;
pub mod version;
