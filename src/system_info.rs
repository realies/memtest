//! Small startup-only display queries; these values do not change the benchmark.
use crate::platform::Hardware;
#[cfg(any(windows, target_os = "linux", test))]
#[path = "system_info/firmware.rs"]
mod firmware;

pub struct Info {
    pub cpu: String,
    pub memory: String,
    pub modules: String,
    pub board: String,
    pub bios: String,
    pub os: String,
}

impl Info {
    fn with_placeholders(mut self) -> Self {
        for value in [
            &mut self.memory,
            &mut self.modules,
            &mut self.board,
            &mut self.bios,
            &mut self.os,
        ] {
            if value.trim().is_empty() {
                *value = "N/A".into();
            }
        }
        self
    }
}

pub fn detect(hardware: &Hardware) -> Info {
    let cpu = cpu_model().unwrap_or_else(|| match std::env::consts::ARCH {
        "aarch64" => "ARM64".into(),
        "x86_64" => "x86-64".into(),
        other => other.into(),
    });
    let memory = if let Some(total) = total_memory().filter(|&n| n != 0) {
        #[cfg(target_os = "macos")]
        let label = {
            let mut arm64 = [0u8; 4];
            if sysctl(b"hw.optional.arm64\0", &mut arm64) == Some(4)
                && u32::from_ne_bytes(arm64) == 1
            {
                "unified memory"
            } else {
                "RAM"
            }
        };
        #[cfg(not(target_os = "macos"))]
        let label = "OS usable RAM";
        format!("{} {label}", capacity(total))
    } else if hardware.memory_budget != 0 {
        format!(
            "{} benchmark allowance",
            capacity(hardware.memory_budget as u64)
        )
    } else {
        "N/A".into()
    };
    #[cfg(any(windows, target_os = "linux"))]
    let details = firmware::detect();
    #[cfg(target_os = "macos")]
    let board = {
        let mut data = [0u8; 128];
        sysctl(b"hw.model\0", &mut data)
            .and_then(|size| std::str::from_utf8(&data[..size]).ok())
            .and_then(name)
            .unwrap_or_default()
    };
    Info {
        cpu,
        #[cfg(any(windows, target_os = "linux"))]
        memory: if details.memory.is_empty() {
            memory
        } else {
            details.memory
        },
        #[cfg(not(any(windows, target_os = "linux")))]
        memory,
        #[cfg(any(windows, target_os = "linux"))]
        modules: details.modules,
        #[cfg(not(any(windows, target_os = "linux")))]
        modules: String::new(),
        #[cfg(any(windows, target_os = "linux"))]
        board: details.board,
        #[cfg(target_os = "macos")]
        board,
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        board: String::new(),
        #[cfg(any(windows, target_os = "linux"))]
        bios: details.bios,
        #[cfg(not(any(windows, target_os = "linux")))]
        bios: String::new(),
        os: os_name(),
    }
    .with_placeholders()
}

fn name(text: &str) -> Option<String> {
    let text = text
        .split('\0')
        .next()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn capacity(bytes: u64) -> String {
    if bytes != 0 && bytes.is_multiple_of(1 << 30) {
        format!("{} GiB", bytes >> 30)
    } else if bytes >= 1 << 30 {
        format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes != 0 && bytes.is_multiple_of(1 << 20) {
        format!("{} MiB", bytes >> 20)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{bytes} B")
    }
}

fn cpu_model() -> Option<String> {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        if __cpuid(0x80000000).eax >= 0x80000004 {
            let mut brand = Vec::with_capacity(48);
            for leaf in 0x80000002..=0x80000004 {
                let value = __cpuid(leaf);
                for word in [value.eax, value.ebx, value.ecx, value.edx] {
                    brand.extend_from_slice(&word.to_le_bytes());
                }
            }
            if let Some(brand) = String::from_utf8(brand).ok().and_then(|s| name(&s)) {
                return Some(brand);
            }
        }
    }
    #[cfg(windows)]
    {
        registry_string(
            "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
            "ProcessorNameString",
        )
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()?
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| name(value)).flatten()
            })
    }
    #[cfg(target_os = "macos")]
    {
        let mut data = [0u8; 256];
        let size = sysctl(b"machdep.cpu.brand_string\0", &mut data)?;
        name(std::str::from_utf8(&data[..size]).ok()?)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn os_name() -> String {
    #[cfg(windows)]
    let os = windows_os();
    #[cfg(target_os = "linux")]
    let os = {
        let release = std::fs::read_to_string("/etc/os-release")
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"));
        let mut os = release
            .ok()
            .and_then(|s| pretty_name(&s))
            .unwrap_or_else(|| "Linux".into());
        if let Some(kernel) = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .and_then(|s| name(&s))
        {
            os.push_str(&format!(" · kernel {kernel}"));
        }
        os
    };
    #[cfg(target_os = "macos")]
    let os = {
        let mut data = [0u8; 128];
        let version = sysctl(b"kern.osproductversion\0", &mut data)
            .and_then(|size| std::str::from_utf8(&data[..size]).ok())
            .and_then(name);
        version.map_or_else(|| "macOS".into(), |v| format!("macOS {v}"))
    };
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let os = std::env::consts::OS.to_owned();
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "ARM64",
        other => other,
    };
    format!("{os} · {arch}")
}

#[cfg(any(target_os = "linux", test))]
fn pretty_name(release: &str) -> Option<String> {
    let value = release
        .lines()
        .filter_map(|s| s.split_once('='))
        .find(|(key, _)| key.trim() == "PRETTY_NAME")?
        .1
        .trim();
    // os-release permits shell quoting/escaping but no variable expansion.
    let (value, single_quoted) = if value.starts_with('\'') || value.starts_with('"') {
        let quote = value.as_bytes()[0] as char;
        (
            value.strip_prefix(quote)?.strip_suffix(quote)?,
            quote == '\'',
        )
    } else {
        (value, false)
    };
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && !single_quoted
            && chars
                .peek()
                .is_some_and(|c| matches!(c, '\\' | '"' | '$' | '`'))
        {
            out.push(chars.next()?);
        } else {
            out.push(c);
        }
    }
    name(&out)
}

#[cfg(windows)]
fn windows_os() -> String {
    const KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
    #[repr(C)]
    struct Version {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
        service_pack_major: u16,
        service_pack_minor: u16,
        suite: u16,
        product_type: u8,
        reserved: u8,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut Version) -> i32;
    }
    let mut version: Version = unsafe { std::mem::zeroed() };
    version.size = size_of::<Version>() as u32;
    let native = unsafe { RtlGetVersion(&mut version) } == 0;
    let mut product = registry_string(KEY, "ProductName").unwrap_or_else(|| {
        registry_string(KEY, "EditionID")
            .map_or_else(|| "Windows".into(), |v| format!("Windows {v}"))
    });
    // Windows 11 can retain a Windows 10 registry name. Do not relabel servers.
    if native
        && version.major == 10
        && version.build >= 22000
        && version.product_type == 1
        && let Some(edition) = product.strip_prefix("Windows 10")
    {
        product = format!("Windows 11{edition}");
    }
    if let Some(display) = registry_string(KEY, "DisplayVersion") {
        product.push_str(&format!(" {display}"));
    }
    let build = if native {
        Some(version.build)
    } else {
        registry_string(KEY, "CurrentBuildNumber").and_then(|s| s.parse::<u32>().ok())
    };
    if let Some(build) = build {
        product.push_str(&format!(" · Build {build}"));
        let mut revision = 0u32;
        if registry_value(KEY, "UBR", 0x10, (&mut revision as *mut u32).cast(), 4) == Some(4) {
            product.push_str(&format!(".{revision}"));
        }
    }
    product
}

#[cfg(windows)]
fn registry_string(key: &str, value: &str) -> Option<String> {
    let mut data = [0u16; 256];
    let bytes = registry_value(
        key,
        value,
        2,
        data.as_mut_ptr().cast(),
        size_of_val(&data) as u32,
    )?;
    if bytes.is_multiple_of(2) {
        name(&String::from_utf16_lossy(&data[..bytes as usize / 2]))
    } else {
        None
    }
}

#[cfg(windows)]
fn registry_value(
    key: &str,
    value: &str,
    flags: u32,
    data: *mut std::ffi::c_void,
    capacity: u32,
) -> Option<u32> {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(
            key: *mut c_void,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            bytes: *mut u32,
        ) -> i32;
    }
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut bytes = capacity;
    // HKEY_LOCAL_MACHINE is the sign-extended predefined handle.
    let status = unsafe {
        RegGetValueW(
            (-2147483646isize) as *mut c_void,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            data,
            &mut bytes,
        )
    };
    (status == 0 && bytes <= capacity).then_some(bytes)
}

fn total_memory() -> Option<u64> {
    #[cfg(windows)]
    {
        crate::platform::total_memory()
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()?
            .lines()
            .find_map(|line| {
                line.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(1024)
            })
    }
    #[cfg(target_os = "macos")]
    {
        let mut data = [0u8; 8];
        (sysctl(b"hw.memsize\0", &mut data)? == 8).then(|| u64::from_ne_bytes(data))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn sysctl(name: &[u8], data: &mut [u8]) -> Option<usize> {
    use std::ffi::c_void;
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const u8,
            out: *mut c_void,
            bytes: *mut usize,
            new: *const c_void,
            new_bytes: usize,
        ) -> i32;
    }
    let mut size = data.len();
    if unsafe {
        sysctlbyname(
            name.as_ptr(),
            data.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null(),
            0,
        )
    } == 0
        && size <= data.len()
    {
        Some(size)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Info, pretty_name};

    #[test]
    fn missing_hardware_display_fields_use_fixed_placeholders() {
        let info = Info {
            cpu: "ARM64".into(),
            memory: String::new(),
            modules: " \t".into(),
            board: String::new(),
            bios: "\n".into(),
            os: String::new(),
        }
        .with_placeholders();
        assert_eq!(info.cpu, "ARM64");
        for value in [
            &info.memory,
            &info.modules,
            &info.board,
            &info.bios,
            &info.os,
        ] {
            assert_eq!(value, "N/A");
        }
    }

    #[test]
    fn available_hardware_values_and_system_model_identifiers_are_preserved() {
        for (model, modules, bios) in [
            (
                "ASUSTeK COMPUTER INC. ProArt X870E",
                "2 × Corsair DIMM",
                "American Megatrends 2103",
            ),
            ("Mac15,6", "", ""),
        ] {
            let info = Info {
                cpu: "Existing CPU".into(),
                memory: "64 GiB existing memory description".into(),
                modules: modules.into(),
                board: model.into(),
                bios: bios.into(),
                os: "Existing OS".into(),
            }
            .with_placeholders();
            assert_eq!(info.cpu, "Existing CPU");
            assert_eq!(info.memory, "64 GiB existing memory description");
            assert_eq!(info.board, model);
            assert_eq!(
                info.modules,
                if modules.is_empty() { "N/A" } else { modules }
            );
            assert_eq!(info.bios, if bios.is_empty() { "N/A" } else { bios });
            assert_eq!(info.os, "Existing OS");
        }
    }

    #[test]
    fn os_release_uses_display_name_without_shell_expansion() {
        assert_eq!(
            pretty_name("NAME=Ignored\nPRETTY_NAME=\"Example Linux 1.2\"\n"),
            Some("Example Linux 1.2".into())
        );
        assert_eq!(
            pretty_name(r#"PRETTY_NAME="Example \"Edition\" \$VAR""#),
            Some("Example \"Edition\" $VAR".into())
        );
        assert_eq!(
            pretty_name("PRETTY_NAME='Linux $VAR'"),
            Some("Linux $VAR".into())
        );
        assert_eq!(pretty_name("PRETTY_NAME=\"unterminated"), None);
    }
}
