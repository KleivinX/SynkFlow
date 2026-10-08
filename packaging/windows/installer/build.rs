// The installer embeds the application files. `SYNKFLOW_PAYLOAD_DIR` points at them;
// without it (unit tests, `cargo check`) tiny stand-in files are generated so the crate still builds.
use std::path::PathBuf;

const FILES: &[&str] = &["synkflow.exe", "synkflow.ico", "LICENSE.txt", "THIRD_PARTY_NOTICES.md", "THIRD_PARTY_LICENSES.txt", "TUTORIAL.txt"];

fn main() {
    println!("cargo:rerun-if-env-changed=SYNKFLOW_PAYLOAD_DIR");
    let dir = match std::env::var_os("SYNKFLOW_PAYLOAD_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let d = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("stand-in-payload");
            std::fs::create_dir_all(&d).unwrap();
            for f in FILES {
                std::fs::write(d.join(f), format!("stand-in for {f}")).unwrap();
            }
            d
        }
    };
    for f in FILES {
        let p = dir.join(f);
        assert!(p.is_file(), "payload file missing: {}", p.display());
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rustc-env=PAYLOAD_DIR={}", dir.display());
}
