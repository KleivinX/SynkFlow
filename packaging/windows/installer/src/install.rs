//! Platform-neutral installer logic (tested on every OS): where files go, how they are
//! written (atomically), and how they are removed again without ever touching anything
//! that is not ours.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const APP: &str = "Synkflow";

/// Every file the installer owns inside the install folder.
pub const OWNED: &[&str] = &["synkflow.exe", "synkflow.ico", "LICENSE.txt", "THIRD_PARTY_NOTICES.md", "THIRD_PARTY_LICENSES.txt", "TUTORIAL.txt", "uninstall.exe"];

pub fn default_dir(local_app_data: &Path) -> PathBuf {
    local_app_data.join("Programs").join(APP)
}

#[derive(Debug, PartialEq, Eq)]
pub enum DirError {
    NotAbsolute,
    TooShallow,
}

/// A custom install folder must be absolute and not a drive/filesystem root.
pub fn check_dir(dir: &Path) -> Result<(), DirError> {
    if !dir.is_absolute() {
        return Err(DirError::NotAbsolute);
    }
    if dir.components().count() < 3 {
        return Err(DirError::TooShallow);
    }
    Ok(())
}

/// Write each file to a temporary name and rename it into place, so an interrupted
/// install never leaves a half-written program.
pub fn write_files(dir: &Path, files: &[(&str, &[u8])]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, bytes) in files {
        debug_assert!(OWNED.contains(name));
        let tmp = dir.join(format!("{name}.part"));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, dir.join(name))?;
    }
    Ok(())
}

/// Remove only the files we own; remove the folder only if that leaves it empty, so a
/// custom folder that also holds the user's files is never deleted.
pub fn remove_files(dir: &Path, keep: &[&str]) -> io::Result<()> {
    for name in OWNED.iter().filter(|n| !keep.contains(n)) {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if keep.is_empty() {
        let _ = std::fs::remove_dir(dir); // fails (harmlessly) if not empty
    }
    Ok(())
}

pub fn uninstall_command(dir: &Path) -> String {
    format!("\"{}\" --uninstall", dir.join("uninstall.exe").display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Vec<(&'static str, &'static [u8])> {
        OWNED.iter().map(|n| (*n, b"data".as_slice())).collect()
    }

    #[test]
    fn installs_atomically_and_leaves_no_temporary_files() {
        let d = tempdir();
        let dir = d.join("Programs").join(APP);
        write_files(&dir, &payload()).unwrap();
        for n in OWNED {
            assert_eq!(std::fs::read(dir.join(n)).unwrap(), b"data");
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".part")).collect();
        assert!(leftovers.is_empty());
        // Re-running (upgrade/repair) is fine and replaces the files.
        write_files(&dir, &[("synkflow.exe", b"new".as_slice())]).unwrap();
        assert_eq!(std::fs::read(dir.join("synkflow.exe")).unwrap(), b"new");
    }

    #[test]
    fn uninstall_removes_only_our_files_and_the_folder_only_if_empty() {
        let d = tempdir();
        let dir = d.join("custom");
        write_files(&dir, &payload()).unwrap();
        std::fs::write(dir.join("my-notes.txt"), "keep me").unwrap();
        remove_files(&dir, &[]).unwrap();
        assert!(dir.join("my-notes.txt").exists(), "the user's own file must survive");
        for n in OWNED {
            assert!(!dir.join(n).exists());
        }
        assert!(dir.exists(), "a folder that still holds user files is not removed");
        std::fs::remove_file(dir.join("my-notes.txt")).unwrap();
        write_files(&dir, &payload()).unwrap();
        remove_files(&dir, &[]).unwrap();
        assert!(!dir.exists(), "an emptied folder is removed");
    }

    #[test]
    fn the_running_uninstaller_can_be_kept_for_later_deletion() {
        let d = tempdir();
        let dir = d.join("x");
        write_files(&dir, &payload()).unwrap();
        remove_files(&dir, &["uninstall.exe"]).unwrap();
        assert!(dir.join("uninstall.exe").exists());
        assert!(!dir.join("synkflow.exe").exists());
    }

    #[test]
    fn folders_are_validated() {
        assert_eq!(check_dir(Path::new("relative/path")), Err(DirError::NotAbsolute));
        #[cfg(unix)]
        {
            assert_eq!(check_dir(Path::new("/")), Err(DirError::TooShallow));
            assert_eq!(check_dir(Path::new("/tmp")), Err(DirError::TooShallow));
            assert!(check_dir(Path::new("/opt/apps/synkflow")).is_ok());
        }
        assert_eq!(default_dir(Path::new("/u/AppData/Local")), Path::new("/u/AppData/Local/Programs/Synkflow"));
        assert!(uninstall_command(Path::new("/a/b")).ends_with("uninstall.exe\" --uninstall"));
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("synkflow-setup-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
