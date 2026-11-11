//! End to end: the real `devsandbox` binary refuses to build from a signed
//! config's copy that isn't at its signed head, at each building command's
//! entry (`fleet::mesh::check_root`), with no daemon and no container
//! runtime: what the dispatcher's subprocesses and the CLI's in-process
//! fallback run.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

#[test]
fn building_verbs_refuse_an_unsigned_copy_in_process() {
    let base: PathBuf = std::env::temp_dir().join(format!("dsv-e2e-mesh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data = base.join("data");
    // A copy no install wrote and no log signed (an edited one is
    // `fleet::mesh`'s unit tests': the same function decides).
    let copy = data.join("devsandbox/mesh/tree/cfg");
    std::fs::create_dir_all(copy.join("sub")).unwrap();
    std::fs::write(copy.join("devsandboxes.toml"), "[sandbox.web]\nimage = \"alpine\"\nfolder = \".\"\n\n[services.db]\nimage = \"postgres\"\n").unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let cli = |dir: &PathBuf, args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_devsandbox"))
            .arg("--in-process")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("XDG_RUNTIME_DIR", &base)
            .env("XDG_STATE_HOME", base.join("state"))
            .env("XDG_DATA_HOME", &data)
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    for args in [&["run", "web"][..], &["rebuild", "web"], &["start", "web"], &["service", "rebuild", "db"]] {
        let (code, stderr) = cli(&copy, args);
        assert_eq!(code, Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains("config root `cfg` has no signed version here yet"), "{args:?}: {stderr}");
        let (code, stderr) = cli(&copy.join("sub"), args);
        assert_eq!(code, Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains("is inside the copy of config root `cfg`, not a signed config"), "{args:?}: {stderr}");
    }
    assert!(!data.join("devsandbox/state.toml").exists(), "refused before any work");
    let _ = std::fs::remove_dir_all(&base);
}
