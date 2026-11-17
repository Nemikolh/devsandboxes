//! A host's capacity report (docs/architecture/daemon/fleet.md, *Capacity*;
//! fleet plan decision 8): cpus and memory from the runtime's `info`
//! (cached [`ENGINE_TTL`]), free memory and disk only for a native Linux
//! engine, the running instance count and `member.toml`'s soft caps, and
//! whether a GPU is usable and lent ([`GpuReport`]). The host poll (`host.rs`) builds one per watched tick and the
//! snapshots carry it (`Snapshots::set_capacity`); a machine's comes from
//! its pushed snapshot (`snapshots::SnapshotMachine::capacity`). On demand,
//! `capacity.get` and a placed `run` build one through [`HostCapacity`],
//! the same engine cache and running count the poll uses, watched or not.
//!
//! Everything but [`HostFacts::read`] and the injected runtime calls is pure:
//! the parsers, the cache over [`Instant`]s, [`over_caps`] (what placement
//! will score against) and the dashboard's and `machine ls`' text.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::fleet::member::SoftCaps;
use crate::runtime::gpu::GpuSupport;
use crate::runtime::identity::EngineInfo;

/// How long one `info` answer is used: cpus, total memory and GPU support
/// rarely move.
pub const ENGINE_TTL: Duration = Duration::from_secs(60);

/// `member.toml`'s `[caps]` in bytes and counts (the wire's).
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
    /// (`member::check` refuses one).
    pub fn of(soft: &SoftCaps) -> Option<Caps> {
        let size = |s: &Option<String>| s.as_deref().and_then(|s| parse_size(s).ok());
        let caps = Caps { cpus: soft.cpus, mem: size(&soft.mem), disk: size(&soft.disk), instances: soft.instances };
        (caps != Caps::default()).then_some(caps)
    }
}

/// One host's capacity, as a snapshot carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// A container runtime is installed there (`runtime::installed`);
    /// `false` is a driver, which placement never picks. Absent reads as
    /// installed.
    #[serde(default = "installed")]
    pub runtime: bool,
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
    /// `member.toml`'s soft caps; `None` when none is set.
    pub caps: Option<Caps>,
    /// A GPU the runtime can give a container; `None`: none usable, or
    /// unknown (placement treats both as none). Absent reads as `None`.
    #[serde(default)]
    pub gpu: Option<GpuReport>,
}

/// The [`Capacity::gpu`] of a host whose runtime can give a container
/// every NVIDIA GPU (`GpuSupport::Gpus` | `Cdi`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuReport {
    /// `/dev/nvidia<N>` nodes on a native engine; `None` on any other
    /// (a VM's devices aren't seen here).
    pub count: Option<u64>,
    /// Placed work may use it here: `trust = "full"` or `[allow] gpus =
    /// true` in `member.toml` ([`Own::lends_gpu`]).
    pub lent: bool,
}

fn installed() -> bool {
    true
}

/// Nothing known, a runtime installed.
impl Default for Capacity {
    fn default() -> Self {
        Capacity {
            runtime: true,
            cpus: None,
            mem_total: None,
            mem_available: None,
            disk_free: None,
            disk_total: None,
            running: 0,
            caps: None,
            gpu: None,
        }
    }
}

/// What this host's own files put in its report: `member.toml`'s `[caps]`
/// and whether it lends its GPU (`Member::lends_gpu`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Own {
    pub caps: SoftCaps,
    pub lends_gpu: bool,
}

/// What this host says about itself beyond the runtime's `info`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostFacts {
    pub mem_available: Option<u64>,
    pub disk_free: Option<u64>,
    pub disk_total: Option<u64>,
    /// NVIDIA GPU device nodes in `/dev` (`gpu::nvidia_device_count`).
    pub gpus: Option<u64>,
}

impl HostFacts {
    /// Read for a native Linux engine only (`engine.native` on Linux):
    /// anywhere else this host's memory, disk and devices aren't the
    /// engine's. The disk is the filesystem of `root_dir`, when it exists
    /// here.
    pub fn read(engine: Option<&EngineInfo>) -> HostFacts {
        let Some(engine) = engine.filter(|e| e.native && cfg!(target_os = "linux")) else {
            return HostFacts::default();
        };
        let mem_available = std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| parse_meminfo(&t));
        let (disk_free, disk_total) = engine.root_dir.as_deref().and_then(statvfs).map_or((None, None), |(f, t)| (Some(f), Some(t)));
        let gpus = std::fs::read_dir("/dev").ok().map(|entries| {
            let names = entries.filter_map(|e| e.ok()?.file_name().into_string().ok());
            crate::runtime::gpu::nvidia_device_count(names)
        });
        HostFacts { mem_available, disk_free, disk_total, gpus }
    }
}

impl Capacity {
    /// The report from its parts (pure), a runtime installed. A `gpu`
    /// support but `None` is no GPU. A native engine with no device node
    /// yet (`host.gpus` `Some(0)`) has an unknown count: the driver creates
    /// `/dev/nvidia<N>` on first use, so a usable runtime outweighs it.
    pub fn build(engine: Option<&EngineInfo>, host: HostFacts, gpu: Option<&GpuSupport>, running: usize, own: &Own) -> Capacity {
        let usable = matches!(gpu, Some(GpuSupport::Gpus | GpuSupport::Cdi));
        Capacity {
            runtime: true,
            cpus: engine.and_then(|e| e.cpus),
            mem_total: engine.and_then(|e| e.mem_total),
            mem_available: host.mem_available,
            disk_free: host.disk_free,
            disk_total: host.disk_total,
            running,
            caps: Caps::of(&own.caps),
            gpu: usable.then_some(GpuReport { count: host.gpus.filter(|n| *n > 0), lent: own.lends_gpu }),
        }
    }

    /// This report with `own`'s caps and GPU lending instead of its own
    /// (pure).
    pub fn with_own(self, own: &Own) -> Capacity {
        let gpu = self.gpu.map(|g| GpuReport { lent: own.lends_gpu, ..g });
        Capacity { caps: Caps::of(&own.caps), gpu, ..self }
    }
}

/// How the runtime's `info` is asked (`Backend::engine_info` outside tests).
pub type EngineInfoFn = Arc<dyn Fn() -> Result<EngineInfo> + Send + Sync>;

/// How GPU support is asked (`Backend::gpu_support` outside tests).
pub type GpuSupportFn = Arc<dyn Fn() -> Result<GpuSupport> + Send + Sync>;

/// Whether a runtime is installed (`runtime::installed::installed`
/// outside tests).
pub type InstalledFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// This host's report, on demand: one [`EngineCache`] each for `info` and
/// GPU support, and the running count, shared by the poll (which sets the count from each listing and
/// asks for the report while a root is watched) and the daemon
/// (`capacity.get`, a placed `run`), so asking needs no watched root.
pub struct HostCapacity {
    engine: Mutex<EngineCache>,
    gpu_cache: Mutex<EngineCache<GpuSupport>>,
    running: AtomicUsize,
    info: EngineInfoFn,
    gpu: GpuSupportFn,
    installed: InstalledFn,
}

impl HostCapacity {
    pub fn new(info: EngineInfoFn, gpu: GpuSupportFn, installed: InstalledFn) -> HostCapacity {
        HostCapacity {
            engine: Mutex::default(),
            gpu_cache: Mutex::default(),
            running: AtomicUsize::new(0),
            info,
            gpu,
            installed,
        }
    }

    /// The poll's latest running count (`host::running_instances`).
    pub fn set_running(&self, running: usize) {
        self.running.store(running, Ordering::Release);
    }

    /// The report now: the cached `info` and GPU support (each asked again
    /// past [`ENGINE_TTL`]), [`HostFacts::read`], the last running count,
    /// `own`. With no runtime installed: `runtime: false`, no GPU, and
    /// nothing asked of one.
    pub fn report(&self, own: &Own) -> Capacity {
        if !(self.installed)() {
            return Capacity { runtime: false, ..Capacity::build(None, HostFacts::default(), None, 0, own) };
        }
        let now = Instant::now();
        let info = self.engine.lock().unwrap_or_else(|e| e.into_inner()).get(now, || (self.info)()).cloned();
        let gpu = self.gpu_cache.lock().unwrap_or_else(|e| e.into_inner()).get(now, || (self.gpu)()).cloned();
        let host = HostFacts::read(info.as_ref());
        Capacity::build(info.as_ref(), host, gpu.as_ref(), self.running.load(Ordering::Acquire), own)
    }
}

/// The runtime's last `info` (or GPU support), asked again every
/// [`ENGINE_TTL`].
#[derive(Debug)]
pub struct EngineCache<T = EngineInfo> {
    last: Option<(Instant, Option<T>)>,
}

impl<T> Default for EngineCache<T> {
    fn default() -> Self {
        EngineCache { last: None }
    }
}

impl<T> EngineCache<T> {
    /// The cached answer, `fetch`ed first when there's none or it's
    /// [`ENGINE_TTL`] old. A failed fetch keeps the last answer and waits
    /// out the same interval (a runtime that's down isn't asked every tick).
    pub fn get(&mut self, now: Instant, fetch: impl FnOnce() -> Result<T>) -> Option<&T> {
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

/// The CAPACITY column: `8 cpu · 12/32G · 120G free · 3 run · 2 gpu`, each
/// part left out while unknown (memory as `32G` without `mem_available`,
/// the GPU as `gpu` without a count, `(not lent)` after it unless lent);
/// `-` without a report, `no runtime` for a driver.
pub fn summary(c: Option<&Capacity>) -> String {
    let Some(c) = c else { return "-".into() };
    if !c.runtime {
        return "no runtime".into();
    }
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
    if let Some(gpu) = &c.gpu {
        let n = gpu.count.map(|n| format!("{n} ")).unwrap_or_default();
        parts.push(format!("{n}gpu{}", if gpu.lent { "" } else { " (not lent)" }));
    }
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
    /// running count, the caps given; `with_own` swaps the latter.
    #[test]
    fn the_host_report_on_demand() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let host = HostCapacity::new(
            Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(EngineInfo { cpus: Some(4), mem_total: Some(8 * G), root_dir: None, native: false })
            }),
            Arc::new(|| Ok(GpuSupport::None("none here".into()))),
            Arc::new(|| true),
        );
        host.set_running(2);
        let capped = Own { caps: SoftCaps { cpus: Some(2), ..SoftCaps::default() }, ..Own::default() };
        let r = host.report(&capped);
        assert_eq!((r.cpus, r.mem_total, r.mem_available, r.running), (Some(4), Some(8 * G), None, 2));
        assert_eq!(r.caps.as_ref().and_then(|c| c.cpus), Some(2));
        assert_eq!(host.report(&Own::default()).caps, None);
        assert_eq!(asked.load(Ordering::SeqCst), 1, "cached");
        assert_eq!(r.clone().with_own(&capped), r.clone());
        assert_eq!(r.clone().with_own(&Own::default()), Capacity { caps: None, ..r });
        // A runtime that won't answer: no engine part, the count still.
        let down = HostCapacity::new(Arc::new(|| bail!("no runtime")), Arc::new(|| bail!("no runtime")), Arc::new(|| true));
        down.set_running(1);
        assert_eq!(down.report(&Own::default()), Capacity { running: 1, ..Capacity::default() });
    }

    /// No runtime installed: `runtime: false`, its `info` never asked, the
    /// caps still; the column says so; a report without the field (or
    /// `Capacity::default()`) has one.
    #[test]
    fn a_host_without_a_runtime_reports_none() {
        let host = HostCapacity::new(Arc::new(|| panic!("info asked")), Arc::new(|| panic!("gpu asked")), Arc::new(|| false));
        host.set_running(3);
        let capped = Own { caps: SoftCaps { instances: Some(2), ..SoftCaps::default() }, lends_gpu: true };
        let r = host.report(&capped);
        assert!(!r.runtime);
        assert_eq!(r.gpu, None);
        assert_eq!((r.cpus, r.mem_total, r.running), (None, None, 0));
        assert_eq!(r.caps.as_ref().and_then(|c| c.instances), Some(2));
        assert_eq!(summary(Some(&r)), "no runtime");
        assert_eq!(serde_json::to_value(&r).unwrap()["runtime"], serde_json::json!(false));
        let old: Capacity = serde_json::from_value(serde_json::json!({
            "cpus": 4, "mem_total": null, "mem_available": null, "disk_free": null, "disk_total": null,
            "running": 1, "caps": null,
        }))
        .unwrap();
        assert!(old.runtime, "absent: installed");
        assert_eq!(old.gpu, None, "absent: no GPU");
        assert!(Capacity::default().runtime);
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
        let mut empty = EngineCache::<EngineInfo>::default();
        assert!(empty.get(t0, || bail!("down")).is_none());
    }

    fn own(caps: SoftCaps) -> Own {
        Own { caps, ..Own::default() }
    }

    #[test]
    fn the_report_from_its_parts() {
        let engine = EngineInfo { cpus: Some(8), mem_total: Some(32 * G), root_dir: None, native: true };
        let host = HostFacts { mem_available: Some(20 * G), disk_free: Some(120 * G), disk_total: Some(500 * G), gpus: Some(0) };
        let soft = SoftCaps { mem: Some("16G".into()), instances: Some(3), ..SoftCaps::default() };
        let c = Capacity::build(Some(&engine), host, None, 3, &own(soft));
        assert_eq!(
            c,
            Capacity {
                runtime: true,
                cpus: Some(8),
                mem_total: Some(32 * G),
                mem_available: Some(20 * G),
                disk_free: Some(120 * G),
                disk_total: Some(500 * G),
                running: 3,
                caps: Some(Caps { cpus: None, mem: Some(16 * G), disk: None, instances: Some(3) }),
                gpu: None,
            }
        );
        assert_eq!(summary(Some(&c)), "8 cpu · 12/32G · 120G free · 3 run");
        // No engine answer, no member.toml.
        let bare = Capacity::build(None, HostFacts::default(), None, 0, &Own::default());
        assert_eq!((bare.cpus, bare.caps.as_ref()), (None, None));
        assert_eq!(summary(Some(&bare)), "0 run");
        assert_eq!(summary(None), "-");
        // A VM's memory without what's free.
        let vm = Capacity { mem_total: Some(8 * G), ..bare.clone() };
        assert_eq!(summary(Some(&vm)), "8G · 0 run");
        assert_eq!(Capacity::build(None, HostFacts::default(), None, 0, &own(SoftCaps::default())).caps, None);
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

    /// A usable GPU is reported with the device count a native engine
    /// gives and `own`'s lending; none usable, or a native engine with no
    /// device node, is no GPU; `with_own` swaps the lending; the cache asks
    /// GPU support once within the TTL.
    #[test]
    fn the_gpu_report() {
        let native = HostFacts { gpus: Some(2), ..HostFacts::default() };
        let lends = Own { lends_gpu: true, ..Own::default() };
        for support in [GpuSupport::Gpus, GpuSupport::Cdi] {
            let c = Capacity::build(None, native, Some(&support), 0, &lends);
            assert_eq!(c.gpu, Some(GpuReport { count: Some(2), lent: true }));
        }
        let c = Capacity::build(None, native, Some(&GpuSupport::Cdi), 1, &Own::default());
        assert_eq!(c.gpu, Some(GpuReport { count: Some(2), lent: false }));
        assert_eq!(summary(Some(&c)), "1 run · 2 gpu (not lent)");
        assert_eq!(c.clone().with_own(&lends).gpu, Some(GpuReport { count: Some(2), lent: true }));
        assert_eq!(summary(Some(&c.with_own(&lends))), "1 run · 2 gpu");
        // Not native: usable, count unknown.
        let vm = Capacity::build(None, HostFacts::default(), Some(&GpuSupport::Gpus), 0, &lends);
        assert_eq!(vm.gpu, Some(GpuReport { count: None, lent: true }));
        assert_eq!(summary(Some(&vm)), "0 run · gpu");
        let one = Capacity { gpu: Some(GpuReport { count: Some(1), lent: false }), ..Capacity::default() };
        assert_eq!(summary(Some(&one)), "0 run · 1 gpu (not lent)");
        // None usable, unknown, or no device node.
        let none = GpuSupport::None("no".into());
        assert_eq!(Capacity::build(None, native, Some(&none), 0, &lends).gpu, None);
        assert_eq!(Capacity::build(None, native, None, 0, &lends).gpu, None);
        let no_node = HostFacts { gpus: Some(0), ..HostFacts::default() };
        assert_eq!(
            Capacity::build(None, no_node, Some(&GpuSupport::Gpus), 0, &lends).gpu,
            Some(GpuReport { count: None, lent: true }),
            "no node yet: the count is unknown"
        );
        assert_eq!(Capacity::default().with_own(&lends).gpu, None, "lending adds no GPU");
        // Through the host report: asked once within the TTL.
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let host = HostCapacity::new(
            Arc::new(|| Ok(EngineInfo::default())),
            Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(GpuSupport::Gpus)
            }),
            Arc::new(|| true),
        );
        assert_eq!(host.report(&lends).gpu, Some(GpuReport { count: None, lent: true }));
        assert_eq!(host.report(&Own::default()).gpu, Some(GpuReport { count: None, lent: false }));
        assert_eq!(asked.load(Ordering::SeqCst), 1, "cached");
        // A report round-trips.
        let back: Capacity = serde_json::from_value(serde_json::to_value(&one).unwrap()).unwrap();
        assert_eq!(back, one);
    }

    #[test]
    fn npm_typings_match() {
        use crate::serve::dts::assert_matches;
        let caps = Caps { cpus: Some(4), mem: Some(G), disk: None, instances: Some(2) };
        let gpu = GpuReport { count: Some(1), lent: true };
        let c = Capacity { caps: Some(caps), gpu: Some(gpu), ..Capacity::default() };
        let v = serde_json::to_value(&c).unwrap();
        assert_matches("Capacity", &v);
        assert_matches("Caps", &v["caps"]);
        assert_matches("GpuReport", &v["gpu"]);
        assert_matches("Capacity", &serde_json::to_value(Capacity::default()).unwrap());
        assert!(v.get("lending").is_none());
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
