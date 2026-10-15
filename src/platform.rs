//! OS topology, conservative allocation budgets, and scoped thread affinity.
#[cfg(any(windows, target_os = "linux", test))]
use std::collections::BTreeMap;
#[cfg(any(windows, target_os = "linux", test))]
use std::collections::BTreeSet;
#[cfg(any(windows, target_os = "linux"))]
use std::io;

#[derive(Debug, Clone)]
pub struct Cache {
    pub level: u8,
    pub bytes: usize,
    /// Zero when the OS does not report a cache line size.
    pub line_bytes: usize,
    /// Accessible CPUs sharing this data/unified cache. Empty on macOS, where
    /// scheduling and exact cache-domain placement are not controlled.
    pub cpus: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Hardware {
    /// Linux allowed IDs; Windows IDs encode processor group * 64 + bit.
    pub cpus: Vec<usize>,
    /// Stable physical-core representatives where OS topology is available.
    pub preferred: Vec<usize>,
    /// Reported L1/L2/L3 data/unified caches; missing metadata is omitted.
    pub caches: Vec<Cache>,
    /// Largest available data/unified cache, or zero if undetected.
    pub cache_bytes: usize,
    /// Distinct highest-level cache domains visible to the available CPU set.
    /// macOS uses performance-level counts, with a conservative upper-bound
    /// estimate (largest cache * physical workers) if those counts are absent.
    pub cache_total_bytes: usize,
    /// Reported cache line size, or zero when unavailable.
    pub cache_line_bytes: usize,
    /// A conservative allowance, not a reservation of free memory.
    pub memory_budget: usize,
    pub affinity: &'static str,
    pub page_size: usize,
}

#[cfg(all(windows, feature = "gui"))]
pub fn total_memory() -> Option<u64> {
    let mut status = native::MemoryStatus {
        length: size_of::<native::MemoryStatus>() as u32,
        ..Default::default()
    };
    (unsafe { native::GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.total)
}

#[cfg(windows)]
mod native {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn GetCurrentThread() -> *mut c_void;
        pub fn GetProcessAffinityMask(p: *mut c_void, a: *mut usize, s: *mut usize) -> i32;
        pub fn GetThreadGroupAffinity(t: *mut c_void, affinity: *mut GroupAffinity) -> i32;
        pub fn SetThreadGroupAffinity(
            t: *mut c_void,
            affinity: *const GroupAffinity,
            previous: *mut GroupAffinity,
        ) -> i32;
        pub fn GetActiveProcessorGroupCount() -> u16;
        pub fn GetSystemInfo(info: *mut Info);
        pub fn GetLogicalProcessorInformationEx(
            relation: u32,
            buffer: *mut u8,
            length: *mut u32,
        ) -> i32;
        pub fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
    }
    #[repr(C)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GroupAffinity {
        pub mask: usize,
        pub group: u16,
        pub reserved: [u16; 3],
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct Info {
        pub architecture: u32,
        pub page_size: u32,
        pub min: usize,
        pub max: usize,
        pub mask: usize,
        pub processors: u32,
        pub kind: u32,
        pub granularity: u32,
        pub level: u16,
        pub revision: u16,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct MemoryStatus {
        pub length: u32,
        pub load: u32,
        pub total: u64,
        pub available: u64,
        pub total_pagefile: u64,
        pub available_pagefile: u64,
        pub total_virtual: u64,
        pub available_virtual: u64,
        pub extended: u64,
    }
    pub fn topology(relation: u32) -> Vec<u8> {
        let mut length = 0;
        unsafe { GetLogicalProcessorInformationEx(relation, std::ptr::null_mut(), &mut length) };
        for _ in 0..3 {
            if length == 0 || length > 16 << 20 {
                break;
            }
            let capacity = length;
            let mut aligned = vec![0u64; (capacity as usize).div_ceil(8)];
            if unsafe {
                GetLogicalProcessorInformationEx(relation, aligned.as_mut_ptr().cast(), &mut length)
            } != 0
                && length <= capacity
            {
                return unsafe {
                    std::slice::from_raw_parts(aligned.as_ptr().cast(), length as usize).to_vec()
                };
            }
        }
        Vec::new()
    }
}
#[cfg(target_os = "linux")]
mod native {
    unsafe extern "C" {
        pub fn sched_getaffinity(pid: i32, size: usize, mask: *mut usize) -> i32;
        pub fn sched_setaffinity(pid: i32, size: usize, mask: *const usize) -> i32;
        pub fn getpagesize() -> i32;
    }
    pub fn mask() -> Option<Vec<usize>> {
        for words in [16, 32, 64, 128, 256, 512, 1024, 2048] {
            let mut mask = vec![0; words];
            if unsafe { sched_getaffinity(0, words * size_of::<usize>(), mask.as_mut_ptr()) } == 0 {
                return Some(mask);
            }
        }
        None
    }
}
#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_void;
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const u8,
            out: *mut c_void,
            size: *mut usize,
            new: *const c_void,
            new_size: usize,
        ) -> i32;
        pub fn getpagesize() -> i32;
    }
    pub fn value(name: &[u8]) -> usize {
        let mut value = 0u64;
        let mut size = 8;
        if unsafe {
            sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut u64).cast(),
                &mut size,
                std::ptr::null(),
                0,
            )
        } == 0
            && matches!(size, 4 | 8)
        {
            value as usize
        } else {
            0
        }
    }
}

/// Linux IDs reflect allowed affinity. Windows enumerates the caller's current
/// processor group, encoded as group*64+bit. macOS IDs are scheduler hints only.
pub fn cpus() -> Vec<usize> {
    #[cfg(windows)]
    {
        let (mut mask, mut system) = (0usize, 0usize);
        let mut group = native::GroupAffinity::default();
        if unsafe { native::GetThreadGroupAffinity(native::GetCurrentThread(), &mut group) } != 0 {
            // With GetCurrentProcess(), Windows reports the calling thread's
            // primary-group process mask, matching GetThreadGroupAffinity.
            if unsafe {
                native::GetProcessAffinityMask(native::GetCurrentProcess(), &mut mask, &mut system)
            } == 0
                || mask == 0
            {
                mask = group.mask;
            }
            let result: Vec<_> = (0..usize::BITS as usize)
                .filter(|i| mask & (1usize << i) != 0)
                .map(|i| usize::from(group.group) * 64 + i)
                .collect();
            if !result.is_empty() {
                return result;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let count = native::value(b"hw.logicalcpu\0");
        if count > 0 {
            return (0..count).collect();
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(mask) = native::mask() {
        let result: Vec<_> = (0..mask.len() * usize::BITS as usize)
            .filter(|i| {
                mask[i / usize::BITS as usize] & (1usize << (i % usize::BITS as usize)) != 0
            })
            .collect();
        if !result.is_empty() {
            return result;
        }
    }
    (0..std::thread::available_parallelism().map_or(1, usize::from)).collect()
}

/// Restores the calling thread's previous affinity when dropped.
pub struct Affinity {
    #[cfg(windows)]
    previous: native::GroupAffinity,
    #[cfg(target_os = "linux")]
    previous: Vec<usize>,
    // A guard must restore affinity on the same thread that created it.
    _thread: std::marker::PhantomData<*mut ()>,
}
pub fn pin(cpu: usize) -> Result<Affinity, String> {
    #[cfg(windows)]
    {
        let group = u16::try_from(cpu / 64).map_err(|_| "CPU group ID is too large")?;
        let target = native::GroupAffinity {
            mask: 1usize << (cpu % 64),
            group,
            reserved: [0; 3],
        };
        let mut previous = native::GroupAffinity::default();
        if unsafe {
            native::SetThreadGroupAffinity(native::GetCurrentThread(), &target, &mut previous)
        } == 0
        {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(Affinity {
            previous,
            _thread: std::marker::PhantomData,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let previous = native::mask().ok_or_else(|| io::Error::last_os_error().to_string())?;
        if cpu >= previous.len() * usize::BITS as usize {
            return Err("CPU ID exceeds affinity mask".into());
        }
        let mut mask = vec![0usize; previous.len()];
        mask[cpu / usize::BITS as usize] |= 1usize << (cpu % usize::BITS as usize);
        if unsafe { native::sched_setaffinity(0, mask.len() * size_of::<usize>(), mask.as_ptr()) }
            != 0
        {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(Affinity {
            previous,
            _thread: std::marker::PhantomData,
        })
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = cpu;
        Ok(Affinity {
            _thread: std::marker::PhantomData,
        })
    }
}
impl Drop for Affinity {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            native::SetThreadGroupAffinity(
                native::GetCurrentThread(),
                &self.previous,
                std::ptr::null_mut(),
            );
        }
        #[cfg(target_os = "linux")]
        unsafe {
            native::sched_setaffinity(
                0,
                self.previous.len() * size_of::<usize>(),
                self.previous.as_ptr(),
            );
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut ids = BTreeSet::new();
    for field in text.trim().split(',') {
        let mut range = field.split('-');
        let first: usize = range.next()?.parse().ok()?;
        let last = range.next().map_or(Some(first), |n| n.parse().ok())?;
        if range.next().is_some() || first > last || last > 65535 {
            return None;
        }
        ids.extend(first..=last);
    }
    Some(ids.into_iter().collect())
}

#[cfg(any(target_os = "linux", test))]
fn cache_size(text: &str) -> Option<usize> {
    let text = text.trim();
    let (number, scale) = match text.as_bytes().last()? {
        b'K' | b'k' => (&text[..text.len() - 1], 1024),
        b'M' | b'm' => (&text[..text.len() - 1], 1024 * 1024),
        b'G' | b'g' => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    number.parse::<usize>().ok()?.checked_mul(scale)
}

#[cfg(any(windows, target_os = "linux", test))]
fn representatives(allowed: &[usize], core_groups: &[Vec<usize>]) -> Vec<usize> {
    let mut selected = BTreeSet::new();
    for &cpu in allowed {
        let representative = core_groups
            .iter()
            .find(|group| group.contains(&cpu))
            .and_then(|group| {
                group
                    .iter()
                    .copied()
                    .filter(|id| allowed.binary_search(id).is_ok())
                    .min()
            })
            .unwrap_or(cpu);
        selected.insert(representative);
    }
    selected.into_iter().collect()
}

#[cfg(any(windows, target_os = "linux", test))]
struct CacheDomain {
    level: usize,
    bytes: usize,
    shared: Vec<usize>,
}

#[cfg(any(windows, target_os = "linux", test))]
fn total_cache(allowed: &[usize], domains: &[CacheDomain]) -> usize {
    let visible = |domain: &CacheDomain| {
        domain
            .shared
            .iter()
            .any(|id| allowed.binary_search(id).is_ok())
    };
    let level = domains
        .iter()
        .filter(|c| visible(c))
        .map(|c| c.level)
        .max()
        .unwrap_or(0);
    let mut unique = BTreeMap::<Vec<usize>, usize>::new();
    for domain in domains.iter().filter(|c| c.level == level && visible(c)) {
        let mut shared = domain.shared.clone();
        shared.sort_unstable();
        shared.dedup();
        let bytes = unique.entry(shared).or_default();
        *bytes = (*bytes).max(domain.bytes);
    }
    unique
        .values()
        .fold(0usize, |total, bytes| total.saturating_add(*bytes))
}

#[cfg(any(windows, target_os = "linux", test))]
fn visible_caches(allowed: &[usize], caches: Vec<Cache>) -> Vec<Cache> {
    let allowed: BTreeSet<_> = allowed.iter().copied().collect();
    let mut unique = BTreeMap::<(u8, Vec<usize>), (usize, usize)>::new();
    for cache in caches {
        if !(1..=3).contains(&cache.level) || cache.bytes == 0 {
            continue;
        }
        let mut cpus: Vec<_> = cache
            .cpus
            .into_iter()
            .filter(|id| allowed.contains(id))
            .collect();
        cpus.sort_unstable();
        cpus.dedup();
        if cpus.is_empty() {
            continue;
        }
        let value = unique.entry((cache.level, cpus)).or_default();
        if cache.bytes > value.0 {
            *value = (cache.bytes, cache.line_bytes);
        } else if cache.bytes == value.0 {
            value.1 = value.1.max(cache.line_bytes);
        }
    }
    unique
        .into_iter()
        .map(|((level, cpus), (bytes, line_bytes))| Cache {
            level,
            bytes,
            line_bytes,
            cpus,
        })
        .collect()
}

#[cfg(any(windows, test))]
fn word(data: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?) as usize)
}
#[cfg(any(windows, test))]
fn dword(data: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize)
}
#[cfg(any(windows, test))]
fn windows_records(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let size = dword(rest, 4)?;
        if size < 8 || size > rest.len() {
            rest = &[];
            return None;
        }
        let (record, tail) = rest.split_at(size);
        rest = tail;
        Some(record)
    })
}
#[cfg(any(windows, test))]
fn windows_masks(record: &[u8], count_at: usize, array_at: usize) -> Option<Vec<usize>> {
    // Legacy cache records expose zero in the reserved GroupCount field.
    let count = word(record, count_at)?.max(1);
    let mut ids = Vec::new();
    for n in 0..count {
        let at = array_at.checked_add(n.checked_mul(16)?)?;
        let mask = u64::from_le_bytes(record.get(at..at + 8)?.try_into().ok()?);
        let group = word(record, at + 8)?;
        ids.extend(
            (0..64)
                .filter(|bit| mask & (1u64 << bit) != 0)
                .map(|bit| group * 64 + bit),
        );
    }
    Some(ids)
}

#[cfg(any(windows, test))]
fn windows_cache(record: &[u8], allowed: &[usize]) -> Option<(CacheDomain, usize)> {
    // PROCESSOR_CACHE_TYPE: 0 unified, 1 instruction, 2 data.
    if dword(record, 0) != Some(2) || !matches!(dword(record, 16), Some(0 | 2)) {
        return None;
    }
    let shared = windows_masks(record, 38, 40)?;
    if !shared.iter().any(|id| allowed.binary_search(id).is_ok()) {
        return None;
    }
    Some((
        CacheDomain {
            level: *record.get(8)? as usize,
            bytes: dword(record, 12)?,
            shared,
        },
        word(record, 10)?,
    ))
}

pub fn hardware() -> Hardware {
    let cpus = cpus();
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    let caches;
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let caches = Vec::new();
    #[cfg(any(windows, target_os = "linux"))]
    let preferred;
    #[cfg(not(any(windows, target_os = "linux")))]
    let mut preferred = cpus.clone();
    let (mut cache_bytes, mut cache_line_bytes) = (0, 0);
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    let cache_total_bytes;
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let cache_total_bytes = 0;
    #[cfg(windows)]
    let mut page_size = 4096;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let page_size;
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let page_size = 4096;
    #[cfg(any(windows, target_os = "linux"))]
    let affinity;
    #[cfg(not(any(windows, target_os = "linux")))]
    let affinity = "scheduler";
    #[cfg(windows)]
    {
        let records = native::topology(0);
        let groups: Vec<_> = windows_records(&records)
            .filter(|r| dword(r, 0) == Some(0))
            .filter_map(|r| windows_masks(r, 30, 32))
            .collect();
        preferred = representatives(&cpus, &groups);
        let mut domains = Vec::new();
        let mut inventory = Vec::new();
        for record in windows_records(&native::topology(2)) {
            if let Some((domain, line)) = windows_cache(record, &cpus) {
                cache_bytes = cache_bytes.max(domain.bytes);
                cache_line_bytes = cache_line_bytes.max(line);
                inventory.push(Cache {
                    level: u8::try_from(domain.level).unwrap_or(0),
                    bytes: domain.bytes,
                    line_bytes: line,
                    cpus: domain.shared.clone(),
                });
                domains.push(domain);
            }
        }
        cache_total_bytes = total_cache(&cpus, &domains).max(cache_bytes);
        caches = visible_caches(&cpus, inventory);
        let mut info = native::Info::default();
        unsafe { native::GetSystemInfo(&mut info) };
        if info.page_size != 0 {
            page_size = info.page_size as usize;
        }
        affinity = if unsafe { native::GetActiveProcessorGroupCount() } > 1 {
            "hard/current-group"
        } else {
            "hard"
        };
    }
    #[cfg(target_os = "linux")]
    {
        let read = |path| std::fs::read_to_string(path).ok();
        let mut groups = Vec::new();
        let mut domains = Vec::new();
        let mut inventory = Vec::new();
        for &cpu in &cpus {
            let base = std::path::PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}"));
            if let Some(group) =
                read(base.join("topology/thread_siblings_list")).and_then(|s| cpu_list(&s))
                && !groups.contains(&group)
            {
                groups.push(group);
            }
            if let Ok(entries) = std::fs::read_dir(base.join("cache")) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let kind = read(path.join("type")).unwrap_or_default();
                    if !matches!(kind.trim(), "Data" | "Unified") {
                        continue;
                    }
                    let line = read(path.join("coherency_line_size"))
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    cache_line_bytes = cache_line_bytes.max(line);
                    if let Some(size) = read(path.join("size")).and_then(|s| cache_size(&s)) {
                        cache_bytes = cache_bytes.max(size);
                        let level = read(path.join("level"))
                            .and_then(|s| s.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        // Missing sharing metadata deliberately overestimates
                        // by treating this CPU's cache as a distinct domain.
                        let shared = read(path.join("shared_cpu_list")).and_then(|s| cpu_list(&s));
                        if let Some(shared) = shared.as_ref().filter(|ids| ids.contains(&cpu)) {
                            inventory.push(Cache {
                                level: u8::try_from(level).unwrap_or(0),
                                bytes: size,
                                line_bytes: line,
                                cpus: shared.clone(),
                            });
                        }
                        domains.push(CacheDomain {
                            level,
                            bytes: size,
                            shared: shared.unwrap_or_else(|| vec![cpu]),
                        });
                    }
                }
            }
        }
        preferred = representatives(&cpus, &groups);
        cache_total_bytes = total_cache(&cpus, &domains).max(cache_bytes);
        caches = visible_caches(&cpus, inventory);
        page_size = usize::try_from(unsafe { native::getpagesize() })
            .unwrap_or(4096)
            .max(1);
        affinity = "hard";
    }
    #[cfg(target_os = "macos")]
    {
        // This determines concurrency; these IDs cannot request hard pinning.
        let physical = native::value(b"hw.physicalcpu\0");
        if physical > 0 {
            preferred.truncate(physical.min(cpus.len()));
        }
        cache_bytes = cache_bytes
            .max(native::value(b"hw.l3cachesize\0"))
            .max(native::value(b"hw.l2cachesize\0"));
        // Apple documents per-performance-level capacities and sharing counts:
        // developer.apple.com/documentation/kernel/1387446-sysctlbyname/determining_system_capabilities
        let levels = native::value(b"hw.nperflevels\0").min(16);
        let line = native::value(b"hw.cachelinesize\0");
        let mut inventory = Vec::new();
        for (level, name) in [
            (1, b"hw.l1dcachesize\0" as &[u8]),
            (2, b"hw.l2cachesize\0"),
            (3, b"hw.l3cachesize\0"),
        ] {
            let bytes = native::value(name);
            if bytes != 0 {
                inventory.push(Cache {
                    level,
                    bytes,
                    line_bytes: line,
                    cpus: Vec::new(),
                });
            }
        }
        let (mut total, mut accounted) = (0usize, 0usize);
        let mut complete = levels > 0;
        for level in 0..levels {
            let query =
                |suffix| native::value(format!("hw.perflevel{level}.{suffix}\0").as_bytes());
            let cores = query("physicalcpu");
            let (l2, l3) = (query("l2cachesize"), query("l3cachesize"));
            for (level, bytes) in [(1, query("l1dcachesize")), (2, l2), (3, l3)] {
                if bytes != 0 {
                    inventory.push(Cache {
                        level,
                        bytes,
                        line_bytes: line,
                        cpus: Vec::new(),
                    });
                }
            }
            cache_bytes = cache_bytes.max(l2).max(l3);
            let (size, sharing) = if l3 > 0 {
                (l3, query("cpusperl3"))
            } else {
                (l2, query("cpusperl2"))
            };
            if cores > 0 && size > 0 && sharing > 0 {
                total = total.saturating_add(size.saturating_mul(cores.div_ceil(sharing)));
                accounted = accounted.saturating_add(cores);
            } else {
                complete = false;
            }
        }
        cache_total_bytes = if complete && accounted == preferred.len() {
            total.max(cache_bytes)
        } else {
            cache_bytes.saturating_mul(preferred.len())
        };
        cache_line_bytes = cache_line_bytes.max(line);
        // Unique reported size options, not a fabricated list of physical domains.
        inventory.sort_unstable_by_key(|c| (c.level, c.bytes, c.line_bytes));
        inventory.dedup_by_key(|c| (c.level, c.bytes, c.line_bytes));
        caches = inventory;
        page_size = usize::try_from(unsafe { native::getpagesize() })
            .unwrap_or(4096)
            .max(1);
    }
    Hardware {
        cpus,
        preferred,
        caches,
        cache_bytes,
        cache_total_bytes,
        cache_line_bytes,
        memory_budget: memory_budget(),
        affinity,
        page_size,
    }
}

#[cfg(any(windows, test))]
fn windows_memory_budget(physical: u64, commit: u64, address_space: u64) -> usize {
    (physical.min(commit).min(address_space) / 4).min(usize::MAX as u64) as usize
}

#[cfg(any(target_os = "linux", test))]
fn linux_memory_budget(available: Option<usize>, cgroup: Option<usize>) -> usize {
    [available, cgroup].into_iter().flatten().min().unwrap_or(0) / 4
}

#[cfg(any(target_os = "linux", test))]
fn cgroup_headroom(
    groups: &str,
    mounts: &str,
    mut read_number: impl FnMut(&std::path::Path) -> Option<usize>,
) -> Option<usize> {
    use std::path::{Component, Path, PathBuf};
    // Decode the mountinfo escapes, leaving an escaped literal backslash last.
    let path = |text: &str| -> PathBuf {
        text.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
            .into()
    };
    let mut headroom = None;
    // A controller belongs to only one hierarchy; a hybrid v2 mount cannot
    // impose memory limits while the memory controller is attached to v1.
    let v1_memory = groups.lines().any(|line| {
        let mut parts = line.splitn(3, ':');
        parts.next();
        let controllers = parts.next().unwrap_or("");
        parts.next().is_some() && controllers.split(',').any(|c| c == "memory")
    });
    // A namespace can inherit a mount whose root is outside its coordinates.
    // Track mapping separately from a finite limit: a mapped unlimited mount
    // still resolves the memory hierarchy.
    let mut unresolved = false;
    let mut resolved = false;
    for mount in mounts.lines() {
        let Some((fields, details)) = mount.split_once(" - ") else {
            continue;
        };
        let mut details = details.split_whitespace();
        let kind = details.next();
        details.next(); // mount source
        let controllers = details.next().unwrap_or("");
        let v2 = kind == Some("cgroup2");
        if v2 && v1_memory {
            continue;
        }
        if !v2 && !(kind == Some("cgroup") && controllers.split(',').any(|c| c == "memory")) {
            continue;
        }
        let controller = if v2 { "" } else { "memory" };
        let Some(group) = groups.lines().find_map(|line| {
            let mut parts = line.splitn(3, ':');
            parts.next()?;
            let controllers = parts.next()?;
            let group = parts.next()?;
            controllers
                .split(',')
                .any(|c| c == controller)
                .then_some(group)
        }) else {
            continue;
        };
        let mut fields = fields.split_whitespace().skip(3);
        let (Some(root), Some(base)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (root, base, group) = (path(root), path(base), Path::new(group));
        let parent = |p: &Path| p.components().any(|c| c == Component::ParentDir);
        if [&root, &base, group].iter().any(|p| !p.has_root()) || parent(&base) {
            continue;
        }
        if parent(&root) || parent(group) {
            unresolved = true;
            continue;
        }
        let Ok(relative) = group.strip_prefix(&root) else {
            continue;
        };
        resolved = true;
        let mut directory = base.join(relative);
        let (limit, used) = if v2 {
            ("memory.max", "memory.current")
        } else {
            ("memory.limit_in_bytes", "memory.usage_in_bytes")
        };
        loop {
            if let (Some(limit), Some(used)) = (
                read_number(&directory.join(limit)),
                read_number(&directory.join(used)),
            ) {
                let available = limit.saturating_sub(used);
                headroom = Some(headroom.map_or(available, |old: usize| old.min(available)));
            }
            if directory == base || !directory.pop() {
                break;
            }
        }
    }
    if unresolved && !resolved {
        // Falling back to host MemAvailable could exceed the hidden cgroup cap.
        Some(0)
    } else {
        headroom
    }
}

/// Conservative allocation allowance. macOS uses total capacity/8; Windows and
/// Linux use available memory/4. Windows also caps by process commit and virtual
/// address headroom. Zero means no usable budget was detected. This is a sizing
/// heuristic, not a reservation.
pub fn memory_budget() -> usize {
    #[cfg(windows)]
    {
        let mut status = native::MemoryStatus {
            length: size_of::<native::MemoryStatus>() as u32,
            ..Default::default()
        };
        if unsafe { native::GlobalMemoryStatusEx(&mut status) } != 0 {
            windows_memory_budget(
                status.available,
                status.available_pagefile,
                status.available_virtual,
            )
        } else {
            0
        }
    }
    #[cfg(target_os = "linux")]
    {
        let available = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|data| {
                data.lines().find_map(|line| {
                    line.strip_prefix("MemAvailable:")
                        .and_then(|rest| rest.split_whitespace().next()?.parse::<usize>().ok())
                        .and_then(|kb| kb.checked_mul(1024))
                })
            });
        // Resolve independently: a cgroup cap remains useful without MemAvailable.
        let groups = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let cgroup = cgroup_headroom(&groups, &mounts, |path| {
            std::fs::read_to_string(path).ok()?.trim().parse().ok()
        });
        linux_memory_budget(available, cgroup)
    }
    #[cfg(target_os = "macos")]
    {
        native::value(b"hw.memsize\0") / 8
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_reports_available_core_representatives() {
        let hw = hardware();
        assert!(!hw.cpus.is_empty() && !hw.preferred.is_empty());
        assert!(hw.preferred.iter().all(|id| hw.cpus.contains(id)));
        assert!(hw.page_size > 0);
        assert!(hw.cache_total_bytes >= hw.cache_bytes);
        assert!(hw.caches.iter().all(|cache| (1..=3).contains(&cache.level)
            && cache.bytes != 0
            && cache.cpus.iter().all(|id| hw.cpus.contains(id))));
        #[cfg(any(windows, target_os = "linux"))]
        assert!(hw.caches.iter().all(|cache| !cache.cpus.is_empty()));
        #[cfg(target_os = "macos")]
        assert!(hw.caches.iter().all(|cache| cache.cpus.is_empty()));
    }
    #[test]
    fn cache_inventory_groups_domains_and_filters_accessible_cpus() {
        let cache = |level, bytes, line_bytes, cpus| Cache {
            level,
            bytes,
            line_bytes,
            cpus,
        };
        let inventory = visible_caches(
            &[5, 1],
            vec![
                cache(2, 512 << 10, 64, vec![0, 1]),
                cache(2, 512 << 10, 0, vec![1, 0, 1]),
                cache(3, 32 << 20, 128, vec![5, 4, 1, 0]),
                cache(1, 32 << 10, 64, vec![4, 5]),
                cache(1, 32 << 10, 64, vec![7]),
                cache(4, 128 << 20, 64, vec![1]),
                cache(2, 0, 64, vec![1]),
                cache(2, 1 << 20, 64, vec![]),
            ],
        );
        let values: Vec<_> = inventory
            .into_iter()
            .map(|c| (c.level, c.bytes, c.line_bytes, c.cpus))
            .collect();
        assert_eq!(
            values,
            vec![
                (1, 32 << 10, 64, vec![5]),
                (2, 512 << 10, 64, vec![1]),
                (3, 32 << 20, 128, vec![1, 5])
            ]
        );
    }
    #[test]
    fn last_level_domains_deduplicate_and_respect_allowed_cpus() {
        let mut domains = Vec::new();
        for core in 0..8 {
            let shared = vec![core * 2, core * 2 + 1];
            domains.push(CacheDomain {
                level: 2,
                bytes: 1 << 20,
                shared: shared.clone(),
            });
            domains.push(CacheDomain {
                level: 3,
                bytes: 32 << 20,
                shared: shared.clone(),
            });
            // Linux exposes the same shared cache beneath each sibling's sysfs.
            domains.push(CacheDomain {
                level: 3,
                bytes: 32 << 20,
                shared: shared.into_iter().rev().collect(),
            });
        }
        // An unavailable higher-level domain must not hide the available LLCs.
        domains.push(CacheDomain {
            level: 4,
            bytes: 512 << 20,
            shared: vec![90, 91],
        });
        assert_eq!(
            total_cache(&[0, 2, 4, 6, 8, 10, 12, 14], &domains),
            256 << 20
        );
        assert_eq!(total_cache(&[1, 3], &domains), 64 << 20);
        assert_eq!(total_cache(&[], &domains), 0);
    }
    #[test]
    fn linux_lists_and_cache_sizes() {
        assert_eq!(
            cpu_list("0-3,7,9-11,2\n"),
            Some(vec![0, 1, 2, 3, 7, 9, 10, 11])
        );
        for bad in ["", "3-1", "0--2", "2,", "65536", "0-999999999", "1-2-3"] {
            assert_eq!(cpu_list(bad), None);
        }
        assert_eq!(cache_size("96M\n"), Some(96 << 20));
        assert_eq!(cache_size("1024K"), Some(1 << 20));
        assert_eq!(cache_size("4096"), Some(4096));
        assert_eq!(cache_size("18446744073709551615G"), None);
    }
    #[test]
    fn restricted_smt_groups() {
        let groups = vec![vec![0, 1], vec![2, 3], vec![4, 5]];
        assert_eq!(representatives(&[1, 2, 3, 5, 8], &groups), vec![1, 2, 5, 8]);
    }
    #[test]
    fn windows_multiple_groups_and_malformed_records() {
        let mut record = vec![0u8; 64];
        record[4..8].copy_from_slice(&64u32.to_le_bytes());
        record[30..32].copy_from_slice(&2u16.to_le_bytes());
        record[32..40].copy_from_slice(&3u64.to_le_bytes());
        record[48..56].copy_from_slice(&0x8000000000000080u64.to_le_bytes());
        record[56..58].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(windows_masks(&record, 30, 32), Some(vec![0, 1, 135, 191]));
        assert_eq!(windows_masks(&record[..56], 30, 32), None);
        assert_eq!(windows_records(&record[..63]).count(), 0);
        record[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(windows_records(&record).count(), 0);
        let mut legacy_cache = vec![0u8; 56];
        legacy_cache[40..48].copy_from_slice(&0x0cu64.to_le_bytes());
        legacy_cache[48..50].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(windows_masks(&legacy_cache, 38, 40), Some(vec![66, 67]));
    }
    #[test]
    fn windows_cache_records_include_data_and_exclude_instructions() {
        let mut record = vec![0u8; 56];
        record[0..4].copy_from_slice(&2u32.to_le_bytes());
        record[4..8].copy_from_slice(&56u32.to_le_bytes());
        record[8] = 2;
        record[10..12].copy_from_slice(&128u16.to_le_bytes());
        record[12..16].copy_from_slice(&(512u32 << 10).to_le_bytes());
        record[16..20].copy_from_slice(&2u32.to_le_bytes());
        record[38..40].copy_from_slice(&1u16.to_le_bytes());
        record[40..48].copy_from_slice(&3u64.to_le_bytes());
        let parsed = windows_records(&record).next().unwrap();
        let (data, line) = windows_cache(parsed, &[1]).unwrap();
        assert_eq!((data.level, data.bytes, line), (2, 512 << 10, 128));
        assert_eq!(data.shared, vec![0, 1]);
        assert!(windows_cache(parsed, &[7]).is_none());
        record[16..20].copy_from_slice(&1u32.to_le_bytes());
        assert!(windows_cache(&record, &[1]).is_none());
        record[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert!(windows_cache(&record, &[1]).is_some());
        record[10..12].fill(0);
        assert_eq!(windows_cache(&record, &[1]).unwrap().1, 0);
    }
    #[test]
    fn windows_budget_respects_process_headroom() {
        assert_eq!(windows_memory_budget(8 << 30, 128 << 20, 1 << 30), 32 << 20);
        assert_eq!(windows_memory_budget(8 << 30, 1 << 30, 64 << 20), 16 << 20);
        assert_eq!(windows_memory_budget(64 << 20, 1 << 30, 8 << 30), 16 << 20);
        assert_eq!(windows_memory_budget(8 << 30, 0, 1 << 30), 0);
        assert_eq!(
            windows_memory_budget(u64::MAX, u64::MAX, u64::MAX),
            (u64::MAX / 4).min(usize::MAX as u64) as usize
        );
    }
    #[test]
    fn linux_budget_uses_known_caps_without_assuming_unknown_memory() {
        assert_eq!(linux_memory_budget(None, None), 0);
        assert_eq!(linux_memory_budget(None, Some(32 << 20)), 8 << 20);
        assert_eq!(linux_memory_budget(Some(64 << 20), None), 16 << 20);
        assert_eq!(linux_memory_budget(Some(64 << 20), Some(32 << 20)), 8 << 20);
        assert_eq!(linux_memory_budget(Some(64 << 20), Some(0)), 0);
    }
    #[test]
    fn cgroup_mount_roots_and_ancestor_limits() {
        let mounts = "29 23 0:25 /tenant /limits\\040v2 rw shared:3 - cgroup2 cgroup rw";
        let values = BTreeMap::from([
            ("/limits v2/job/memory.max", 200),
            ("/limits v2/job/memory.current", 20),
            ("/limits v2/memory.max", 100),
            ("/limits v2/memory.current", 30),
        ]);
        let read = |path: &std::path::Path| {
            values
                .iter()
                .find_map(|(p, n)| (path == std::path::Path::new(p)).then_some(*n))
        };
        assert_eq!(cgroup_headroom("0::/tenant/job", mounts, read), Some(70));

        let mounts = "30 23 0:26 /docker/x /custom/memory rw - cgroup cgroup rw,cpu,memory";
        let values = BTreeMap::from([
            ("/custom/memory/leaf/memory.limit_in_bytes", 200),
            ("/custom/memory/leaf/memory.usage_in_bytes", 25),
            ("/custom/memory/memory.limit_in_bytes", 300),
            ("/custom/memory/memory.usage_in_bytes", 200),
        ]);
        let read = |path: &std::path::Path| {
            values
                .iter()
                .find_map(|(p, n)| (path == std::path::Path::new(p)).then_some(*n))
        };
        assert_eq!(
            cgroup_headroom("2:cpu,memory:/docker/x/leaf", mounts, read),
            Some(100)
        );

        let mounts = "29 23 0:25 / /sys/fs/cgroup rw - cgroup2 cgroup rw";
        let read = |path: &std::path::Path| {
            if path == std::path::Path::new("/sys/fs/cgroup/sub/memory.max") {
                Some(12)
            } else if path == std::path::Path::new("/sys/fs/cgroup/sub/memory.current") {
                Some(13)
            } else {
                None
            }
        };
        assert_eq!(cgroup_headroom("0::/sub", mounts, read), Some(0));
        assert_eq!(
            cgroup_headroom("0::/../outside", mounts, |_| panic!(
                "outside visible mount"
            )),
            Some(0)
        );
        assert_eq!(
            cgroup_headroom("0::/sub", "malformed mount", |_| panic!("invalid mount")),
            None
        );
    }
    #[test]
    fn inherited_namespace_mount_requires_a_usable_mapping() {
        let inherited = "29 23 0:25 /.. /sys/fs/cgroup rw - cgroup2 cgroup rw";
        let unknown = cgroup_headroom("0::/", inherited, |_| panic!("unresolved path"));
        assert_eq!(unknown, Some(0));
        assert_eq!(linux_memory_budget(Some(8 << 30), unknown), 0);

        let unrelated = "30 23 0:25 /other /other-mount rw - cgroup2 cgroup rw";
        assert_eq!(
            cgroup_headroom("0::/", unrelated, |_| panic!("outside mount")),
            None
        );
        assert_eq!(
            cgroup_headroom("0::/", &format!("{inherited}\n{unrelated}"), |_| panic!(
                "unmapped"
            )),
            Some(0)
        );
    }
    #[test]
    fn alternate_mapping_resolves_finite_or_unlimited_namespace_budget() {
        let inherited = "29 23 0:25 /.. /old rw - cgroup2 cgroup rw";
        let mapped = "30 23 0:25 / /resolved rw - cgroup2 cgroup rw";
        for mounts in [
            format!("{inherited}\n{mapped}"),
            format!("{mapped}\n{inherited}"),
        ] {
            let finite = cgroup_headroom("0::/", &mounts, |path| {
                if path == std::path::Path::new("/resolved/memory.max") {
                    Some(128 << 20)
                } else if path == std::path::Path::new("/resolved/memory.current") {
                    Some(32 << 20)
                } else {
                    panic!("unexpected path: {}", path.display())
                }
            });
            assert_eq!(finite, Some(96 << 20));
            assert_eq!(linux_memory_budget(Some(8 << 30), finite), 24 << 20);
            // The numeric reader returns None for the unlimited value "max".
            let unlimited = cgroup_headroom("0::/", &mounts, |_| None);
            assert_eq!(unlimited, None);
            assert_eq!(linux_memory_budget(Some(8 << 30), unlimited), 2 << 30);
        }
    }
    #[test]
    fn unified_mapping_does_not_hide_unresolved_v1_memory_hierarchy() {
        let mounts = "29 23 0:25 /.. /old rw - cgroup cgroup rw,memory\n30 23 0:26 / /unified rw - cgroup2 cgroup rw";
        assert_eq!(
            cgroup_headroom("2:memory:/\n0::/", mounts, |_| None),
            Some(0)
        );
    }
    #[test]
    fn v1_memory_ignores_unrelated_unresolved_unified_mapping() {
        let inherited = "29 23 0:25 /.. /unified rw - cgroup2 cgroup rw";
        let memory = "30 23 0:26 / /memory rw - cgroup cgroup rw,cpu,memory";
        for mounts in [
            format!("{inherited}\n{memory}"),
            format!("{memory}\n{inherited}"),
        ] {
            let headroom = cgroup_headroom("2:cpu,memory:/\n0::/", &mounts, |path| {
                if path == std::path::Path::new("/memory/memory.limit_in_bytes") {
                    Some(128 << 20)
                } else if path == std::path::Path::new("/memory/memory.usage_in_bytes") {
                    Some(32 << 20)
                } else {
                    panic!("unexpected path: {}", path.display())
                }
            });
            assert_eq!(headroom, Some(96 << 20));
        }
        // A named hierarchy containing "memory" is not the memory controller.
        assert_eq!(
            cgroup_headroom("2:name=memory:/\n0::/", inherited, |_| panic!("unresolved")),
            Some(0)
        );
    }
    #[test]
    fn affinity_round_trip() {
        let before = cpus();
        #[cfg(windows)]
        let mut saved = native::GroupAffinity::default();
        #[cfg(windows)]
        assert_ne!(
            unsafe { native::GetThreadGroupAffinity(native::GetCurrentThread(), &mut saved) },
            0
        );
        {
            let _pin = pin(before[0]).unwrap();
            #[cfg(windows)]
            {
                let mut pinned = native::GroupAffinity::default();
                assert_ne!(
                    unsafe {
                        native::GetThreadGroupAffinity(native::GetCurrentThread(), &mut pinned)
                    },
                    0
                );
                assert_eq!(pinned.group as usize, before[0] / 64);
                assert_eq!(pinned.mask, 1usize << (before[0] % 64));
            }
        }
        assert_eq!(cpus(), before);
        #[cfg(windows)]
        {
            let mut restored = native::GroupAffinity::default();
            assert_ne!(
                unsafe {
                    native::GetThreadGroupAffinity(native::GetCurrentThread(), &mut restored)
                },
                0
            );
            assert_eq!(saved, restored);
        }
    }
    #[cfg(windows)]
    #[test]
    fn memory_status_layout() {
        assert_eq!(size_of::<native::MemoryStatus>(), 64);
    }
}
