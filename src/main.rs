mod cache;
mod cli;
#[cfg(all(feature = "gui", windows))]
mod desktop;
#[cfg(feature = "gui")]
mod gui;
mod json;
mod platform;
mod portable;
mod profile;
#[cfg(feature = "gui")]
mod system_info;
mod tune;

#[cfg(not(all(
    any(windows, target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("memtest supports Windows, Linux, and macOS on x86-64 and ARM64");

use profile::Spec;

fn calibrates_workers(options: &cli::Options) -> bool {
    matches!(options.level.as_str(), "all" | "memory")
        && options.test != "latency"
        && !options.quick
        && options.cpus.is_none()
        && options
            .threads
            .map_or(options.profile.is_none(), |count| count == 0)
}

fn placement(hardware: &platform::Hardware, count: usize) -> Result<Vec<usize>, String> {
    if count == 0 || count > hardware.cpus.len() {
        return Err(format!(
            "requested {count} workers, but only {} CPUs are available",
            hardware.cpus.len()
        ));
    }
    let mut ids = hardware.preferred.clone();
    for &cpu in &hardware.cpus {
        if !ids.contains(&cpu) {
            ids.push(cpu);
        }
    }
    ids.truncate(count);
    Ok(ids)
}

fn automatic_bytes(
    hardware: &platform::Hardware,
    desired: usize,
    alignment: usize,
    active: bool,
    streams: usize,
) -> Result<usize, String> {
    // Keep complete profile metadata when an unused buffer has no known budget.
    let budget = if !active && hardware.memory_budget == 0 {
        desired
    } else {
        hardware.memory_budget / streams
    };
    let bytes = desired.min(budget) / alignment * alignment;
    if bytes == 0 {
        return Err(
            "available memory is unknown or too small for automatic buffers; specify explicit sizes".into(),
        );
    }
    Ok(bytes)
}

fn resolve(
    options: &cli::Options,
    hardware: &platform::Hardware,
) -> Result<(Spec, Vec<usize>), String> {
    portable::Backend::resolve(&options.isa)?;
    let saved = options.profile.as_deref().map(Spec::load).transpose()?;
    let requested = options
        .threads
        .or(saved.as_ref().map(|s| s.threads))
        .unwrap_or(0);
    let threads = if requested != 0 {
        requested
    } else if let Some(ids) = &options.cpus {
        ids.len()
    } else if options.quick || options.test == "latency" {
        1
    } else {
        // Upper bound for the same calibration procedure on every host.
        hardware
            .preferred
            .len()
            .max(1)
            .min((hardware.memory_budget / (16 << 20)).max(1))
            .min(2048)
    };
    if !(1..=2048).contains(&threads) {
        return Err("worker count must be between 1 and 2048".into());
    }
    let cache_threads =
        if let Some(ids) = options.cpus.as_ref().filter(|_| options.test != "latency") {
            ids.len()
        } else if let Some(count) = options.threads.filter(|&n| n != 0) {
            count
        } else if options.quick {
            1
        } else if let Some(saved) = saved.as_ref().filter(|_| options.threads != Some(0)) {
            saved.cache_threads
        } else {
            hardware.preferred.len().clamp(1, 2048)
        };
    // A latency-only run needs one CPU even when a profile saves more
    // bandwidth workers than this host provides.
    let memory = matches!(options.level.as_str(), "all" | "memory");
    let active_threads = if options.test == "latency" {
        1
    } else if !memory {
        cache_threads
    } else {
        threads
    };
    let mut cpus = if let Some(ids) = &options.cpus {
        if hardware.affinity == "scheduler" {
            return Err("--cpus requires hard CPU affinity, which is unavailable on macOS".into());
        }
        if ids.len() != active_threads {
            return Err("--cpus must contain exactly one ID per active worker".into());
        }
        if ids.iter().any(|id| !hardware.cpus.contains(id)) {
            return Err("--cpus contains a CPU unavailable to this process".into());
        }
        ids.clone()
    } else {
        placement(hardware, active_threads)?
    };
    let stride = options
        .stride
        .or(saved.as_ref().map(|s| s.stride))
        .unwrap_or(128);
    if stride < 8 || !stride.is_power_of_two() {
        return Err("--stride must be a power of two of at least 8 bytes".into());
    }
    let alignment = 128;
    let explicit_bandwidth = options.size.or(saved.as_ref().map(|s| s.bandwidth_bytes));
    let bandwidth_bytes = match explicit_bandwidth {
        Some(bytes) if bytes != 0 => bytes,
        _ => automatic_bytes(
            hardware,
            if options.quick {
                64 << 20
            } else {
                (256 << 20).max(hardware.cache_total_bytes.saturating_mul(4))
            },
            alignment,
            memory && options.test != "latency",
            if matches!(options.test.as_str(), "all" | "copy") {
                2
            } else {
                1
            },
        )?,
    };
    let latency_bytes = match options
        .latency_size
        .or(saved.as_ref().map(|s| s.latency_bytes))
    {
        Some(bytes) if bytes != 0 => bytes,
        _ => automatic_bytes(
            hardware,
            if options.quick {
                16 << 20
            } else {
                (256 << 20).max(hardware.cache_bytes.saturating_mul(4))
            },
            stride,
            memory && (options.test == "all" || options.test == "latency"),
            1,
        )?,
    };
    let threads = if calibrates_workers(options) {
        threads.min((bandwidth_bytes / (16 << 20)).max(1))
    } else {
        threads
    };
    if memory {
        cpus.truncate(threads);
    }
    let spec = Spec {
        threads,
        cache_threads,
        stores: options
            .stores
            .or(saved.as_ref().map(|s| s.stores))
            .unwrap_or(portable::Stores::Streaming),
        bandwidth_bytes,
        latency_bytes,
        stride,
        window: options
            .window
            .or(saved.as_ref().map(|s| s.window))
            .unwrap_or(profile::DEFAULT_WINDOW.max(stride).min(latency_bytes)),
        seed: options.seed.or(saved.as_ref().map(|s| s.seed)).unwrap_or(1),
        samples: options
            .samples
            .or(saved.as_ref().map(|s| s.samples))
            .unwrap_or(if options.quick { 3 } else { 7 }),
        min_time: options
            .minimum
            .or(saved.as_ref().map(|s| s.min_time))
            .unwrap_or(if options.quick { 0.01 } else { 0.2 }),
    };
    spec.validate()?;
    if memory && matches!(options.test.as_str(), "all" | "copy") {
        spec.bandwidth_bytes
            .checked_mul(2)
            .ok_or("copy buffer size overflow")?;
    }
    Ok((spec, cpus))
}

fn cache_plans(
    options: &cli::Options,
    hardware: &platform::Hardware,
    spec: &Spec,
    cpus: &[usize],
) -> Result<Vec<cache::Plan>, String> {
    if options.level == "memory" {
        return Ok(Vec::new());
    }
    let candidates = if options.cpus.is_some() {
        cpus.to_vec()
    } else {
        let count = if options.test == "latency" {
            1
        } else {
            spec.cache_threads
        };
        placement(hardware, count)?
    };
    cache::plans(hardware, &candidates, &options.level)
}

pub(crate) enum Progress {
    Plan(Vec<cache::Plan>, Spec, Vec<usize>),
    Stage(&'static str),
    Test(u8, &'static str),
    Sample(u8, &'static str, usize, usize, f64),
    Result(u8, &'static str, portable::Measurement),
}

pub(crate) struct Report {
    pub hardware: platform::Hardware,
    pub spec: Spec,
    pub cpus: Vec<usize>,
    pub worker_selection: &'static str,
    pub backend: portable::Backend,
    pub results: json::Results,
    pub caches: Vec<cache::Result>,
}

/// Both frontends use the same allocation, selection, and measurement path.
pub(crate) fn measure(
    options: &cli::Options,
    mut update: impl FnMut(Progress),
) -> Result<Report, String> {
    let backend = portable::Backend::resolve(&options.isa)?;
    let hardware = platform::hardware();
    let (mut spec, mut cpus) = resolve(options, &hardware)?;
    let plans = cache_plans(options, &hardware, &spec, &cpus)?;
    let worker_selection = if calibrates_workers(options) {
        update(Progress::Stage("Selecting workers"));
        spec.threads = tune::workers(
            backend,
            spec.stores,
            &options.test,
            &cpus,
            spec.bandwidth_bytes,
        )?;
        cpus.truncate(spec.threads);
        spec.validate()?;
        "calibrated"
    } else {
        "fixed"
    };
    update(Progress::Plan(plans.clone(), spec.clone(), cpus.clone()));
    if let Some(path) = &options.save_profile {
        spec.save(path)?;
    }
    let memory = matches!(options.level.as_str(), "all" | "memory");
    let results = if memory {
        measure_row(options, backend, &cpus, cpus[0], &spec, 0, &mut update)?
    } else {
        json::Results::default()
    };
    let mut caches = Vec::new();
    for plan in plans {
        let cache_spec = Spec {
            threads: plan.cpus.len(),
            stores: portable::Stores::Cached,
            bandwidth_bytes: plan.bandwidth_bytes,
            latency_bytes: plan.working_set_bytes,
            stride: plan.stride,
            window: 0,
            ..spec.clone()
        };
        let results = measure_row(
            options,
            backend,
            &plan.cpus,
            plan.cpu,
            &cache_spec,
            plan.level,
            &mut update,
        )?;
        caches.push(cache::Result { plan, results });
    }
    if options.test == "latency" {
        cpus.truncate(1);
    }
    Ok(Report {
        hardware,
        spec,
        cpus,
        worker_selection,
        backend,
        results,
        caches,
    })
}

fn measure_row(
    options: &cli::Options,
    backend: portable::Backend,
    cpus: &[usize],
    latency_cpu: usize,
    spec: &Spec,
    level: u8,
    update: &mut impl FnMut(Progress),
) -> Result<json::Results, String> {
    let mut results = json::Results::default();
    for (kind, label, key) in [
        (portable::BandwidthKind::Read, "Read", "read"),
        (portable::BandwidthKind::Write, "Write", "write"),
        (portable::BandwidthKind::Copy, "Copy", "copy"),
    ] {
        if options.test != "all" && options.test != key {
            continue;
        }
        update(Progress::Test(level, label));
        // Cache Copy's source + destination must fit the same working set as
        // Read/Write; RAM Copy uses a full stream-sized buffer for each.
        let bytes = if level != 0 && matches!(kind, portable::BandwidthKind::Copy) {
            spec.bandwidth_bytes / 2
        } else {
            spec.bandwidth_bytes
        };
        let result = portable::bandwidth(
            backend,
            spec.stores,
            kind,
            cpus,
            bytes,
            spec.samples,
            spec.min_time,
            |index, value| update(Progress::Sample(level, label, index, spec.samples, value)),
        )?;
        match kind {
            portable::BandwidthKind::Read => results.read = Some(result),
            portable::BandwidthKind::Write => results.write = Some(result),
            portable::BandwidthKind::Copy => results.copy = Some(result),
        }
        update(Progress::Result(level, label, result));
    }
    if options.test == "all" || options.test == "latency" {
        update(Progress::Test(level, "Latency"));
        let result = portable::latency(
            spec.latency_bytes,
            spec.stride,
            spec.window,
            spec.seed,
            latency_cpu,
            spec.samples,
            spec.min_time,
            |index, value| {
                update(Progress::Sample(
                    level,
                    "Latency",
                    index,
                    spec.samples,
                    value,
                ))
            },
        )?;
        results.latency = Some(result);
        update(Progress::Result(level, "Latency", result));
    }
    Ok(results)
}

fn platform_name() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "windows" => "Windows",
        "linux" => "Linux",
        "macos" => "macOS",
        name => name,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "ARM64",
        name => name,
    };
    (os, arch)
}

pub(crate) fn report_text(report: &Report) -> String {
    use std::fmt::Write;
    let Report {
        hardware,
        spec,
        cpus,
        worker_selection,
        backend,
        results,
        caches,
    } = report;
    let (os, arch) = platform_name();
    let mut text = format!(
        "memtest | {os} {arch} | {} CPUs | {} | {} RAM stores\n\n",
        hardware.cpus.len(),
        backend.name(),
        spec.stores.name(*backend)
    );
    for (label, result) in [
        ("Read", results.read),
        ("Write", results.write),
        ("Copy", results.copy),
    ] {
        if let Some(result) = result {
            writeln!(
                text,
                "{label:<7} {:>10.2} MB/s  {:>5.1}% range / {} {} / {:.1} MiB",
                result.value,
                result.spread_percent,
                spec.threads,
                if spec.threads == 1 {
                    "thread"
                } else {
                    "threads"
                },
                spec.bandwidth_bytes as f64 / (1 << 20) as f64
            )
            .unwrap();
        }
    }
    if let Some(result) = results.latency {
        let window = if spec.window == 0 {
            "full shuffle".into()
        } else if spec.window.is_multiple_of(1024) {
            format!("{} KiB random blocks", spec.window / 1024)
        } else {
            format!("{} B random blocks", spec.window)
        };
        let cpu = if hardware.affinity == "scheduler" {
            String::new()
        } else {
            format!(" / CPU {}", cpus[0])
        };
        writeln!(
            text,
            "{:<7} {:>10.3} ns    {:>5.1}% range / {:.1} MiB / {} B stride / {window}{cpu}",
            "Latency",
            result.value,
            result.spread_percent,
            spec.latency_bytes as f64 / (1 << 20) as f64,
            spec.stride
        )
        .unwrap();
    }
    if !caches.is_empty() {
        writeln!(
            text,
            "\nCache       Read GB/s    Write GB/s     Copy GB/s    Latency ns   Workers / BW / latency"
        )
        .unwrap();
        for row in caches {
            let rate = |value: Option<portable::Measurement>, divisor: f64| {
                value.map_or_else(|| "—".into(), |v| format!("{:.2}", v.value / divisor))
            };
            writeln!(
                text,
                "L{:<5} {:>13} {:>13} {:>13} {:>13}   {} / {} KiB / {} KiB",
                row.plan.level,
                rate(row.results.read, 1000.0),
                rate(row.results.write, 1000.0),
                rate(row.results.copy, 1000.0),
                rate(row.results.latency, 1.0),
                row.plan.cpus.len(),
                row.plan.bandwidth_bytes / 1024,
                row.plan.working_set_bytes / 1024
            )
            .unwrap();
        }
        writeln!(
            text,
            "Caches: aggregate cached bandwidth; single-worker latency, one pointer per cache line."
        )
        .unwrap();
    }
    if results.copy.is_some() || caches.iter().any(|c| c.results.copy.is_some()) {
        writeln!(text, "Copy counts bytes read + written; RAM uses two stream-sized buffers, caches split their working set.").unwrap();
    }
    writeln!(
        text,
        "\nMethod {} | {} samples / {:.3}s target | seed {} | workers {worker_selection}",
        profile::METHOD,
        spec.samples,
        spec.min_time,
        spec.seed
    )
    .unwrap();
    let affinity = match hardware.affinity {
        "hard" => "pinned",
        "hard/current-group" => "pinned, current group",
        _ => "OS scheduling",
    };
    let placement = if hardware.affinity == "scheduler" {
        format!(
            "{} {}",
            cpus.len(),
            if cpus.len() == 1 { "worker" } else { "workers" }
        )
    } else {
        format!(
            "CPUs {}",
            cpus.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let cache_line = if hardware.cache_line_bytes == 0 {
        "unknown".into()
    } else {
        format!("{} B", hardware.cache_line_bytes)
    };
    writeln!(
        text,
        "{placement} | {affinity} | {} KiB base pages / {cache_line} cache lines",
        hardware.page_size / 1024
    )
    .unwrap();
    text
}

pub(crate) fn report_json(report: &Report) -> Result<String, String> {
    json::render(
        &report.spec,
        &report.hardware,
        &report.cpus,
        report.worker_selection,
        report.backend,
        &report.results,
        &report.caches,
    )
}

fn run(options: cli::Options) -> Result<(), String> {
    let report = measure(&options, |progress| match progress {
        Progress::Plan(_plans, _spec, _cpus) => {}
        Progress::Stage(_stage) => {}
        Progress::Test(_level, _name) => {}
        Progress::Sample(_level, _name, _index, _count, _value) => {}
        Progress::Result(_level, _name, _result) => {}
    })?;
    if options.json {
        print!("{}", report_json(&report)?);
    } else {
        print!("{}", report_text(&report));
        if let Some(path) = options.save_profile {
            println!("Profile saved: {}", path.display());
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    #[cfg(feature = "gui")]
    let no_args = args.is_empty();
    let action = cli::parse(args).unwrap_or_else(|error| {
        eprintln!("error: {error}\n\nTry 'memtest --help' for usage.");
        std::process::exit(2);
    });
    let result = match action {
        cli::Action::Help => {
            cli::help();
            Ok(())
        }
        cli::Action::Version => {
            println!("memtest {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Action::Run(options) => {
            #[cfg(feature = "gui")]
            if no_args || options.gui {
                #[cfg(windows)]
                desktop::prepare_gui();
                if let Err(error) = gui::run(options) {
                    eprintln!("error: {error}");
                    #[cfg(windows)]
                    desktop::show_error(&error);
                    std::process::exit(1);
                }
                return;
            }
            run(options)
        }
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hardware() -> platform::Hardware {
        platform::Hardware {
            cpus: vec![0, 1, 2, 3, 4, 5],
            preferred: vec![0, 2, 4],
            cache_bytes: 32 << 20,
            cache_total_bytes: 64 << 20,
            memory_budget: 1 << 30,
            affinity: "hard",
            page_size: 4096,
            cache_line_bytes: 64,
            caches: Vec::new(),
        }
    }
    fn options(text: &str) -> cli::Options {
        let cli::Action::Run(o) = cli::parse(text.split_whitespace().map(str::to_string)).unwrap()
        else {
            panic!()
        };
        o
    }
    #[test]
    fn automatic_limits_and_explicit_cpu_placement() {
        let (spec, cpus) = resolve(&options(""), &hardware()).unwrap();
        assert_eq!(cpus, vec![0, 2, 4]);
        assert_eq!(spec.threads, 3);
        assert!(spec.bandwidth_bytes > hardware().cache_bytes);
        assert_eq!(spec.bandwidth_bytes % 128, 0);
        let (spec, cpus) =
            resolve(&options("--quick --cpus 5,1 --size-mib 64"), &hardware()).unwrap();
        assert_eq!(cpus, vec![5, 1]);
        assert_eq!((spec.threads, spec.samples, spec.min_time), (2, 3, 0.01));
        assert_eq!(
            resolve(&options("--size-mib 1"), &hardware())
                .unwrap()
                .0
                .threads,
            1
        );
        assert_eq!(
            resolve(&options("--test latency"), &hardware())
                .unwrap()
                .0
                .threads,
            1
        );
    }
    #[test]
    fn calibration_respects_fixed_runs() {
        for input in ["", "--threads 0", "--profile fixed --threads 0"] {
            assert!(calibrates_workers(&options(input)), "{input}");
        }
        for input in [
            "--threads 2",
            "--cpus 0,2",
            "--profile fixed",
            "--quick",
            "--quick --threads 0",
            "--test latency",
        ] {
            assert!(!calibrates_workers(&options(input)), "{input}");
        }
    }
    #[test]
    fn cache_only_profiles_place_cache_workers_and_preserve_ram_settings() {
        let (mut spec, _) = resolve(&options(""), &hardware()).unwrap();
        spec.threads = 32;
        spec.cache_threads = 2;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("memtest-{}-{stamp}.profile", std::process::id()));
        spec.save(&path).unwrap();
        let mut settings = options("--level l2");
        settings.profile = Some(path.clone());
        let resolved = resolve(&settings, &hardware());
        std::fs::remove_file(path).unwrap();
        let (replayed, cpus) = resolved.unwrap();
        assert_eq!(replayed, spec);
        assert_eq!(cpus, vec![0, 2]);
        let (explicit, cpus) =
            resolve(&options("--quick --threads 2 --level l1"), &hardware()).unwrap();
        assert_eq!((explicit.cache_threads, cpus), (2, vec![0, 2]));
    }
    #[test]
    fn automatic_and_quick_buffers_respect_memory_limits() {
        let mut hardware = hardware();
        hardware.memory_budget = (8 << 20) + 63;
        for input in ["", "--quick"] {
            let (spec, _) = resolve(&options(input), &hardware).unwrap();
            assert_eq!(spec.bandwidth_bytes, 4 << 20);
            assert_eq!(spec.latency_bytes, 8 << 20);
        }
        hardware.memory_budget = 0;
        assert!(resolve(&options("--quick"), &hardware).is_err());
        let (spec, _) = resolve(
            &options("--quick --size-mib 64 --latency-size-mib 16"),
            &hardware,
        )
        .unwrap();
        assert_eq!(spec.bandwidth_bytes, 64 << 20);
        assert_eq!(spec.latency_bytes, 16 << 20);
        assert!(resolve(&options("--test read --size-mib 64"), &hardware).is_ok());
        assert!(resolve(&options("--test latency --latency-size-mib 16"), &hardware).is_ok());
    }
    #[test]
    fn latency_uses_one_cpu_without_rewriting_bandwidth_settings() {
        let (spec, cpus) = resolve(&options("--test latency --threads 8"), &hardware()).unwrap();
        assert_eq!(spec.threads, 8);
        assert_eq!(cpus, vec![0]);
        let (spec, cpus) =
            resolve(&options("--test latency --threads 8 --cpus 5"), &hardware()).unwrap();
        assert_eq!(spec.threads, 8);
        assert_eq!(cpus, vec![5]);
        assert!(resolve(&options("--test latency --cpus 0,2"), &hardware()).is_err());
    }
    #[test]
    fn latency_profile_replay_preserves_worker_limits_on_a_smaller_host() {
        let (mut saved, _) = resolve(&options(""), &hardware()).unwrap();
        saved.threads = 32;
        saved.cache_threads = 16;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "memtest-latency-profile-{}-{stamp}.profile",
            std::process::id()
        ));
        saved.save(&path).unwrap();
        let mut host = hardware();
        host.cpus = vec![5];
        host.preferred = vec![5];
        host.caches = vec![platform::Cache {
            level: 1,
            bytes: 32 << 10,
            line_bytes: 64,
            cpus: vec![5],
        }];
        for input in [
            "--test latency --level memory",
            "--test latency --level memory --cpus 5",
            "--test latency --level l1",
            "--test latency --level l1 --cpus 5",
        ] {
            let mut settings = options(input);
            settings.profile = Some(path.clone());
            let (replayed, cpus) = resolve(&settings, &host).unwrap();
            assert_eq!(replayed, saved, "{input}");
            assert_eq!(replayed.text(), saved.text());
            assert_eq!(cpus, vec![5]);
            let plans = cache_plans(&settings, &host, &replayed, &cpus).unwrap();
            if settings.level == "l1" {
                assert_eq!(plans.len(), 1);
                assert_eq!(plans[0].cpus, vec![5]);
            } else {
                assert!(plans.is_empty());
            }
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn static_specs_are_validated_without_silent_substitution() {
        assert!(resolve(&options("--threads 8"), &hardware()).is_err());
        assert!(resolve(&options("--threads 2 --cpus 0"), &hardware()).is_err());
        assert!(resolve(&options("--cpus 8"), &hardware()).is_err());
        assert!(resolve(&options("--stride 63"), &hardware()).is_err());
        assert_eq!(
            resolve(&options("--threads 3 --size-mib 256"), &hardware())
                .unwrap()
                .0
                .bandwidth_bytes,
            256 << 20
        );
        assert_eq!(
            resolve(&options("--threads 3 --size-mib 384"), &hardware())
                .unwrap()
                .0
                .bandwidth_bytes,
            384 << 20
        );
        assert_eq!(placement(&hardware(), 5).unwrap(), vec![0, 2, 4, 1, 3]);
    }
    #[test]
    fn unsupported_backend_fails_before_profile_or_measurement() {
        for backend in [
            portable::Backend::Sse2,
            portable::Backend::Avx2,
            portable::Backend::Neon,
        ] {
            if !portable::Backend::available().contains(&backend) {
                let options = options(&format!(
                    "--isa {} --profile missing.profile",
                    backend.key()
                ));
                let error = resolve(&options, &hardware()).unwrap_err();
                assert!(error.contains("unavailable"));
                assert!(
                    measure(&options, |_| panic!("must not start measurement"))
                        .err()
                        .unwrap()
                        .contains("unavailable")
                );
            }
        }
    }
}
