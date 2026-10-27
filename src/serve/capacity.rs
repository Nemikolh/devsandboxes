//! A host's capacity report (docs/architecture/daemon/fleet.md, *Capacity*;
//! fleet plan decision 8): cpus and memory from the runtime's `info`
//! (cached [`ENGINE_TTL`]), free memory and disk only for a native Linux
//! engine, the running instance count, and `lend.toml`'s state and soft
//! caps. The host poll (`host.rs`) builds one per watched tick and the
//! snapshots carry it (`Snapshots::set_capacity`); a machine's comes from
//! its pushed snapshot (`snapshots::SnapshotMachine::capacity`). On demand,
//! `capacity.get` and a placed `run` build one through [`HostCapacity`],
//! the same engine cache and running count the poll uses, watched or not.
//!
//! Everything but [`HostFacts::read`] and the injected `info` call is pure:
//! the parsers, the cache over [`Instant`]s, [`over_caps`] (what placement
//! will score against) and the dashboard's and `machine ls`' text.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::fleet::lend::{Lend, SoftCaps, State};
use crate::runtime::identity::EngineInfo;

/// How long one `info` answer is used: cpus and total memory rarely move.
pub const ENGINE_TTL: Duration = Duration::from_secs(60);

/// `lend.toml`'s `[caps]` in bytes and counts (the wire's).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    pub cpus: Option<u64>,
    /// Bytes.
    pub mem: Option<u64>,
    /// Bytes.
    pub disk: Option<u64>,
    pub instances: Option<u64>,
}

impl Caps {
    /// `None` when no cap is set. A size that doesn't parse is unset
    /// (`lend::check` refuses one before it's written).
    pub fn of(soft: &SoftCaps) -> Option<Caps> {
        let size = |s: &Option<String>| s.as_deref().and_then(|s| parse_size(s).ok());
        let caps = Caps { cpus: soft.cpus, mem: size(&soft.mem), disk: size(&soft.disk), instances: soft.instances };
        (caps != Caps::default()).then_some(caps)
    }
}

/// One host's capacity, as a snapshot carries it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// The engine host's (a VM's, for a VM-backed engine); `None` unknown.
    pub cpus: Option<u64>,
    /// Bytes, the engine host's.
    pub mem_total: Option<u64>,
    /// Bytes (`/proc/meminfo` `MemAvailable`): native Linux engines only.
    pub mem_available: Option<u64>,
    /// Bytes free to an unprivileged writer on the filesystem of the
    /// engine's data root (`statvfs`): native Linux engines only.
    pub disk_free: Option<u64>,
    /// Bytes, that filesystem's size.
    pub disk_total: Option<u64>,
    /// Instances in `state.toml` whose container runs.
    pub running: usize,
    /// `lend.toml`'s `state`; `None` without a `lend.toml`.
    pub lending: Option<State>,
    /// `lend.toml`'s soft caps; `None` when none is set.
    pub caps: Option<Caps>,
}

/// What this host says about itself beyond the runtime's `info`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostFacts {
    pub mem_available: Option<u64>,
    pub disk_free: Option<u64>,
    pub disk_total: Option<u64>,
}

impl HostFacts {
    /// Read for a native Linux engine only (`engine.native` on Linux):
    /// anywhere else this host's memory and disk aren't the engine's. The
    /// disk is the filesystem of `root_dir`, when it exists here.
    pub fn read(engine: Option<&EngineInfo>) -> HostFacts {
        let Some(engine) = engine.filter(|e| e.native && cfg!(target_os = "linux")) else {
            return HostFacts::default();
        };
        let mem_available = std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| parse_meminfo(&t));
        let (disk_free, disk_total) = engine.root_dir.as_deref().and_then(statvfs).map_or((None, None), |(f, t)| (Some(f), Some(t)));
        HostFacts { mem_available, disk_free, disk_total }
    }
}

impl Capacity {
    /// The report from its parts (pure).
    pub fn build(engine: Option<&EngineInfo>, host: HostFacts, running: usize, lend: Option<&Lend>) -> Capacity {
        Capacity {
            cpus: engine.and_then(|e| e.cpus),
            mem_total: engine.and_then(|e| e.mem_total),
            mem_available: host.mem_available,
            disk_free: host.disk_free,
            disk_total: host.disk_total,
            running,
            lending: lend.map(|l| l.state),
            caps: lend.and_then(|l| Caps::of(&l.caps)),
        }
    }

    /// This report with `lend`'s state and caps instead of its own (pure).
    pub fn with_lend(self, lend: Option<&Lend>) -> Capacity {
        Capacity { lending: lend.map(|l| l.state), caps: lend.and_then(|l| Caps::of(&l.caps)), ..self }
    }
}

/// How the runtime's `info` is asked (`Backend::engine_info` outside tests).
pub type EngineInfoFn = Arc<dyn Fn() -> Result<EngineInfo> + Send + Sync>;

/// This host's report, on demand: one [`EngineCache`] and the running
/// count, shared by the poll (which sets the count from each listing and
/// asks for the report while a root is watched) and the daemon
/// (`capacity.get`, a placed `run`), so asking needs no watched root.
pub struct HostCapacity {
    engine: Mutex<EngineCache>,
    running: AtomicUsize,
    info: EngineInfoFn,
}

impl HostCapacity {
    pub fn new(info: EngineInfoFn) -> HostCapacity {
        HostCapacity { engine: Mutex::default(), running: AtomicUsize::new(0), info }
    }

    /// The poll's latest running count (`host::running_instances`).
    pub fn set_running(&self, running: usize) {
        self.running.store(running, Ordering::Release);
    }

    /// The report now: the cached `info` (asked again past [`ENGINE_TTL`]),
    /// [`HostFacts::read`], the last running count, `lend`.
    pub fn report(&self, lend: Option<&Lend>) -> Capacity {
        let info = self.engine.lock().unwrap_or_else(|e| e.into_inner()).get(Instant::now(), || (self.info)()).cloned();
        Capacity::build(info.as_ref(), HostFacts::read(info.as_ref()), self.running.load(Ordering::Acquire), lend)
    }
}

/// The runtime's last `info`, asked again every [`ENGINE_TTL`].
#[derive(Debug, Default)]
pub struct EngineCache {
    last: Option<(Instant, Option<EngineInfo>)>,
}

impl EngineCache {
    /// The cached answer, `fetch`ed first when there's none or it's
    /// [`ENGINE_TTL`] old. A failed fetch keeps the last answer and waits
    /// out the same interval (a runtime that's down isn't asked every tick).
    pub fn get(&mut self, now: Instant, fetch: impl FnOnce() -> Result<EngineInfo>) -> Option<&EngineInfo> {
        let due = self.last.as_ref().is_none_or(|(at, _)| now >= *at + ENGINE_TTL);
        if due {
            let prev = self.last.take().and_then(|(_, info)| info);
            self.last = Some((now, fetch().ok().or(prev)));
        }
        self.last.as_ref().and_then(|(_, info)| info.as_ref())
    }
}

/// `MemAvailable` of `/proc/meminfo`, in bytes.
pub fn parse_meminfo(text: &str) -> Option<u64> {
    let line = text.lines().find_map(|l| l.strip_prefix("MemAvailable:"))?;
    let mut words = line.split_whitespace();
    let n: u64 = words.next()?.parse().ok()?;
    match words.next() {
        Some("kB") => n.checked_mul(1024),
        None => Some(n),
        Some(_) => None,
    }
}

/// `(free to unprivileged writers, total)` bytes of the filesystem holding
/// `path`.
fn statvfs(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path, `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_bavail as u64 * frsize, st.f_blocks as u64 * frsize))
}

/// A size: bytes, or a number with `K`, `M`, `G` or `T` (binary; `Gi`,
/// `GB`, `GiB` the same, any case).
pub fn parse_size(text: &str) -> Result<u64> {
    let t = text.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let value: f64 = match num.parse() {
        Ok(v) if num.chars().next().is_some_and(|c| c.is_ascii_digit()) => v,
        _ => bail!("`{text}` isn't a size (like `16G`)"),
    };
    let unit = unit.trim().to_ascii_lowercase();
    let unit = unit.trim_end_matches('b').trim_end_matches('i');
    let shift = match unit {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        _ => bail!("`{text}` isn't a size (like `16G`)"),
    };
    Ok((value * (1u64 << shift) as f64) as u64)
}

/// A soft cap exceeded: `item`'s use over its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Over {
    pub item: CapItem,
    pub used: u64,
    pub cap: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapItem {
    Mem,
    Disk,
    Instances,
}

impl CapItem {
    pub fn as_str(self) -> &'static str {
        match self {
            CapItem::Mem => "mem",
            CapItem::Disk => "disk",
            CapItem::Instances => "instances",
        }
    }
}

impl fmt::Display for Over {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.item {
            CapItem::Instances => write!(f, "instances {}/{}", self.used, self.cap),
            item => write!(f, "{} {}/{}", item.as_str(), size(self.used), size(self.cap)),
        }
    }
}

/// The soft caps `c` exceeds (pure; placement's score reads the same):
/// memory in use (`mem_total - mem_available`) over `mem`, disk in use
/// (`disk_total - disk_free`) over `disk`, `running` over `instances`.
/// `cpus` has no item: no cpu use is reported. An item whose use is
/// unknown is never over.
pub fn over_caps(c: &Capacity) -> Vec<Over> {
    let Some(caps) = &c.caps else { return Vec::new() };
    let used = |total: Option<u64>, free: Option<u64>| Some(total?.saturating_sub(free?));
    [
        (CapItem::Mem, used(c.mem_total, c.mem_available), caps.mem),
        (CapItem::Disk, used(c.disk_total, c.disk_free), caps.disk),
        (CapItem::Instances, Some(c.running as u64), caps.instances),
    ]
    .into_iter()
    .filter_map(|(item, used, cap)| {
        let (used, cap) = (used?, cap?);
        (used > cap).then_some(Over { item, used, cap })
    })
    .collect()
}

/// `over cap: mem 18G/16G, instances 4/3`; `None` when nothing is over.
pub fn over_note(c: &Capacity) -> Option<String> {
    let over = over_caps(c);
    (!over.is_empty()).then(|| format!("over cap: {}", over.iter().map(Over::to_string).collect::<Vec<_>>().join(", ")))
}

/// The LEND column: `on`, `drain`, `off`, `-` (no report or no `lend.toml`).
pub fn lend_label(c: Option<&Capacity>) -> &'static str {
    c.and_then(|c| c.lending).map_or("-", State::as_str)
}

/// The CAPACITY column: `8 cpu · 12/32G · 120G free · 3 run`, each part
/// left out while unknown (memory as `32G` without `mem_available`); `-`
/// without a report.
pub fn summary(c: Option<&Capacity>) -> String {
    let Some(c) = c else { return "-".into() };
    let mut parts = Vec::new();
    if let Some(n) = c.cpus {
        parts.push(format!("{n} cpu"));
    }
    match (c.mem_total, c.mem_available) {
        (Some(total), Some(avail)) => {
            let unit = unit_of(total);
            parts.push(format!("{}/{}{}", number(total.saturating_sub(avail), unit), number(total, unit), UNITS[unit]));
        }
        (Some(total), None) => parts.push(size(total)),
        _ => {}
    }
    if let Some(free) = c.disk_free {
        parts.push(format!("{} free", size(free)));
    }
    parts.push(format!("{} run", c.running));
    parts.join(" · ")
}

const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];

fn unit_of(bytes: u64) -> usize {
    let mut unit = 0;
    while unit < UNITS.len() - 1 && bytes >= 1u64 << (10 * (unit + 1)) {
        unit += 1;
    }
    unit
}

/// `bytes` in `unit`: whole from 10 up, else one decimal (`1.5`, `8`).
fn number(bytes: u64, unit: usize) -> String {
    let v = bytes as f64 / (1u64 << (10 * unit)) as f64;
    if v >= 10.0 || unit == 0 {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
    }
}

/// `18G`, `1.5G`, `512M`, `2T`: binary units, short.
pub fn size(bytes: u64) -> String {
    let unit = unit_of(bytes);
    format!("{}{}", number(bytes, unit), UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    /// On demand: the cached `info` (asked once within the TTL), the poll's
    /// running count, the lend state given; `with_lend` swaps the latter.
    #[test]
    fn the_host_report_on_demand() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let host = HostCapacity::new(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(EngineInfo { cpus: Some(4), mem_total: Some(8 * G), root_dir: None, native: false })
        }));
        host.set_running(2);
        let lend = Lend { state: State::Drain, ..Lend::default() };
        let r = host.report(Some(&lend));
        assert_eq!((r.cpus, r.mem_total, r.mem_available, r.running, r.lending), (Some(4), Some(8 * G), None, 2, Some(State::Drain)));
        assert_eq!(host.report(None).lending, None);
        assert_eq!(asked.load(Ordering::SeqCst), 1, "cached");
        let on = Lend { state: State::On, ..Lend::default() };
        assert_eq!(r.clone().with_lend(Some(&on)), Capacity { lending: Some(State::On), ..r.clone() });
        assert_eq!(r.clone().with_lend(None), Capacity { lending: None, caps: None, ..r });
        // A runtime that won't answer: no engine part, the count still.
        let down = HostCapacity::new(Arc::new(|| bail!("no runtime")));
        down.set_running(1);
        assert_eq!(down.report(None), Capacity { running: 1, ..Capacity::default() });
    }

    #[test]
    fn meminfo_gives_mem_available_in_bytes() {
        let text = "MemTotal:       32582664 kB\nMemFree:         1234567 kB\nMemAvailable:   12582912 kB\nBuffers:          123 kB\n";
        assert_eq!(parse_meminfo(text), Some(12 * G));
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None, "an old kernel without it");
        assert_eq!(parse_meminfo("MemAvailable: x kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable: 5 MB\n"), None);
        assert_eq!(parse_meminfo(""), None);
    }

    #[test]
    fn sizes_parse_in_binary_units() {
        assert_eq!(parse_size("16G").unwrap(), 16 * G);
        assert_eq!(parse_size("16GiB").unwrap(), 16 * G);
        assert_eq!(parse_size("16 gb").unwrap(), 16 * G);
        assert_eq!(parse_size("1.5T").unwrap(), 1536 * G);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        for bad in ["", "G", "lots", "-1G", "16X", ".5G", "1.2.3G"] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sizes_print_short() {
        assert_eq!(size(18 * G), "18G");
        assert_eq!(size(3 * G / 2), "1.5G");
        assert_eq!(size(8 * G), "8G");
        assert_eq!(size(512 << 20), "512M");
        assert_eq!(size(2048 * G), "2T");
        assert_eq!(size(100), "100B");
    }

    #[test]
    fn the_engine_cache_asks_every_ttl_and_keeps_the_last_answer() {
        let info = |cpus| EngineInfo { cpus: Some(cpus), ..EngineInfo::default() };
        let mut cache = EngineCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.get(t0, || Ok(info(4))).unwrap().cpus, Some(4));
        let never = || -> Result<EngineInfo> { panic!("asked within the ttl") };
        assert_eq!(cache.get(t0 + ENGINE_TTL - Duration::from_millis(1), never).unwrap().cpus, Some(4));
        assert_eq!(cache.get(t0 + ENGINE_TTL, || Ok(info(8))).unwrap().cpus, Some(8));
        // A failure keeps the last answer, and isn't retried before the ttl.
        let t1 = t0 + 2 * ENGINE_TTL;
        assert_eq!(cache.get(t1, || bail!("down")).unwrap().cpus, Some(8));
        assert_eq!(cache.get(t1 + Duration::from_secs(1), never).unwrap().cpus, Some(8));
        // Never answered: nothing.
        let mut empty = EngineCache::default();
        assert!(empty.get(t0, || bail!("down")).is_none());
    }

    fn lend(state: State, caps: SoftCaps) -> Lend {
        Lend { state, hub: "hub".into(), caps, ..Lend::default() }
    }

    #[test]
    fn the_report_from_its_parts() {
        let engine = EngineInfo { cpus: Some(8), mem_total: Some(32 * G), root_dir: None, native: true };
        let host = HostFacts { mem_available: Some(20 * G), disk_free: Some(120 * G), disk_total: Some(500 * G) };
        let soft = SoftCaps { mem: Some("16G".into()), instances: Some(3), ..SoftCaps::default() };
        let c = Capacity::build(Some(&engine), host, 3, Some(&lend(State::Drain, soft)));
        assert_eq!(
            c,
            Capacity {
                cpus: Some(8),
                mem_total: Some(32 * G),
                mem_available: Some(20 * G),
                disk_free: Some(120 * G),
                disk_total: Some(500 * G),
                running: 3,
                lending: Some(State::Drain),
                caps: Some(Caps { cpus: None, mem: Some(16 * G), disk: None, instances: Some(3) }),
            }
        );
        assert_eq!(summary(Some(&c)), "8 cpu · 12/32G · 120G free · 3 run");
        assert_eq!(lend_label(Some(&c)), "drain");
        // No engine answer, no lend.toml.
        let bare = Capacity::build(None, HostFacts::default(), 0, None);
        assert_eq!((bare.cpus, bare.lending, bare.caps.as_ref()), (None, None, None));
        assert_eq!(summary(Some(&bare)), "0 run");
        assert_eq!((summary(None), lend_label(None), lend_label(Some(&bare))), ("-".into(), "-", "-"));
        // A VM's memory without what's free.
        let vm = Capacity { mem_total: Some(8 * G), ..bare.clone() };
        assert_eq!(summary(Some(&vm)), "8G · 0 run");
        // A lend.toml with no caps: none reported.
        assert_eq!(Capacity::build(None, HostFacts::default(), 0, Some(&lend(State::On, SoftCaps::default()))).caps, None);
        // Not native: never read.
        let vm_engine = EngineInfo { native: false, root_dir: Some("/".into()), ..engine };
        assert_eq!(HostFacts::read(Some(&vm_engine)), HostFacts::default());
        assert_eq!(HostFacts::read(None), HostFacts::default());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_facts_read_this_hosts_memory_and_the_roots_disk() {
        let engine = EngineInfo { native: true, root_dir: Some(std::env::temp_dir()), ..EngineInfo::default() };
        let facts = HostFacts::read(Some(&engine));
        assert!(facts.mem_available.is_some_and(|m| m > 0), "{facts:?}");
        let (free, total) = (facts.disk_free.unwrap(), facts.disk_total.unwrap());
        assert!(total > 0 && free <= total, "{facts:?}");
        let gone = EngineInfo { root_dir: Some("/nonexistent/devsandbox".into()), ..engine };
        assert_eq!(HostFacts::read(Some(&gone)).disk_free, None);
    }

    #[test]
    fn npm_typings_match() {
        use crate::serve::dts::assert_matches;
        let caps = Caps { cpus: Some(4), mem: Some(G), disk: None, instances: Some(2) };
        let c = Capacity { caps: Some(caps), lending: Some(State::On), ..Capacity::default() };
        let v = serde_json::to_value(&c).unwrap();
        assert_matches("Capacity", &v);
        assert_matches("Caps", &v["caps"]);
        assert_eq!(v["lending"], "on");
        assert_eq!(crate::serve::dts::prop_type("InstancesSnapshot", "capacity").as_deref(), Some("Capacity"));
    }

    #[test]
    fn over_caps_lists_each_exceeded_cap() {
        let c = Capacity {
            mem_total: Some(32 * G),
            mem_available: Some(14 * G),
            disk_free: Some(400 * G),
            disk_total: Some(500 * G),
            running: 4,
            caps: Some(Caps { cpus: Some(2), mem: Some(16 * G), disk: Some(200 * G), instances: Some(3) }),
            ..Capacity::default()
        };
        let over = over_caps(&c);
        assert_eq!(
            over,
            [Over { item: CapItem::Mem, used: 18 * G, cap: 16 * G }, Over { item: CapItem::Instances, used: 4, cap: 3 }]
        );
        assert_eq!(over_note(&c).as_deref(), Some("over cap: mem 18G/16G, instances 4/3"));
        // At the cap isn't over; unknown use never is; no caps, nothing.
        let at = Capacity { running: 3, mem_available: None, ..c.clone() };
        assert_eq!(over_caps(&at), []);
        let disk = Capacity { disk_free: Some(100 * G), ..at.clone() };
        assert_eq!(over_note(&disk).as_deref(), Some("over cap: disk 400G/200G"));
        assert_eq!(over_note(&Capacity { caps: None, ..c }), None);
    }
}
