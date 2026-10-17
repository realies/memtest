//! Event-driven desktop frontend; measurement remains in the shared core.
use crate::{Progress, cache, cli, platform, portable, profile};
use slint::winit_030::{WinitWindowAccessor, winit};
use slint::{ComponentHandle, Model};
use std::{cell::RefCell, rc::Rc, thread::JoinHandle, time::Instant};

slint::slint! { export { MemtestWindow } from "../ui/memtest.slint"; }
#[path = "screenshot.rs"]
mod screenshot;

fn fields(
    base: &cli::Options,
    workers: &str,
    bandwidth: &str,
    latency: &str,
    quick: bool,
) -> Result<cli::Options, String> {
    let mut args = Vec::new();
    for (flag, value) in [
        ("--threads", workers),
        ("--size-mib", bandwidth),
        ("--latency-size-mib", latency),
    ] {
        if !value.trim().is_empty() {
            args.extend([flag.to_owned(), value.trim().to_owned()]);
        }
    }
    let cli::Action::Run(edited) = cli::parse(args)? else {
        return Err("invalid settings".into());
    };
    let mut options = base.clone();
    // Blank means inherit the profile, including after a CLI worker override.
    options.threads = if base.profile.is_some() {
        edited.threads
    } else {
        edited.threads.or(base.threads)
    };
    options.size = edited.size.or(base.size);
    options.latency_size = edited.latency_size.or(base.latency_size);
    options.quick = quick;
    if quick && options.profile.is_some() {
        return Err("Quick run cannot be combined with a fixed profile".into());
    }
    Ok(options)
}

fn settings(window: &MemtestWindow, base: &cli::Options) -> Result<cli::Options, String> {
    let mut options = fields(
        base,
        &window.get_workers(),
        &window.get_bandwidth(),
        &window.get_latency(),
        window.get_quick(),
    )?;
    options.isa = window.get_code_path().to_string();
    options.stores = Some(portable::Stores::parse(&window.get_store_policy())?);
    options.window = Some(
        window
            .get_locality()
            .parse()
            .map_err(|_| "invalid locality block size")?,
    );
    portable::Backend::resolve(&options.isa)?;
    Ok(options)
}

fn mib(bytes: usize) -> String {
    if bytes.is_multiple_of(1 << 20) {
        (bytes / (1 << 20)).to_string()
    } else {
        format!("{:.1}", bytes as f64 / (1 << 20) as f64)
    }
}

fn workers_text(workers: usize, upper_bound: bool) -> String {
    format!(
        "{}{workers} {}",
        if upper_bound { "up to " } else { "" },
        if workers == 1 { "worker" } else { "workers" }
    )
}

fn displayed_workers(options: &cli::Options, cpus: &[usize], plans: &[cache::Plan]) -> usize {
    if options.test == "latency" {
        1
    } else if matches!(options.level.as_str(), "all" | "memory") {
        cpus.len()
    } else {
        plans.first().map_or(cpus.len(), |plan| plan.cpus.len())
    }
}

fn details(spec: &profile::Spec, workers: usize, upper_bound: bool, level: &str) -> String {
    let scope = if matches!(level, "all" | "memory") {
        "RAM"
    } else {
        "Cache"
    };
    let stride = if scope == "RAM" {
        format!("{} B stride", spec.stride)
    } else {
        "cache-line stride".into()
    };
    format!(
        "{scope}: {} · {} samples × {:.3} s · {stride} · seed {}",
        workers_text(workers, upper_bound),
        spec.samples,
        spec.min_time,
        spec.seed
    )
}

fn choices(
    selected: &str,
    automatic: String,
    inherited: Option<String>,
    values: impl IntoIterator<Item = usize>,
    unit: &str,
) -> (slint::ModelRc<Choice>, i32) {
    let selected = if selected.is_empty() && inherited.is_none() {
        "0"
    } else {
        selected
    };
    let mut items = vec![Choice {
        label: if selected == "0" {
            automatic
        } else {
            "Auto".into()
        }
        .into(),
        value: "0".into(),
    }];
    if let Some(label) = inherited {
        items.push(Choice {
            label: label.into(),
            value: "".into(),
        });
    }
    for value in values
        .into_iter()
        .filter(|&value| value != 0)
        .map(|value| value.to_string())
        .chain((!selected.is_empty() && selected != "0").then(|| selected.to_owned()))
    {
        if !items.iter().any(|item| item.value == value.as_str()) {
            items.push(Choice {
                label: format!("{value}{unit}").into(),
                value: value.into(),
            });
        }
    }
    let index = items
        .iter()
        .position(|item| item.value == selected)
        .unwrap_or(0) as i32;
    (std::rc::Rc::new(slint::VecModel::from(items)).into(), index)
}

fn initial_choice(value: Option<usize>) -> Option<String> {
    value.map(|value| value.to_string())
}

fn preview(window: &MemtestWindow, base: &cli::Options, hardware: &platform::Hardware) {
    window.set_memory_warning("".into());
    window.set_details("".into());
    let resolved = settings(window, base)
        .and_then(|options| crate::resolve(&options, hardware).map(|resolved| (options, resolved)));
    {
        let spec = resolved.as_ref().ok().map(|(_, (spec, _))| spec);
        let initial = base
            .profile
            .as_deref()
            .and_then(|path| profile::Spec::load(path).ok());
        let mut workers = vec![1];
        while workers.last().copied().unwrap() <= hardware.cpus.len() / 2 {
            workers.push(workers.last().copied().unwrap() * 2);
        }
        workers.extend([hardware.cpus.len(), base.threads.unwrap_or(0)]);
        if base.profile.is_some() {
            workers.extend(initial.as_ref().map(|spec| spec.threads));
        }
        let (model, index) = choices(
            &window.get_workers(),
            resolved.as_ref().map_or_else(
                |_| "Auto".into(),
                |(options, (spec, _))| {
                    if crate::calibrates_workers(options) {
                        format!("Auto ({} max)", spec.threads)
                    } else {
                        format!("Auto ({})", spec.threads)
                    }
                },
            ),
            initial
                .as_ref()
                .map(|spec| format!("Fixed ({})", spec.threads)),
            workers,
            "",
        );
        window.set_worker_options(model);
        window.set_worker_index(index);
        for (selected, bytes, original, latency) in [
            (
                window.get_bandwidth(),
                spec.map(|s| s.bandwidth_bytes),
                base.size,
                false,
            ),
            (
                window.get_latency(),
                spec.map(|s| s.latency_bytes),
                base.latency_size,
                true,
            ),
        ] {
            let original_bytes = original
                .or_else(|| {
                    initial.as_ref().map(|s| {
                        if latency {
                            s.latency_bytes
                        } else {
                            s.bandwidth_bytes
                        }
                    })
                })
                .unwrap_or(0);
            let fixed = original_bytes != 0
                && !original_bytes.is_multiple_of(1 << 20)
                && (original.is_some_and(|n| n != 0)
                    || (base.profile.is_some() && original != Some(0)));
            let (model, index) = choices(
                &selected,
                bytes.map_or_else(|| "Auto".into(), |n| format!("Auto ({} MiB)", mib(n))),
                fixed.then(|| format!("Fixed ({} MiB)", mib(original_bytes))),
                [64, 128, 256, 512, 1024].into_iter().chain(
                    (original.is_some() || base.profile.is_some())
                        .then_some(original_bytes)
                        .filter(|n| n.is_multiple_of(1 << 20))
                        .map(|n| n / (1 << 20)),
                ),
                " MiB",
            );
            if latency {
                window.set_latency_options(model);
                window.set_latency_index(index);
            } else {
                window.set_bandwidth_options(model);
                window.set_bandwidth_index(index);
            }
        }
    }
    match resolved {
        Ok((options, (spec, cpus))) => {
            let plans = match crate::cache_plans(&options, hardware, &spec, &cpus) {
                Ok(plans) => plans,
                Err(error) => {
                    replace_rows(window, rows(&options, &spec, &cpus, &[], hardware, false));
                    window.set_status(error.into());
                    window.set_error(true);
                    return;
                }
            };
            replace_rows(
                window,
                rows(&options, &spec, &cpus, &plans, hardware, false),
            );
            window.set_details(
                details(
                    &spec,
                    displayed_workers(&options, &cpus, &plans),
                    crate::calibrates_workers(&options),
                    &options.level,
                )
                .into(),
            );
            window.set_status("Ready".into());
            window.set_error(false);
        }
        Err(error) => {
            window.set_details("".into());
            window.set_status(error.into());
            window.set_error(true);
        }
    }
}

fn clear(window: &MemtestWindow) {
    replace_rows(
        window,
        Rc::new(slint::VecModel::from(Vec::<ResultRow>::new())).into(),
    );
    window.set_has_results(false);
    window.set_active_level(-1);
    window.set_active_column(-1);
    window.set_progress(0.0);
    window.set_elapsed("".into());
}

fn value_text(level: u8, label: &str, value: f64) -> String {
    if label == "Latency" {
        format!("{value:.1}")
    } else if level == 0 {
        format!("{value:.0}")
    } else {
        format!("{:.1}", value / 1000.0)
    }
}

fn column(label: &str) -> Option<usize> {
    match label {
        "Read" => Some(0),
        "Write" => Some(1),
        "Copy" => Some(2),
        "Latency" => Some(3),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Target {
    level: u8,
    column: Option<usize>,
}

impl Target {
    fn available(self, rows: &slint::ModelRc<ResultRow>) -> bool {
        rows.iter()
            .any(|row| row.level == i32::from(self.level) && row.available)
    }

    fn level_key(self) -> String {
        if self.level == 0 {
            "memory".into()
        } else {
            format!("l{}", self.level)
        }
    }

    fn parse(level: i32, column: i32) -> Result<Option<Self>, String> {
        match (level, column) {
            (-1, -1) => Ok(None),
            (0..=3, -1) => Ok(Some(Self {
                level: level as u8,
                column: None,
            })),
            (0..=3, 0..=3) => Ok(Some(Self {
                level: level as u8,
                column: Some(column as usize),
            })),
            _ => Err("invalid result cell".into()),
        }
    }

    fn prepare(self, options: &mut cli::Options) {
        if self.column.is_none() || self.column == Some(2) {
            // Copy and whole-row runs must budget both streams before auto sizes
            // become explicit settings in `options` below.
            options.test = if self.column.is_none() { "all" } else { "copy" }.into();
            options.level = self.level_key();
        }
    }

    fn options(
        self,
        options: &mut cli::Options,
        spec: &profile::Spec,
        cpus: &[usize],
        plans: &[cache::Plan],
        affinity: &str,
    ) -> Result<(), String> {
        let cpu = if self.level == 0 {
            cpus[0]
        } else {
            plans
                .iter()
                .find(|plan| plan.level == self.level)
                .ok_or("cache level is unavailable")?
                .cpu
        };
        options.level = self.level_key();
        options.test = self
            .column
            .map_or("all", |column| ["read", "write", "copy", "latency"][column])
            .into();
        // Keep the displayed footprint; filtering tests must not change auto sizing.
        options.size = Some(spec.bandwidth_bytes);
        options.latency_size = Some(spec.latency_bytes);
        options.save_profile = None;
        if self.column == Some(3) {
            options.threads = Some(1);
            options.cpus = (affinity != "scheduler").then(|| vec![cpu]);
        }
        Ok(())
    }
}

fn refresh_rows(
    current: slint::ModelRc<ResultRow>,
    fresh: slint::ModelRc<ResultRow>,
    target: Option<Target>,
) -> slint::ModelRc<ResultRow> {
    let Some(target) = target else {
        return fresh;
    };
    for (index, mut row) in current.iter().enumerate() {
        let Some(replacement) = fresh.iter().find(|fresh| fresh.level == row.level) else {
            continue;
        };
        let targeted = row.level == i32::from(target.level);
        let same_kind = row.label == replacement.label;
        if !targeted && row.available && replacement.available && same_kind {
            continue;
        }
        if let Some(column) = target.column
            && targeted
            && row.available
            && replacement.available
            && same_kind
        {
            if let (Some(cells), Some(cell)) = (
                row.cells.as_any().downcast_ref::<slint::VecModel<Cell>>(),
                replacement.cells.row_data(column),
            ) {
                cells.set_row_data(column, cell);
            }
            row.context = "Hover a result for its tested settings.".into();
        } else {
            // Availability or cache identity changes discard every stale cell.
            row = replacement;
        }
        if let Some(rows) = current
            .as_any()
            .downcast_ref::<slint::VecModel<ResultRow>>()
        {
            rows.set_row_data(index, row);
        }
    }
    current
}

fn has_results(model: &slint::ModelRc<ResultRow>) -> bool {
    model
        .iter()
        .filter(|row| row.available)
        .any(|row| row.cells.iter().any(|cell| !cell.value.is_empty()))
}

fn memory_notes(model: &slint::ModelRc<ResultRow>) -> String {
    let mut notes = Vec::new();
    for row in model.iter().filter(|row| row.available) {
        for cell in row.cells.iter() {
            for note in cell.notes.lines() {
                if !notes.iter().any(|seen| seen == note) {
                    notes.push(note.to_owned());
                }
            }
        }
    }
    notes.join("\n")
}

fn replace_rows(window: &MemtestWindow, model: slint::ModelRc<ResultRow>) {
    window.set_memory_warning(memory_notes(&model).into());
    window.set_rows(model);
}

fn update_has_results(window: &MemtestWindow) {
    window.set_has_results(has_results(&window.get_rows()));
}

fn row_name(level: u8) -> String {
    if level == 0 {
        "Memory".into()
    } else {
        format!("L{level} Cache")
    }
}

fn row_label(rows: &slint::ModelRc<ResultRow>, level: u8) -> String {
    rows.iter()
        .find(|row| row.level == i32::from(level))
        .map_or_else(|| row_name(level), |row| row.label.to_string())
}

fn bytes_text(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{} MiB", mib(bytes))
    } else {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    }
}

fn row(level: u8, context: String, copy_context: String, latency_context: String) -> ResultRow {
    let cells = [
        context.clone(),
        context.clone(),
        copy_context,
        latency_context,
    ]
    .into_iter()
    .map(|info| Cell {
        value: "".into(),
        info: info.into(),
        notes: "".into(),
    })
    .collect::<Vec<_>>();
    ResultRow {
        level: i32::from(level),
        available: true,
        label: row_name(level).into(),
        context: context.into(),
        cells: Rc::new(slint::VecModel::from(cells)).into(),
    }
}

fn unavailable_reason(level: u8, hardware: &platform::Hardware, apple_arm: bool) -> String {
    if hardware.caches.iter().all(|cache| cache.level != level) {
        if level == 3 && apple_arm {
            "macOS did not report CPU L3 cache geometry. No supported SLC geometry is available."
                .into()
        } else {
            format!("L{level} data/unified cache geometry is not reported by this system.")
        }
    } else if !hardware.caches.iter().any(|cache| {
        cache.level == level
            && cache.bytes != 0
            && cache.line_bytes >= 8
            && cache.line_bytes.is_power_of_two()
    }) {
        format!(
            "Reported L{level} cache geometry is incomplete; a capacity and valid cache-line size are required."
        )
    } else {
        format!(
            "L{level} cache geometry is reported, but an isolated test working set is unavailable with the current CPU selection, lower-cache geometry and memory allowance."
        )
    }
}

fn unavailable_row(level: u8, hardware: &platform::Hardware) -> ResultRow {
    let reason = unavailable_reason(
        level,
        hardware,
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
    );
    let mut row = row(level, reason.clone(), reason.clone(), reason);
    row.available = false;
    row
}

fn rows(
    options: &cli::Options,
    spec: &profile::Spec,
    cpus: &[usize],
    plans: &[cache::Plan],
    hardware: &platform::Hardware,
    resolved: bool,
) -> slint::ModelRc<ResultRow> {
    let mut rows = Vec::new();
    if matches!(options.level.as_str(), "all" | "memory") {
        let workers = workers_text(cpus.len(), !resolved && crate::calibrates_workers(options));
        let context = format!(
            "{} total read/write working set · {workers}. {} RAM stores; decimal MB/s.",
            bytes_text(spec.bandwidth_bytes),
            spec.stores
                .name(portable::Backend::resolve(&options.isa).unwrap()),
        );
        let copy = format!(
            "{} source + {} destination total · {workers}. {} stores; copy counts combined bytes read + written.",
            bytes_text(spec.bandwidth_bytes),
            bytes_text(spec.bandwidth_bytes),
            spec.stores
                .name(portable::Backend::resolve(&options.isa).unwrap()),
        );
        let locality = if spec.window == 0 {
            "full shuffle".into()
        } else {
            format!("{} random blocks", bytes_text(spec.window))
        };
        let latency = format!(
            "{} pointer chain · {} B stride · {locality} · one worker. Dependent-load latency in ns.",
            bytes_text(spec.latency_bytes),
            spec.stride
        );
        rows.push(row(0, context, copy, latency));
    }
    for level in 1..=3 {
        if options.level != "all" && options.level != format!("l{level}") {
            continue;
        }
        let Some(plan) = plans.iter().find(|plan| plan.level == level) else {
            rows.push(unavailable_row(level, hardware));
            continue;
        };
        let worker = if cfg!(target_os = "macos") {
            "one OS-scheduled worker".into()
        } else {
            format!("one worker on CPU {}", plan.cpu)
        };
        let slc = matches!(plan.kind, cache::Kind::Slc);
        let context = if slc {
            format!(
                "{} CPU-visible working set · one OS-scheduled worker. Cached loads/stores, decimal GB/s.\n{} published nominal SLC capacity.",
                bytes_text(plan.bandwidth_bytes),
                bytes_text(plan.capacity_bytes),
            )
        } else {
            format!(
                "{} aggregate working set · {} workers. Cached bandwidth, decimal GB/s.",
                bytes_text(plan.bandwidth_bytes),
                plan.cpus.len(),
            )
        };
        let copy = format!(
            "{context}\n{} source + {} destination share the working set. Copy rate counts combined bytes read + written.",
            bytes_text(plan.bandwidth_bytes / 2),
            bytes_text(plan.bandwidth_bytes / 2)
        );
        let latency = if slc {
            format!(
                "{} published nominal SLC capacity · {} CPU-visible pointer chain · one OS-scheduled worker · {} B stride. Dependent-load latency in ns.",
                bytes_text(plan.capacity_bytes),
                bytes_text(plan.working_set_bytes),
                plan.stride,
            )
        } else {
            format!(
                "{} capacity · {} pointer chain · {worker} · {} B stride. Dependent-load latency in ns.",
                bytes_text(plan.capacity_bytes),
                bytes_text(plan.working_set_bytes),
                plan.stride,
            )
        };
        let mut entry = row(plan.level, context, copy, latency);
        entry.label = plan.label().into();
        rows.push(entry);
    }
    let method = format!(
        "Method {} · {} · {} samples × {:.3} s · seed {}",
        profile::METHOD,
        portable::Backend::resolve(&options.isa)
            .unwrap()
            .key()
            .to_uppercase(),
        spec.samples,
        spec.min_time,
        spec.seed,
    );
    for row in rows.iter().filter(|row| row.available) {
        let cells = row
            .cells
            .as_any()
            .downcast_ref::<slint::VecModel<Cell>>()
            .unwrap();
        for (index, mut cell) in cells.iter().enumerate() {
            let selected = options.test == "all"
                || options.test == ["read", "write", "copy", "latency"][index];
            if row.level == 0 {
                if index == 0 {
                    cell.info = format!(
                        "{} total read working set · {}. Cached loads; decimal MB/s.",
                        bytes_text(spec.bandwidth_bytes),
                        workers_text(cpus.len(), !resolved && crate::calibrates_workers(options)),
                    )
                    .into();
                }
                if selected {
                    cell.notes = crate::memory::warnings(hardware, spec, index < 3, index == 3)
                        .join("\n")
                        .into();
                }
            } else if selected
                && plans.iter().any(|plan| {
                    i32::from(plan.level) == row.level && matches!(plan.kind, cache::Kind::Slc)
                })
            {
                cell.notes = "Published nominal SLC geometry, not OS-reported capacity. CPU-visible results include lower-cache contributions; SLC-only hits are not guaranteed."
                    .into();
            }
            cell.info = format!("{}\n{method}", cell.info).into();
            cells.set_row_data(index, cell);
        }
    }
    Rc::new(slint::VecModel::from(rows)).into()
}

fn score(
    model: &slint::ModelRc<ResultRow>,
    level: u8,
    label: &str,
    value: f64,
    range: Option<f64>,
) {
    let Some(column) = column(label) else {
        return;
    };
    for row in model
        .iter()
        .filter(|row| row.level == i32::from(level) && row.available)
    {
        let Some(cells) = row.cells.as_any().downcast_ref::<slint::VecModel<Cell>>() else {
            return;
        };
        if let Some(mut cell) = cells.row_data(column) {
            cell.value = value_text(level, label, value).into();
            if let Some(range) = range {
                cell.info = format!(
                    "{}\nMedian result; full sample range {:.1}% (max − min) / median.",
                    cell.info, range
                )
                .into();
            }
            cells.set_row_data(column, cell);
        }
    }
}

fn result(window: &MemtestWindow, level: u8, label: &str, measurement: portable::Measurement) {
    score(
        &window.get_rows(),
        level,
        label,
        measurement.value,
        Some(measurement.spread_percent),
    );
}

pub fn run(options: cli::Options) -> Result<(), String> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .map_err(|e| e.to_string())?;
    let window = MemtestWindow::new().map_err(|e| e.to_string())?;
    window.set_version(env!("CARGO_PKG_VERSION").into());
    let minimize_window = window.as_weak();
    window.on_minimize_window(move || {
        if let Some(window) = minimize_window.upgrade() {
            window.window().set_minimized(true);
        }
    });
    let save_window = window.as_weak();
    window.on_save(move || {
        let Some(window) = save_window.upgrade() else {
            return;
        };
        if window.get_busy() {
            return;
        }
        match screenshot::save(window.window()) {
            Ok(Some(_)) => {
                window.set_error(false);
                window.set_status("Saved image".into());
            }
            Ok(None) => {}
            Err(error) => {
                window.set_status(error.into());
                window.set_error(true);
            }
        }
    });
    let hardware = platform::hardware();
    let info = crate::system_info::detect(&hardware);
    window.set_cpu(info.cpu.into());
    window.set_memory(info.memory.into());
    window.set_modules(info.modules.into());
    window.set_board(info.board.into());
    window.set_bios(info.bios.into());
    window.set_threads(format!("{} available", hardware.cpus.len()).into());
    let mut backends = vec![Choice {
        label: format!(
            "Auto ({})",
            portable::Backend::resolve("auto")?.key().to_uppercase()
        )
        .into(),
        value: "auto".into(),
    }];
    backends.extend(portable::Backend::available().iter().map(|backend| Choice {
        label: backend.key().to_uppercase().into(),
        value: backend.key().into(),
    }));
    if !backends
        .iter()
        .any(|item| item.value == options.isa.as_str())
    {
        backends.push(Choice {
            label: options.isa.to_uppercase().into(),
            value: options.isa.clone().into(),
        });
    }
    let index = backends
        .iter()
        .position(|item| item.value == options.isa.as_str())
        .unwrap_or(0);
    window.set_code_path_options(Rc::new(slint::VecModel::from(backends)).into());
    window.set_code_path_index(index as i32);
    window.set_code_path(options.isa.clone().into());
    window.set_os(info.os.into());
    window.set_quick(options.quick);
    window.set_quick_allowed(options.profile.is_none());
    let initial = options
        .profile
        .as_deref()
        .and_then(|path| profile::Spec::load(path).ok());
    let stores = options
        .stores
        .or(initial.as_ref().map(|s| s.stores))
        .unwrap_or(portable::Stores::Streaming);
    window.set_store_policy(stores.key().into());
    window.set_store_index(if stores == portable::Stores::Streaming {
        0
    } else {
        1
    });
    window.set_store_options(
        Rc::new(slint::VecModel::from(vec![
            Choice {
                label: "Streaming".into(),
                value: "streaming".into(),
            },
            Choice {
                label: "Cached".into(),
                value: "cached".into(),
            },
        ]))
        .into(),
    );
    let locality = options
        .window
        .or(initial.as_ref().map(|s| s.window))
        .unwrap_or_else(|| {
            crate::resolve(&options, &hardware).map_or(hardware.page_size, |(spec, _)| spec.window)
        });
    let mut localities = vec![
        Choice {
            label: "Block-local".into(),
            value: profile::DEFAULT_WINDOW.to_string().into(),
        },
        Choice {
            label: "Page-local".into(),
            value: hardware.page_size.to_string().into(),
        },
        Choice {
            label: "Full random".into(),
            value: "0".into(),
        },
    ];
    let index = localities
        .iter()
        .position(|c| c.value == locality.to_string().as_str())
        .unwrap_or_else(|| {
            localities.push(Choice {
                label: format!("{} blocks", bytes_text(locality)).into(),
                value: locality.to_string().into(),
            });
            localities.len() - 1
        });
    window.set_locality(locality.to_string().into());
    window.set_locality_index(index as i32);
    window.set_locality_options(Rc::new(slint::VecModel::from(localities)).into());
    for (value, set) in [
        (options.threads, 0),
        (
            options
                .size
                .or(initial.as_ref().map(|s| s.bandwidth_bytes))
                .filter(|n| n.is_multiple_of(1 << 20))
                .map(|n| n / (1 << 20)),
            1,
        ),
        (
            options
                .latency_size
                .or(initial.as_ref().map(|s| s.latency_bytes))
                .filter(|n| n.is_multiple_of(1 << 20))
                .map(|n| n / (1 << 20)),
            2,
        ),
    ] {
        if let Some(value) = initial_choice(value) {
            match set {
                0 => window.set_workers(value.into()),
                1 => window.set_bandwidth(value.into()),
                _ => window.set_latency(value.into()),
            }
        }
    }
    preview(&window, &options, &hardware);
    window.on_close_window(|| {
        let _ = slint::quit_event_loop();
    });
    let weak = window.as_weak();
    let base = options.clone();
    let preview_hardware = hardware.clone();
    window.on_settings_changed(move || {
        if let Some(window) = weak.upgrade() {
            clear(&window);
            preview(&window, &base, &preview_hardware);
        }
    });
    let worker: Rc<RefCell<Option<JoinHandle<()>>>> = Rc::default();
    let retained = worker.clone();
    let weak = window.as_weak();
    window.on_start(move |level, cell_column| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if window.get_busy() {
            return;
        }
        let resolved = (|| {
            let target = Target::parse(level, cell_column)?;
            if target.is_some_and(|target| !target.available(&window.get_rows())) {
                return Err("result row is unavailable".to_owned());
            }
            let mut options = settings(&window, &options)?;
            if let Some(target) = target {
                target.prepare(&mut options);
            }
            let (mut spec, mut cpus) = crate::resolve(&options, &hardware)?;
            let mut plans = crate::cache_plans(&options, &hardware, &spec, &cpus)?;
            if let Some(target) = target {
                target.options(&mut options, &spec, &cpus, &plans, hardware.affinity)?;
                (spec, cpus) = crate::resolve(&options, &hardware)?;
                plans = crate::cache_plans(&options, &hardware, &spec, &cpus)?;
            }
            Ok((options, spec, cpus, plans, target))
        })();
        let (options, spec, cpus, plans, target) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                window.set_status(error.into());
                window.set_error(true);
                return;
            }
        };
        if let Some(previous) = retained.borrow_mut().take() {
            let _ = previous.join();
        }
        window.set_has_results(false);
        window.set_progress(0.0);
        window.set_elapsed("".into());
        replace_rows(
            &window,
            refresh_rows(
                window.get_rows(),
                rows(&options, &spec, &cpus, &plans, &hardware, false),
                target,
            ),
        );
        window.set_busy(true);
        window.set_error(false);
        window.set_status("Preparing…".into());
        let updates = weak.clone();
        let completion = weak.clone();
        let memory = matches!(options.level.as_str(), "all" | "memory");
        let tests = if options.test == "all" { 4 } else { 1 };
        let mut total = (usize::from(memory) + plans.len()) * tests;
        let starting_hardware = hardware.clone();
        match std::thread::Builder::new()
            .name("memtest-measure".into())
            .spawn(move || {
                let started = Instant::now();
                let mut completed = 0;
                let row_options = std::sync::Arc::new(options.clone());
                // Debug builds unwind; release builds intentionally abort on panic.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::measure(&options, |progress| {
                        if let Progress::Plan(plans, _, _, _) = &progress {
                            total = (usize::from(memory) + plans.len()) * tests;
                        }
                        if matches!(progress, Progress::Result(..)) {
                            completed += 1;
                        }
                        let row_options = row_options.clone();
                        let _ = updates.upgrade_in_event_loop(move |window| match progress {
                            Progress::Plan(plans, spec, cpus, hardware) => {
                                replace_rows(
                                    &window,
                                    refresh_rows(
                                        window.get_rows(),
                                        rows(&row_options, &spec, &cpus, &plans, &hardware, true),
                                        target,
                                    ),
                                );
                                window.set_details(
                                    details(
                                        &spec,
                                        displayed_workers(&row_options, &cpus, &plans),
                                        false,
                                        &row_options.level,
                                    )
                                    .into(),
                                );
                            }
                            Progress::Stage(stage) => {
                                window.set_status(stage.into());
                                window.set_active_level(-1);
                                window.set_active_column(-1);
                            }
                            Progress::Test(level, label) => {
                                window.set_status(
                                    format!("{} · {label}", row_label(&window.get_rows(), level))
                                        .into(),
                                );
                                window.set_active_level(i32::from(level));
                                window.set_active_column(column(label).map_or(-1, |n| n as i32));
                            }
                            Progress::Result(level, label, measurement) => {
                                result(&window, level, label, measurement);
                                window.set_progress(completed as f32 / total as f32);
                                window.set_elapsed(format!("{completed} / {total}").into());
                            }
                            Progress::Sample(level, label, index, count, value) => {
                                score(&window.get_rows(), level, label, value, None);
                                let progress =
                                    (completed as f32 + index as f32 / count as f32) / total as f32;
                                window.set_progress(progress);
                                window.set_status(
                                    format!(
                                        "{} · {label} · sample {index} of {count}",
                                        row_label(&window.get_rows(), level)
                                    )
                                    .into(),
                                );
                                window.set_elapsed(format!("{:.0} %", 100.0 * progress).into());
                            }
                        });
                    })
                }))
                .unwrap_or_else(|_| Err("Benchmark thread panicked".into()));
                let elapsed = started.elapsed();
                let _ = completion.upgrade_in_event_loop(move |window| {
                    window.set_busy(false);
                    window.set_active_level(-1);
                    window.set_active_column(-1);
                    match outcome {
                        Ok(report) => {
                            let plans: Vec<_> = report
                                .caches
                                .iter()
                                .map(|cache| cache.plan.clone())
                                .collect();
                            replace_rows(
                                &window,
                                refresh_rows(
                                    window.get_rows(),
                                    rows(
                                        &options,
                                        &report.spec,
                                        &report.cpus,
                                        &plans,
                                        &report.hardware,
                                        true,
                                    ),
                                    target,
                                ),
                            );
                            for (label, value) in [
                                ("Read", report.results.read),
                                ("Write", report.results.write),
                                ("Copy", report.results.copy),
                                ("Latency", report.results.latency),
                            ] {
                                if let Some(value) = value {
                                    result(&window, 0, label, value);
                                }
                            }
                            for cache in &report.caches {
                                for (label, value) in [
                                    ("Read", cache.results.read),
                                    ("Write", cache.results.write),
                                    ("Copy", cache.results.copy),
                                    ("Latency", cache.results.latency),
                                ] {
                                    if let Some(value) = value {
                                        result(&window, cache.plan.level, label, value);
                                    }
                                }
                            }
                            window.set_details(
                                details(
                                    &report.spec,
                                    displayed_workers(&options, &report.cpus, &plans),
                                    false,
                                    &options.level,
                                )
                                .into(),
                            );
                            update_has_results(&window);
                            window.set_status(target.map_or_else(
                                || "Complete".into(),
                                |target| {
                                    let name = row_label(&window.get_rows(), target.level);
                                    let label = target.column.map_or_else(
                                        || name.clone(),
                                        |column| {
                                            format!(
                                                "{} · {}",
                                                name,
                                                ["Read", "Write", "Copy", "Latency"][column]
                                            )
                                        },
                                    );
                                    format!("{label} complete").into()
                                },
                            ));
                            window.set_progress(1.0);
                            window.set_elapsed(format!("{:.1} s", elapsed.as_secs_f64()).into());
                        }
                        Err(error) => {
                            // A live sample is not a completed result.
                            replace_rows(
                                &window,
                                refresh_rows(
                                    window.get_rows(),
                                    rows(&options, &spec, &cpus, &plans, &starting_hardware, false),
                                    target,
                                ),
                            );
                            update_has_results(&window);
                            window.set_status(error.into());
                            window.set_error(true);
                        }
                    }
                });
            }) {
            Ok(handle) => *retained.borrow_mut() = Some(handle),
            Err(error) => {
                window.set_busy(false);
                update_has_results(&window);
                window.set_status(error.to_string().into());
                window.set_error(true);
            }
        }
    });
    let startup_error = Rc::new(RefCell::new(None));
    let error = startup_error.clone();
    let startup_window = window.as_weak();
    slint::spawn_local(async move {
        let Some(window) = startup_window.upgrade() else {
            return;
        };
        // The native window is created hidden at its DPI-scaled size.
        let outcome = match window.window().winit_window().await {
            Ok(native) => {
                if let Some(monitor) = native
                    .current_monitor()
                    .or_else(|| native.primary_monitor())
                {
                    let origin = monitor.position();
                    let screen = monitor.size();
                    let size = native.outer_size();
                    // Wayland leaves placement to the compositor and ignores this request.
                    native.set_outer_position(winit::dpi::PhysicalPosition::new(
                        f64::from(origin.x)
                            + f64::from(screen.width.saturating_sub(size.width)) / 2.0,
                        f64::from(origin.y)
                            + f64::from(screen.height.saturating_sub(size.height)) / 2.0,
                    ));
                }
                drop(native);
                window.show()
            }
            Err(error) => Err(error),
        };
        if let Err(message) = outcome {
            *error.borrow_mut() = Some(message.to_string());
            let _ = slint::quit_event_loop();
        }
    })
    .map_err(|e| e.to_string())?;
    let outcome = slint::run_event_loop().map_err(|e| e.to_string());
    // Closing this frontend exits main; dropping the handle never blocks the UI.
    drop(worker.borrow_mut().take());
    startup_error.borrow_mut().take().map_or(outcome, Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hardware() -> platform::Hardware {
        platform::Hardware {
            cpus: vec![0, 1],
            preferred: vec![0, 1],
            caches: Vec::new(),
            cache_bytes: 32 << 20,
            cache_total_bytes: 64 << 20,
            cache_line_bytes: 64,
            slc_capacity: None,
            memory_budget: 1 << 30,
            affinity: "hard",
            page_size: 4096,
        }
    }

    #[test]
    fn nominal_slc_identity_and_notes_do_not_survive_a_cpu_cache_replacement() {
        let hardware = hardware();
        let options = cli::Options {
            threads: Some(1),
            ..Default::default()
        };
        let (spec, cpus) = crate::resolve(&options, &hardware).unwrap();
        let mut plan = cache::Plan {
            kind: cache::Kind::Slc,
            level: 3,
            capacity_bytes: 48 << 20,
            working_set_bytes: 24 << 20,
            bandwidth_bytes: 24 << 20,
            stride: 128,
            cpu: 0,
            cpus: vec![0],
        };
        let model = rows(
            &options,
            &spec,
            &cpus,
            std::slice::from_ref(&plan),
            &hardware,
            true,
        );
        let slc = model.row_data(3).unwrap();
        assert_eq!(slc.label, "SLC");
        assert_eq!(row_label(&model, 3), "SLC");
        assert!(
            slc.context
                .contains("48 MiB published nominal SLC capacity")
        );
        assert!(slc.context.contains("24 MiB CPU-visible working set"));
        assert!(slc.context.contains("one OS-scheduled worker"));
        assert!(slc.cells.iter().all(|cell| {
            cell.notes.contains("lower-cache contributions")
                && cell.notes.contains("SLC-only hits are not guaranteed")
                && !cell.info.contains("streaming")
        }));
        assert_eq!(
            memory_notes(&model)
                .matches("Published nominal SLC")
                .count(),
            1
        );
        score(&model, 0, "Read", 60000.0, Some(1.0));
        for label in ["Read", "Write", "Copy", "Latency"] {
            score(&model, 3, label, 1000.0, Some(2.0));
        }
        let memory = model.row_data(0).unwrap().cells.iter().collect::<Vec<_>>();
        plan.kind = cache::Kind::Cpu;
        let model = refresh_rows(
            model,
            rows(&options, &spec, &cpus, &[plan], &hardware, true),
            Some(Target {
                level: 3,
                column: Some(0),
            }),
        );
        let cache = model.row_data(3).unwrap();
        assert_eq!(cache.label, "L3 Cache");
        assert!(
            cache
                .cells
                .iter()
                .all(|cell| cell.value.is_empty() && cell.notes.is_empty())
        );
        assert_eq!(
            model.row_data(0).unwrap().cells.iter().collect::<Vec<_>>(),
            memory
        );
        assert!(!memory_notes(&model).contains("Published nominal SLC"));
    }

    #[test]
    fn cache_placeholders_keep_order_and_explain_unavailability() {
        let mut hardware = hardware();
        for (level, bytes) in [(1, 32 << 10), (2, 512 << 10)] {
            hardware.caches.push(platform::Cache {
                level,
                bytes,
                line_bytes: 64,
                cpus: vec![0, 1],
            });
        }
        let options = cli::Options {
            threads: Some(2),
            ..Default::default()
        };
        let (spec, cpus) = crate::resolve(&options, &hardware).unwrap();
        let mut plans = cache::plans(&hardware, &cpus, "all").unwrap();
        plans.reverse();
        let model = rows(&options, &spec, &cpus, &plans, &hardware, false);
        assert_eq!(
            model.iter().map(|row| row.level).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(
            model.iter().map(|row| row.available).collect::<Vec<_>>(),
            [true, true, true, false]
        );
        assert_eq!(model.row_data(3).unwrap().label, "L3 Cache");
        assert!(
            model
                .row_data(3)
                .unwrap()
                .cells
                .iter()
                .all(|cell| cell.value.is_empty())
        );
        assert!(
            !Target {
                level: 3,
                column: None
            }
            .available(&model)
        );
        assert!(
            Target {
                level: 2,
                column: Some(0)
            }
            .available(&model)
        );
        score(&model, 3, "Read", 123456.0, Some(1.0));
        assert!(!has_results(&model));
        assert!(unavailable_reason(3, &hardware, false).contains("not reported"));
        let apple = unavailable_reason(3, &hardware, true);
        assert!(apple.contains("macOS did not report CPU L3"));
        assert!(apple.contains("No supported SLC geometry is available"));

        hardware.caches.push(platform::Cache {
            level: 3,
            bytes: 64 << 10,
            line_bytes: 0,
            cpus: vec![0, 1],
        });
        assert!(unavailable_reason(3, &hardware, false).contains("incomplete"));
        hardware.caches.last_mut().unwrap().line_bytes = 64;
        let plans = cache::plans(&hardware, &cpus, "all").unwrap();
        assert!(plans.iter().all(|plan| plan.level != 3));
        let model = rows(&options, &spec, &cpus, &plans, &hardware, true);
        let unavailable = model.row_data(3).unwrap();
        assert!(!unavailable.available);
        assert!(unavailable.context.contains("geometry is reported"));
        assert!(
            unavailable
                .context
                .contains("isolated test working set is unavailable")
        );
    }

    #[test]
    fn targeted_availability_changes_clear_stale_scores_and_preserve_other_rows() {
        let model: slint::ModelRc<ResultRow> = Rc::new(slint::VecModel::from(vec![
            row(0, "RAM".into(), "copy".into(), "latency".into()),
            row(2, "old".into(), "old copy".into(), "old latency".into()),
            row(3, "old L3".into(), "old copy".into(), "old latency".into()),
        ]))
        .into();
        score(&model, 0, "Read", 60000.0, Some(1.0));
        for label in ["Read", "Write", "Copy", "Latency"] {
            score(&model, 2, label, 1000.0, Some(2.0));
        }
        score(&model, 3, "Read", 2000.0, Some(1.0));
        let cache_cells = model.row_data(1).unwrap().cells;
        let cells = cache_cells
            .as_any()
            .downcast_ref::<slint::VecModel<Cell>>()
            .unwrap();
        let mut cell = cells.row_data(0).unwrap();
        cell.notes = "stale note".into();
        cells.set_row_data(0, cell);
        let untouched = model.row_data(0).unwrap().cells.iter().collect::<Vec<_>>();
        let target = Target {
            level: 2,
            column: Some(0),
        };
        let model = refresh_rows(
            model,
            Rc::new(slint::VecModel::from(vec![
                row(0, "fresh RAM".into(), "copy".into(), "latency".into()),
                unavailable_row(2, &hardware()),
                unavailable_row(3, &hardware()),
            ]))
            .into(),
            Some(target),
        );
        assert!(!target.available(&model));
        assert!(!model.row_data(2).unwrap().available);
        assert!(
            model
                .row_data(2)
                .unwrap()
                .cells
                .iter()
                .all(|cell| cell.value.is_empty())
        );
        assert!(
            model
                .row_data(1)
                .unwrap()
                .cells
                .iter()
                .all(|cell| cell.value.is_empty() && cell.notes.is_empty())
        );
        assert!(has_results(&model));
        assert!(memory_notes(&model).is_empty());
        let model = refresh_rows(
            model,
            Rc::new(slint::VecModel::from(vec![
                row(2, "new".into(), "copy".into(), "latency".into()),
                row(3, "new L3".into(), "copy".into(), "latency".into()),
            ]))
            .into(),
            Some(target),
        );
        assert!(target.available(&model));
        assert!(model.row_data(2).unwrap().available);
        assert!(
            model
                .row_data(2)
                .unwrap()
                .cells
                .iter()
                .all(|cell| cell.value.is_empty())
        );
        assert!(
            model
                .row_data(1)
                .unwrap()
                .cells
                .iter()
                .all(|cell| cell.value.is_empty())
        );
        score(&model, 2, "Read", 500000.0, Some(3.0));
        assert_eq!(
            model.row_data(1).unwrap().cells.row_data(0).unwrap().value,
            "500.0"
        );
        assert_eq!(
            model.row_data(0).unwrap().cells.iter().collect::<Vec<_>>(),
            untouched
        );
    }

    #[test]
    fn memory_notes_follow_selected_cells_and_read_uses_cached_loads() {
        let hardware = hardware();
        let options = cli::Options {
            test: "read".into(),
            threads: Some(2),
            size: Some(64 << 20),
            latency_size: Some(64 << 20),
            ..Default::default()
        };
        let (spec, cpus) = crate::resolve(&options, &hardware).unwrap();
        let model = rows(&options, &spec, &cpus, &[], &hardware, false);
        let cells = model.row_data(0).unwrap().cells;
        let read = cells.row_data(0).unwrap();
        assert!(read.info.contains("Cached loads"));
        assert!(!read.info.contains("streaming"));
        assert!(read.notes.contains("RAM bandwidth:"));
        assert!((1..4).all(|column| cells.row_data(column).unwrap().notes.is_empty()));
        assert!(!memory_notes(&model).contains("RAM latency:"));

        let options = cli::Options {
            test: "all".into(),
            ..options
        };
        let model = rows(&options, &spec, &cpus, &[], &hardware, false);
        let notes = memory_notes(&model);
        assert_eq!(notes.matches("RAM bandwidth:").count(), 1);
        assert_eq!(notes.matches("RAM latency:").count(), 1);
    }

    #[test]
    fn notes_survive_targeted_refresh_and_clear_with_the_model() {
        let memory = row(0, "RAM".into(), "copy".into(), "latency".into());
        let cells = memory
            .cells
            .as_any()
            .downcast_ref::<slint::VecModel<Cell>>()
            .unwrap();
        for (column, notes) in [
            "old read note",
            "bandwidth note",
            "bandwidth note",
            "latency note",
        ]
        .into_iter()
        .enumerate()
        {
            let mut cell = cells.row_data(column).unwrap();
            cell.notes = notes.into();
            cells.set_row_data(column, cell);
        }
        let model: slint::ModelRc<ResultRow> = Rc::new(slint::VecModel::from(vec![
            memory,
            row(2, "cache".into(), "copy".into(), "latency".into()),
        ]))
        .into();
        let before = memory_notes(&model);
        let cache = Rc::new(slint::VecModel::from(vec![row(
            2,
            "new cache".into(),
            "copy".into(),
            "latency".into(),
        )]))
        .into();
        let model = refresh_rows(
            model,
            cache,
            Some(Target {
                level: 2,
                column: Some(0),
            }),
        );
        assert_eq!(memory_notes(&model), before);

        let fresh = row(0, "new RAM".into(), "copy".into(), "latency".into());
        let cells = fresh
            .cells
            .as_any()
            .downcast_ref::<slint::VecModel<Cell>>()
            .unwrap();
        let mut read = cells.row_data(0).unwrap();
        read.notes = "new read note".into();
        cells.set_row_data(0, read);
        let model = refresh_rows(
            model,
            Rc::new(slint::VecModel::from(vec![fresh])).into(),
            Some(Target {
                level: 0,
                column: Some(0),
            }),
        );
        let notes = memory_notes(&model);
        assert!(!notes.contains("old read note"));
        assert!(notes.contains("new read note") && notes.contains("latency note"));
        assert_eq!(notes.matches("bandwidth note").count(), 1);

        let empty = Rc::new(slint::VecModel::from(Vec::<ResultRow>::new())).into();
        assert!(memory_notes(&refresh_rows(model, empty, None)).is_empty());
    }

    #[test]
    fn result_units_and_row_updates_remain_isolated_and_reset() {
        assert_eq!(value_text(0, "Read", 56000.0), "56000");
        assert_eq!(value_text(2, "Read", 56000.0), "56.0");
        assert_eq!(value_text(3, "Latency", 1.26), "1.3");
        let options = cli::Options::default();
        let spec = profile::Spec {
            threads: 2,
            cache_threads: 2,
            stores: portable::Stores::Streaming,
            bandwidth_bytes: 384 << 20,
            latency_bytes: 512 << 20,
            stride: 128,
            window: 0,
            seed: 1,
            samples: 3,
            min_time: 0.001,
        };
        let plans = [cache::Plan {
            kind: cache::Kind::Cpu,
            level: 2,
            capacity_bytes: 512 << 10,
            working_set_bytes: 256 << 10,
            bandwidth_bytes: 512 << 10,
            cpus: vec![0, 1],
            stride: 64,
            cpu: 0,
        }];
        let preview = rows(&options, &spec, &[0, 1], &plans, &hardware(), false);
        assert!(
            preview
                .row_data(0)
                .unwrap()
                .context
                .contains("up to 2 workers")
        );
        let model = rows(&options, &spec, &[0, 1], &plans, &hardware(), true);
        assert!(model.row_data(0).unwrap().context.contains("2 workers"));
        assert!(!model.row_data(0).unwrap().context.contains("up to"));
        assert_eq!(model.row_count(), 4);
        score(&model, 2, "Read", 512000.0, Some(2.5));
        let memory = model.row_data(0).unwrap();
        let cache = model.row_data(2).unwrap();
        assert_eq!(memory.cells.row_data(0).unwrap().value, "");
        assert_eq!(cache.cells.row_data(0).unwrap().value, "512.0");
        assert!(cache.cells.row_data(0).unwrap().info.contains("2.5%"));
        assert_eq!(cache.cells.row_data(2).unwrap().value, "");
        assert!(
            cache
                .cells
                .row_data(2)
                .unwrap()
                .info
                .contains("combined bytes read + written")
        );
        let reset = rows(&options, &spec, &[0, 1], &plans, &hardware(), true);
        assert!(reset.iter().all(|row| {
            row.cells
                .iter()
                .all(|cell| cell.value.is_empty() && !cell.info.contains("sample range"))
        }));
        let selected = cli::Options {
            level: "l2".into(),
            ..options
        };
        assert_eq!(displayed_workers(&selected, &[0, 1, 2, 3], &plans), 2);
        assert!(details(&spec, 2, false, "l2").contains("cache-line stride"));
        assert_eq!(
            rows(&selected, &spec, &[0, 1], &plans, &hardware(), true).row_count(),
            1
        );

        score(&model, 0, "Write", 90000.0, Some(1.0));
        let untouched = model.row_data(0).unwrap().cells.row_data(1).unwrap();
        let target = Target {
            level: 2,
            column: Some(0),
        };
        let model = refresh_rows(
            model,
            rows(&selected, &spec, &[0, 1], &plans, &hardware(), true),
            Some(target),
        );
        assert_eq!(model.row_count(), 4);
        assert_eq!(
            model.row_data(2).unwrap().context,
            "Hover a result for its tested settings."
        );
        assert_eq!(
            model.row_data(0).unwrap().cells.row_data(1).unwrap(),
            untouched
        );
        let cleared = model.row_data(2).unwrap().cells.row_data(0).unwrap();
        assert!(cleared.value.is_empty());
        assert!(!cleared.info.contains("sample range"));
        score(&model, 2, "Read", 600000.0, Some(1.5));
        assert!(has_results(&model));
        assert_eq!(
            model.row_data(0).unwrap().cells.row_data(1).unwrap().value,
            "90000"
        );
        let refreshed = model.row_data(2).unwrap().cells.row_data(0).unwrap();
        assert_eq!(refreshed.value, "600.0");
        assert_eq!(refreshed.info.matches("Median result;").count(), 1);
        assert!(refreshed.info.contains("1.5%"));

        let mut options = cli::Options {
            cpus: Some(vec![0, 1]),
            threads: Some(2),
            save_profile: Some("fixed.profile".into()),
            seed: Some(42),
            ..Default::default()
        };
        let mut plans = plans;
        plans[0].cpu = 1;
        Target {
            level: 2,
            column: Some(3),
        }
        .options(&mut options, &spec, &[0, 1], &plans, "pinned")
        .unwrap();
        assert_eq!(options.level, "l2");
        assert_eq!(options.test, "latency");
        assert_eq!(options.cpus, Some(vec![1]));
        assert_eq!(options.threads, Some(1));
        assert_eq!(options.size, Some(spec.bandwidth_bytes));
        assert_eq!(options.latency_size, Some(spec.latency_bytes));
        assert_eq!(options.seed, Some(42));
        assert_eq!(options.save_profile, None);
        Target {
            level: 2,
            column: Some(3),
        }
        .options(&mut options, &spec, &[0, 1], &plans, "scheduler")
        .unwrap();
        assert_eq!(options.cpus, None);
    }
    #[test]
    fn target_mapping_accepts_full_run_row_or_cell() {
        assert_eq!(Target::parse(-1, -1).unwrap(), None);
        for level in 0..4 {
            for column in 0..4 {
                assert_eq!(
                    Target::parse(level, column).unwrap(),
                    Some(Target {
                        level: level as u8,
                        column: Some(column as usize),
                    })
                );
            }
            assert_eq!(
                Target::parse(level, -1).unwrap(),
                Some(Target {
                    level: level as u8,
                    column: None,
                })
            );
        }
        for (level, column) in [(-1, 0), (0, -2), (4, 0), (0, 4)] {
            assert!(Target::parse(level, column).is_err());
        }
    }

    #[test]
    fn targeted_copy_budgets_both_streams_before_freezing_auto_sizes() {
        let hardware = platform::Hardware {
            cpus: vec![0, 1],
            preferred: vec![0, 1],
            cache_bytes: 0,
            cache_total_bytes: 0,
            memory_budget: 64 << 20,
            affinity: "hard",
            page_size: 4096,
            cache_line_bytes: 64,
            slc_capacity: None,
            caches: Vec::new(),
        };
        let base = cli::Options {
            test: "read".into(),
            level: "memory".into(),
            threads: Some(2),
            cpus: Some(vec![1, 0]),
            ..Default::default()
        };
        let (read, _) = crate::resolve(&base, &hardware).unwrap();
        assert_eq!(read.bandwidth_bytes, hardware.memory_budget);
        for column in [Some(2), None] {
            let target = Target { level: 0, column };
            let mut options = base.clone();
            target.prepare(&mut options);
            let (spec, cpus) = crate::resolve(&options, &hardware).unwrap();
            assert_eq!(spec.bandwidth_bytes * 2, hardware.memory_budget);
            target
                .options(&mut options, &spec, &cpus, &[], hardware.affinity)
                .unwrap();
            let (frozen, cpus) = crate::resolve(&options, &hardware).unwrap();
            assert_eq!(frozen.bandwidth_bytes, spec.bandwidth_bytes);
            assert_eq!(cpus, vec![1, 0]);
            assert_eq!(options.threads, base.threads);
        }
        let mut explicit = base;
        explicit.size = Some(64 << 20);
        Target {
            level: 0,
            column: Some(2),
        }
        .prepare(&mut explicit);
        assert_eq!(
            crate::resolve(&explicit, &hardware)
                .unwrap()
                .0
                .bandwidth_bytes,
            64 << 20
        );
    }

    #[test]
    fn row_refresh_replaces_all_four_cells_and_keeps_other_rows() {
        let model: slint::ModelRc<ResultRow> = Rc::new(slint::VecModel::from(
            (0..=3)
                .map(|level| row(level, "old".into(), "old copy".into(), "old latency".into()))
                .collect::<Vec<_>>(),
        ))
        .into();
        for level in 0..=3 {
            for label in ["Read", "Write", "Copy", "Latency"] {
                score(&model, level, label, 100000.0, Some(1.5));
            }
        }
        let untouched: Vec<_> = model
            .iter()
            .filter(|row| row.level != 2)
            .flat_map(|row| row.cells.iter().collect::<Vec<_>>())
            .collect();
        let fresh = row(2, "new".into(), "new copy".into(), "new latency".into());
        let model = refresh_rows(
            model,
            Rc::new(slint::VecModel::from(vec![fresh.clone()])).into(),
            Some(Target {
                level: 2,
                column: None,
            }),
        );
        assert_eq!(model.row_count(), 4);
        assert_eq!(model.row_data(2).unwrap().context, fresh.context);
        assert_eq!(
            model.row_data(2).unwrap().cells.iter().collect::<Vec<_>>(),
            fresh.cells.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            model
                .iter()
                .filter(|row| row.level != 2)
                .flat_map(|row| row.cells.iter().collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            untouched
        );
        assert!(has_results(&model));
        assert_eq!(
            model
                .iter()
                .flat_map(|row| row.cells.iter().collect::<Vec<_>>())
                .filter(|cell| !cell.value.is_empty())
                .count(),
            12
        );
    }
    #[test]
    fn profile_workers_distinguish_auto_inherited_and_explicit_choices() {
        for (threads, expected_choice) in [(None, ""), (Some(0), "0"), (Some(2), "2")] {
            let selected = initial_choice(threads).unwrap_or_default();
            let (model, index) = choices(
                &selected,
                "Auto (8 max)".into(),
                Some("Fixed (4)".into()),
                [1, 2, 4, 8],
                "",
            );
            assert_eq!(
                model.row_data(index as usize).unwrap().value,
                expected_choice
            );
        }
        let base = cli::Options {
            threads: Some(2),
            profile: Some("fixed.profile".into()),
            size: Some(123456128),
            latency_size: Some(123456128),
            ..Default::default()
        };
        let inherited = fields(&base, "", "", "", false).unwrap();
        assert_eq!(inherited.threads, None);
        assert_eq!(inherited.size, base.size);
        assert_eq!(inherited.latency_size, base.latency_size);
        let automatic = fields(&base, "0", "", "", false).unwrap();
        assert_eq!(automatic.threads, Some(0));
        assert!(crate::calibrates_workers(&automatic));
        assert!(!crate::calibrates_workers(&inherited));
        assert_eq!(fields(&base, "8", "", "", false).unwrap().threads, Some(8));
    }
    #[test]
    fn dropdowns_preserve_exact_and_custom_choices() {
        use slint::Model;
        let (model, index) = choices("96", "Auto (384 MiB)".into(), None, [64, 96, 128], " MiB");
        assert_eq!(model.row_count(), 4);
        assert_eq!(model.row_data(index as usize).unwrap().value, "96");
        let (model, index) = choices(
            "",
            "Auto (384 MiB)".into(),
            Some("Fixed (117.7 MiB)".into()),
            [64, 128],
            " MiB",
        );
        assert_eq!(model.row_data(index as usize).unwrap().value, "");
        let base = cli::Options {
            size: Some(123456128),
            ..Default::default()
        };
        assert_eq!(
            fields(
                &base,
                "",
                &model.row_data(index as usize).unwrap().value,
                "",
                false
            )
            .unwrap()
            .size,
            base.size
        );
        let (model, index) = choices(
            "0",
            "Auto (384 MiB)".into(),
            Some("Fixed (117.7 MiB)".into()),
            [64, 128],
            " MiB",
        );
        assert_eq!(index, 0);
        assert_eq!(model.row_data(0).unwrap().value, "0");
        assert_eq!(model.row_data(1).unwrap().value, "");
    }
    #[test]
    fn edits_preserve_hidden_settings_and_validate_numbers() {
        let base = cli::Options {
            test: "all".into(),
            seed: Some(42),
            ..Default::default()
        };
        let edited = fields(&base, "3", "384", "512", true).unwrap();
        assert_eq!(
            (
                edited.threads,
                edited.size,
                edited.latency_size,
                edited.seed
            ),
            (Some(3), Some(384 << 20), Some(512 << 20), Some(42))
        );
        assert!(edited.quick);
        assert!(fields(&base, "bad", "", "", false).is_err());
        assert!(fields(&base, "", &usize::MAX.to_string(), "", false).is_err());
    }
    #[test]
    fn blank_fields_keep_exact_profile_and_zero_requests_auto() {
        let base = cli::Options {
            test: "all".into(),
            size: Some(123456128),
            profile: Some("fixed.profile".into()),
            ..Default::default()
        };
        assert_eq!(fields(&base, "", "", "", false).unwrap(), base);
        assert_eq!(fields(&base, "0", "0", "", false).unwrap().size, Some(0));
        assert!(fields(&base, "", "", "", true).is_err());
    }
}
