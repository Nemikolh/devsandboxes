//! `access.log`, next to `state.toml`: one JSON line per brokered call
//! (credential-broker, *Audit*), rotated by size to `access.log.1`. Never a
//! token or a phantom: a phantom shows as whose it was (`phantom`), the
//! route as its path before injection with the query dropped (a query slot
//! holds the phantom).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

pub const FILE: &str = "access.log";
/// Past this, the next line first moves the file to `access.log.1`.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Line {
    pub time: String,
    pub instance: String,
    pub instance_id: String,
    pub incarnation: Option<String>,
    pub kind: Option<String>,
    pub credential: Option<String>,
    pub method: String,
    pub route: String,
    pub scope: Option<String>,
    /// `allow`, `uninjected`, `refused`, `leak` or `error`.
    pub verdict: String,
    pub reason: Option<String>,
    pub status: u16,
    /// `own`, `other-claim`, `foreign`: whose phantom the call carried.
    pub phantom: Option<&'static str>,
}

pub struct Audit {
    path: PathBuf,
    max: u64,
    lock: Mutex<()>,
}

impl Audit {
    /// `<data>/access.log`.
    pub fn in_data(data: &Path) -> Audit {
        Audit::new(data.join(FILE), MAX_BYTES)
    }

    pub fn new(path: PathBuf, max: u64) -> Audit {
        Audit { path, max, lock: Mutex::new(()) }
    }

    /// Best effort: the call is answered whether or not its line lands.
    pub fn write(&self, line: &Line) {
        let _ = self.try_write(line);
    }

    fn try_write(&self, line: &Line) -> std::io::Result<()> {
        let mut text = serde_json::to_vec(line).map_err(std::io::Error::other)?;
        text.push(b'\n');
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + text.len() as u64 > self.max {
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            fs::rename(&self.path, rotated)?;
        }
        OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path)?.write_all(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_append_and_rotate_by_size() {
        let dir = std::env::temp_dir().join(format!("dsv-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let audit = Audit::new(dir.join(FILE), 700);
        let line = Line {
            time: "2026-11-21T00:00:00Z".into(),
            instance: "web-1".into(),
            method: "GET".into(),
            route: "/api/0/".into(),
            verdict: "allow".into(),
            status: 200,
            phantom: Some("own"),
            ..Line::default()
        };
        audit.write(&line);
        audit.write(&line);
        let text = fs::read_to_string(dir.join(FILE)).unwrap();
        assert_eq!(text.lines().count(), 2);
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!((v["instance"].as_str(), v["status"].as_u64(), v["phantom"].as_str()), (Some("web-1"), Some(200), Some("own")));
        assert!(v["credential"].is_null());
        for _ in 0..3 {
            audit.write(&line);
        }
        assert!(dir.join("access.log.1").exists());
        assert!(fs::metadata(dir.join(FILE)).unwrap().len() <= 700);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }
}
