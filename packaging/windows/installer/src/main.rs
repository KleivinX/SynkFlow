//! Synkflow setup for Windows.
//!
//! * Per-user: installs to `%LOCALAPPDATA%\Programs\Synkflow`. **No Administrator rights.**
//! * Creates a Start-menu shortcut and an "Apps & features" entry with an uninstaller.
//! * Never touches the firewall, never enables start-at-login, never contacts a network.
//! * `SynkflowSetup.exe /S` installs silently; `uninstall.exe --uninstall` removes everything it created.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod install;
#[cfg(windows)]
mod win;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub static FILES: &[(&str, &[u8])] = &[
    ("synkflow.exe", include_bytes!(concat!(env!("PAYLOAD_DIR"), "/synkflow.exe"))),
    ("synkflow.ico", include_bytes!(concat!(env!("PAYLOAD_DIR"), "/synkflow.ico"))),
    ("LICENSE.txt", include_bytes!(concat!(env!("PAYLOAD_DIR"), "/LICENSE.txt"))),
    ("THIRD_PARTY_NOTICES.md", include_bytes!(concat!(env!("PAYLOAD_DIR"), "/THIRD_PARTY_NOTICES.md"))),
    ("THIRD_PARTY_LICENSES.txt", include_bytes!(concat!(env!("PAYLOAD_DIR"), "/THIRD_PARTY_LICENSES.txt"))),
    ("TUTORIAL.txt", include_bytes!(concat!(env!("PAYLOAD_DIR"), "/TUTORIAL.txt"))),
];

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(not(windows))]
fn main() {
    // The installer is for Windows; on other systems it only exists so its logic can be unit-tested.
    eprintln!("Synkflow setup {VERSION} is a Windows installer.");
    std::process::exit(2);
}
