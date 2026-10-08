//! Synkflow engine: identity, pinned TLS, protocol, input routing, clipboard
//! and file transfer. The Slint UI in `main.rs` is a thin client of this crate.

pub mod autostart;
pub mod clipboard;
pub mod config;
pub mod control;
pub mod discovery;
pub mod engine;
pub mod format;
pub mod geometry;
pub mod identity;
pub mod keys;
pub mod layout_editor;
pub mod limits;
pub mod platform;
pub mod proto;
pub mod session;
pub mod tls;
pub mod transfer;
#[cfg(feature = "gui")]
pub mod ui;
pub mod view;
