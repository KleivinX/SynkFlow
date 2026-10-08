//! Opt-in "start at login". Writes the per-user entry each OS expects; never
//! needs administrator rights and is removed again when switched off. The
//! launcher passes `--background` so only the tray item appears.

use std::path::Path;

use crate::config::APP_ID;

#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("could not write the login entry: {0}")]
    Io(#[from] std::io::Error),
    #[error("this platform has no supported login mechanism")]
    Unsupported,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn plist(exe: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>{APP_ID}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>--background</string>\n\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n"
    )
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn desktop_entry(exe: &Path) -> String {
    // The Exec value is quoted per the Desktop Entry spec.
    let exe = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "[Desktop Entry]\nType=Application\nName=Synkflow\nComment=Your desk. In sync.\nExec=\"{exe}\" --background\nIcon=synkflow\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn entry_path(home: &Path) -> std::path::PathBuf {
    home.join("Library/LaunchAgents").join(format!("{APP_ID}.plist"))
}

#[cfg(target_os = "linux")]
fn entry_path(home: &Path) -> std::path::PathBuf {
    home.join(".config/autostart/synkflow.desktop")
}

pub fn is_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("reg")
            .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "Synkflow"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        directories::BaseDirs::new().is_some_and(|d| entry_path(d.home_dir()).exists())
    }
}

pub fn set_enabled(on: bool) -> Result<(), AutostartError> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "windows")]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut cmd = std::process::Command::new("reg");
        if on {
            cmd.args(["add", key, "/v", "Synkflow", "/t", "REG_SZ", "/d", &format!("\"{}\" --background", exe.display()), "/f"]);
        } else {
            cmd.args(["delete", key, "/v", "Synkflow", "/f"]);
        }
        let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
        return if ok || !on { Ok(()) } else { Err(AutostartError::Io(std::io::Error::other("registry write failed"))) };
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).ok_or(AutostartError::Unsupported)?;
        write_entry(&home, &exe, on)
    }
}

#[cfg(not(target_os = "windows"))]
fn write_entry(home: &Path, exe: &Path, on: bool) -> Result<(), AutostartError> {
    let path = entry_path(home);
    if !on {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        };
    }
    if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
        return Err(AutostartError::Unsupported);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(target_os = "macos")]
    let body = plist(exe);
    #[cfg(not(target_os = "macos"))]
    let body = desktop_entry(exe);
    std::fs::write(path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_at_load_in_background_and_escapes_the_path() {
        let p = plist(Path::new("/Applications/Syn&flow <1>.app/Contents/MacOS/synkflow"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("--background"));
        assert!(p.contains("Syn&amp;flow &lt;1&gt;"));
        assert!(p.contains(APP_ID));
    }

    #[test]
    fn desktop_entry_quotes_the_command() {
        let d = desktop_entry(Path::new("/opt/My Apps/synkflow"));
        assert!(d.contains("Exec=\"/opt/My Apps/synkflow\" --background"));
        assert!(d.contains("Icon=synkflow"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn enabling_and_disabling_writes_and_removes_the_entry() {
        if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new("/tmp/synkflow");
        write_entry(home.path(), exe, true).unwrap();
        assert!(entry_path(home.path()).exists());
        write_entry(home.path(), exe, false).unwrap();
        assert!(!entry_path(home.path()).exists());
        write_entry(home.path(), exe, false).unwrap(); // idempotent
    }
}
