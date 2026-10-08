//! Synkflow — your desk, in sync. Desktop entry point.

// A GUI program: no console window when started from the Start menu.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use synkflow::config::{AppPaths, Config};
use synkflow::engine::{Engine, EngineDeps};
use synkflow::platform;

/// Logs go to `synkflow.log` in the settings folder (a GUI program has no console to read them from), capped at about
/// 1 MB across two files. Only Synkflow's own messages follow the chosen level; libraries stay at "warn".
fn init_logging(level: &str, paths: &AppPaths) {
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("SYNKFLOW_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("warn,synkflow={level}")));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_target(false);
    // Messages never carry clipboard content, file contents or full paths; they do name devices and network addresses.
    match open_log(&paths.dir) {
        Some(file) => builder.with_writer(std::sync::Mutex::new(file)).init(),
        None => builder.init(),
    }
}

fn open_log(dir: &std::path::Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join("synkflow.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        let _ = std::fs::rename(&path, dir.join("synkflow.log.1"));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).ok()
}

fn engine_deps(paths: AppPaths) -> EngineDeps {
    let stores = paths.secret_stores();
    EngineDeps {
        paths,
        input: platform::native(),
        clipboard: platform::clipboard::SystemClipboard::new().ok().map(|c| Box::new(c) as Box<dyn synkflow::platform::ClipboardBackend>),
        stores,
        bind: std::net::Ipv4Addr::UNSPECIFIED.into(),
        discovery: true,
        allow_loopback: false,
    }
}

/// The first line of every log: which build, on what, listening where. Without it a pasted log says nothing.
fn log_started(engine: &Engine) {
    let s = engine.snapshot();
    tracing::info!(
        "Synkflow {} started on {}: TCP {}, addresses {:?}, discovery {}, sharing {}",
        env!("CARGO_PKG_VERSION"),
        s.device.platform.label(),
        s.device.listen_port,
        s.network.addresses,
        if s.network.discovery_on { "on" } else { "off" },
        if s.sharing.paused { "paused" } else { "active" },
    );
}

fn fatal(msg: &str) -> ExitCode {
    eprintln!("Synkflow could not start: {msg}");
    #[cfg(feature = "gui")]
    {
        let _ = rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("Synkflow could not start").set_description(msg).show();
    }
    ExitCode::FAILURE
}

/// `--selftest`: start the real engine headless against a throw-away config
/// folder, report what this computer can do, and stop. Used to check an
/// installed build without touching real settings.
fn selftest(rt: &tokio::runtime::Runtime) -> ExitCode {
    let dir = std::env::temp_dir().join(format!("synkflow-selftest-{}", std::process::id()));
    let paths = AppPaths::at(&dir);
    let file = synkflow::identity::SecretStore::File { path: paths.identity_file() };
    let mut deps = engine_deps(paths);
    deps.stores = (file.clone(), file);
    deps.discovery = false;
    let result = rt.block_on(async {
        let engine = Engine::start(deps).await.map_err(|e| e.to_string())?;
        log_started(&engine);
        let s = engine.snapshot();
        println!("Synkflow {} self-test", env!("CARGO_PKG_VERSION"));
        println!("platform:      {}", s.device.platform.label());
        println!("input API:     {}", s.backend.api);
        println!("capture:       {}", s.backend.capture);
        println!("injection:     {}", s.backend.inject);
        println!("clipboard:     {}", if s.backend.clipboard_ok { "available" } else { "unavailable" });
        println!("listening on:  TCP {}", s.device.listen_port);
        println!("fingerprint:   {}", s.device.fingerprint.grouped());
        engine.shutdown().await;
        Ok::<_, String>(())
    });
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(()) => {
            println!("result:        OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("result:        FAILED ({e})");
            ExitCode::FAILURE
        }
    }
}

/// A GUI-subsystem process starts without standard handles, so `--version`,
/// `--help` and `--selftest` would print nowhere. When the handles are missing
/// and a parent terminal exists, borrow that terminal. (Not verified on a real
/// Windows machine; if it fails nothing is printed, the app itself is unaffected.)
#[cfg(windows)]
fn attach_parent_console() {
    use std::os::windows::io::IntoRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(id: u32) -> isize;
        fn SetStdHandle(id: u32, handle: isize) -> i32;
        fn AttachConsole(pid: u32) -> i32;
    }
    const STD_OUT: u32 = -11i32 as u32;
    const STD_ERR: u32 = -12i32 as u32;
    unsafe {
        let missing: Vec<u32> = [STD_OUT, STD_ERR].into_iter().filter(|&id| GetStdHandle(id) == 0).collect();
        if missing.is_empty() || AttachConsole(u32::MAX) == 0 {
            return;
        }
        for id in missing {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                SetStdHandle(id, f.into_raw_handle() as isize);
            }
        }
    }
}

fn main() -> ExitCode {
    #[cfg(windows)]
    attach_parent_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("Synkflow {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Synkflow {} — your desk, in sync.\n\nUsage: synkflow [--background] [--selftest] [--version]\n\n  --background  start with only the tray / menu-bar item\n  --selftest    start the engine headless, print capabilities, exit\n\nEnvironment: SYNKFLOW_CONFIG_DIR, SYNKFLOW_SECRET_STORE=file, SYNKFLOW_LOG",
            env!("CARGO_PKG_VERSION")
        );
        return ExitCode::SUCCESS;
    }

    let paths = AppPaths::discover();
    let level = Config::load(&paths.config_file()).0.diagnostics.log_level;
    init_logging(&level, &paths);

    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("synkflow-rt").enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return fatal(&e.to_string()),
    };
    if args.iter().any(|a| a == "--selftest") {
        return selftest(&rt);
    }

    #[cfg(feature = "gui")]
    {
        let engine = match rt.block_on(Engine::start(engine_deps(paths))) {
            Ok(e) => e,
            Err(e) => return fatal(&e.to_string()),
        };
        log_started(&engine);
        let background = args.iter().any(|a| a == "--background");
        let result = synkflow::ui::run(engine, rt.handle().clone(), synkflow::ui::Options { background });
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fatal(&format!("the window system failed: {e}")),
        }
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = paths;
        fatal("this build has no user interface; use --selftest")
    }
}
