//! Windows-only parts: dialogs, the registry, the shortcut, and starting the app.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MessageBoxW};

use crate::install::{self, APP};
use crate::{FILES, VERSION};

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Synkflow";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn message(title: &str, text: &str, flags: u32) -> i32 {
    let (t, x) = (wide(title), wide(text));
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), x.as_ptr(), t.as_ptr(), flags) }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

fn set_str(key: HKEY, name: &str, value: &str) {
    let (n, v) = (wide(name), wide(value));
    // SAFETY: `key` is an open key; name/value are NUL-terminated UTF-16 and the byte length includes the NUL.
    unsafe {
        RegSetValueExW(key, n.as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32);
    }
}

fn set_dword(key: HKEY, name: &str, value: u32) {
    let n = wide(name);
    // SAFETY: as above; a DWORD is 4 bytes.
    unsafe {
        RegSetValueExW(key, n.as_ptr(), 0, REG_DWORD, &value as *const u32 as *const u8, 4);
    }
}

fn register(dir: &Path, size_kb: u32) -> bool {
    let mut key: HKEY = std::ptr::null_mut();
    let sub = wide(UNINSTALL_KEY);
    // SAFETY: creating/opening a key under HKCU; `key` receives the handle and is closed below.
    let rc = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut key, std::ptr::null_mut()) };
    if rc != 0 {
        return false;
    }
    set_str(key, "DisplayName", APP);
    set_str(key, "DisplayVersion", VERSION);
    set_str(key, "Publisher", "Synkflow contributors");
    set_str(key, "InstallLocation", &dir.display().to_string());
    set_str(key, "DisplayIcon", &dir.join("synkflow.ico").display().to_string());
    set_str(key, "UninstallString", &install::uninstall_command(dir));
    set_str(key, "QuietUninstallString", &format!("{} --silent", install::uninstall_command(dir)));
    set_dword(key, "NoModify", 1);
    set_dword(key, "NoRepair", 1);
    set_dword(key, "EstimatedSize", size_kb);
    // SAFETY: closing the handle opened above.
    unsafe {
        RegCloseKey(key);
    }
    true
}

fn unregister() {
    let sub = wide(UNINSTALL_KEY);
    // SAFETY: deleting our own key tree under HKCU.
    unsafe {
        RegDeleteTreeW(HKEY_CURRENT_USER, sub.as_ptr());
    }
}

fn shortcut_path() -> Option<PathBuf> {
    env_dir("APPDATA").map(|a| a.join(r"Microsoft\Windows\Start Menu\Programs").join("Synkflow.lnk"))
}

fn make_shortcut(dir: &Path) -> bool {
    let Some(lnk) = shortcut_path() else { return false };
    let Ok(mut sl) = mslnk::ShellLink::new(dir.join("synkflow.exe")) else { return false };
    sl.set_name(Some("Synkflow".into()));
    sl.set_icon_location(Some(dir.join("synkflow.ico").display().to_string()));
    sl.set_working_dir(Some(dir.display().to_string()));
    if let Some(parent) = lnk.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    sl.create_lnk(lnk).is_ok()
}

struct Args {
    silent: bool,
    uninstall: bool,
    dir: Option<PathBuf>,
}

fn parse() -> Args {
    let mut a = Args { silent: false, uninstall: false, dir: None };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.to_ascii_lowercase().as_str() {
            "/s" | "--silent" => a.silent = true,
            "--uninstall" | "/uninstall" => a.uninstall = true,
            "--dir" | "/dir" => a.dir = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    a
}

pub fn run() {
    let args = parse();
    let Some(local) = env_dir("LOCALAPPDATA") else {
        message("Synkflow setup", "Windows did not say where your user profile is, so nothing was installed.", MB_OK | MB_ICONERROR);
        std::process::exit(1);
    };
    let dir = args.dir.clone().unwrap_or_else(|| install::default_dir(&local));
    if let Err(e) = install::check_dir(&dir) {
        message("Synkflow setup", &format!("That folder cannot be used ({e:?}). Nothing was changed."), MB_OK | MB_ICONERROR);
        std::process::exit(1);
    }
    if args.uninstall {
        uninstall(&dir, args.silent);
    } else {
        install_app(&dir, args.silent);
    }
}

fn install_app(dir: &Path, silent: bool) {
    if !silent {
        let text = format!(
            "Install Synkflow {VERSION} for your Windows account?\n\n\
             • Installs to {}\n\
             • Adds a Start-menu shortcut and an Apps & features entry\n\
             • Needs no administrator rights, changes no firewall rules, and does not start with Windows\n\
             • Synkflow is free software (GPL-3.0-only); the license is installed with it\n\n\
             Windows may ask once whether Synkflow may use your private network: allow it there only.",
            dir.display()
        );
        if message("Synkflow setup", &text, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
    }
    let mut files: Vec<(&str, &[u8])> = FILES.to_vec();
    // The uninstaller is this very program.
    let me = std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok());
    if let Some(me) = me.as_deref() {
        files.push(("uninstall.exe", me));
    }
    let size_kb = (files.iter().map(|f| f.1.len()).sum::<usize>() / 1024) as u32;
    if let Err(e) = install::write_files(dir, &files) {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied { " If Synkflow is running, quit it (menu-bar icon → Quit Synkflow) and run setup again." } else { "" };
        if !silent {
            message("Synkflow setup", &format!("Installation stopped: {e}.{hint}\n\nNothing else was changed."), MB_OK | MB_ICONERROR);
        }
        std::process::exit(1);
    }
    let shortcut = make_shortcut(dir);
    let registered = register(dir, size_kb);
    if silent {
        return;
    }
    let mut msg = String::from("Synkflow is installed.\n\nOpen it from the Start menu.");
    if !shortcut {
        msg.push_str("\n\n(The Start-menu shortcut could not be created; run synkflow.exe from the install folder.)");
    }
    if !registered {
        msg.push_str("\n\n(It could not be listed under Apps & features; remove it by running uninstall.exe in the install folder.)");
    }
    msg.push_str("\n\nStart Synkflow now?");
    if message("Synkflow setup", &msg, MB_YESNO | MB_ICONINFORMATION) == IDYES {
        let _ = Command::new(dir.join("synkflow.exe")).spawn();
    }
}

fn uninstall(dir: &Path, silent: bool) {
    if !silent {
        let text = "Remove Synkflow from this computer?\n\nYour paired devices and settings are kept in your user profile; delete them from inside the app first (Settings → Privacy and security) if you want them gone.";
        if message("Synkflow setup", text, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
    }
    // The uninstaller cannot delete itself while running: keep it, then let a short shell command finish the job.
    match install::remove_files(dir, &["uninstall.exe"]) {
        Ok(()) => {}
        Err(e) => {
            if !silent {
                message("Synkflow setup", &format!("Could not remove everything: {e}. If Synkflow is running, quit it and try again."), MB_OK | MB_ICONERROR);
            }
            std::process::exit(1);
        }
    }
    if let Some(lnk) = shortcut_path() {
        let _ = std::fs::remove_file(lnk);
    }
    unregister();
    let me = dir.join("uninstall.exe");
    let cmd = format!("ping 127.0.0.1 -n 3 >nul & del /F /Q \"{}\" & rmdir \"{}\"", me.display(), dir.display());
    let _ = Command::new("cmd").args(["/C", &cmd]).creation_flags(CREATE_NO_WINDOW).spawn();
    if !silent {
        message("Synkflow setup", "Synkflow was removed.", MB_OK | MB_ICONINFORMATION);
    }
}
