//! RAM sizing diagnostics against reported caches, not proof of RAM isolation.
use crate::{platform::Hardware, profile::Spec};

fn geometry(stride: usize, line: usize) -> bool {
    (8..=128).contains(&line) && line.is_power_of_two() && stride >= 8 && stride.is_power_of_two()
}

/// Allocate enough span to touch four times the reported cache, retaining the
/// 256 MiB span floor. The caller still applies the available-memory budget.
/// Unknown line geometry retains the previous span-based sizing heuristic.
pub fn automatic_latency_span(hardware: &Hardware, stride: usize) -> usize {
    let target = hardware.cache_bytes.saturating_mul(4);
    let span = if geometry(stride, hardware.cache_line_bytes) && stride > hardware.cache_line_bytes
    {
        target
            .div_ceil(hardware.cache_line_bytes)
            .saturating_mul(stride)
    } else {
        target
    };
    (256 << 20).max(span)
}

/// Estimate whole cache lines touched by the pointer chain, including partial
/// final lines. The allocation is aligned to 128 bytes; larger or invalid line
/// sizes cannot be estimated reliably. Arithmetic overflow also returns None.
pub fn latency_footprint(spec: &Spec, cache_line_bytes: usize) -> Option<usize> {
    if !geometry(spec.stride, cache_line_bytes) {
        return None;
    }
    let nodes = spec.latency_bytes.checked_div(spec.stride)?;
    if spec.stride <= cache_line_bytes {
        nodes
            .checked_mul(spec.stride)?
            .div_ceil(cache_line_bytes)
            .checked_mul(cache_line_bytes)
    } else {
        nodes.checked_mul(cache_line_bytes)
    }
}

fn size(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
    } else if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// Report only the selected RAM workloads. Aggregate reported cache capacity is
/// a sizing heuristic: an empty warning list does not verify RAM isolation.
pub fn warnings(hardware: &Hardware, spec: &Spec, bandwidth: bool, latency: bool) -> Vec<String> {
    warnings_for(
        hardware,
        spec,
        bandwidth,
        latency,
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
    )
}

fn warnings_for(
    hardware: &Hardware,
    spec: &Spec,
    bandwidth: bool,
    latency: bool,
    apple_slc: bool,
) -> Vec<String> {
    let mut result = Vec::new();
    if bandwidth {
        if hardware.cache_total_bytes == 0 {
            result.push(
                "RAM bandwidth: aggregate cache capacity is unknown; RAM isolation is unverified."
                    .into(),
            );
        } else if (spec.bandwidth_bytes as u128) < 4 * hardware.cache_total_bytes as u128 {
            result.push(format!(
                "RAM bandwidth: {} per stream is below 4× reported aggregate cache ({}); cache may influence the result.",
                size(spec.bandwidth_bytes),
                size(hardware.cache_total_bytes),
            ));
        }
    }
    if latency {
        match latency_footprint(spec, hardware.cache_line_bytes) {
            Some(bytes) if hardware.cache_bytes != 0 => {
                if (bytes as u128) < 4 * hardware.cache_bytes as u128 {
                    result.push(format!(
                        "RAM latency: estimated touched cache footprint {} is below 4× the largest reported cache ({}); cache may influence the result.",
                        size(bytes),
                        size(hardware.cache_bytes),
                    ));
                }
            }
            _ => result.push("RAM latency: reported cache capacity or a reliable cache-line footprint is unknown; RAM isolation is unverified.".into()),
        }
    }
    if apple_slc && (bandwidth || latency) {
        result.push(
            "Apple system-level cache (SLC) capacity is not reported; RAM isolation is unverified."
                .into(),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hardware() -> Hardware {
        Hardware {
            cpus: vec![0],
            preferred: vec![0],
            caches: Vec::new(),
            cache_bytes: 16 << 20,
            cache_total_bytes: 32 << 20,
            cache_line_bytes: 64,
            memory_budget: 1 << 30,
            affinity: "hard",
            page_size: 4096,
        }
    }

    fn spec() -> Spec {
        Spec {
            threads: 1,
            cache_threads: 1,
            stores: crate::portable::Stores::Streaming,
            bandwidth_bytes: 128 << 20,
            latency_bytes: 128 << 20,
            stride: 128,
            window: 2 << 20,
            seed: 1,
            samples: 3,
            min_time: 0.01,
        }
    }

    #[test]
    fn thresholds_warn_below_four_times_reported_cache() {
        let hardware = hardware();
        let mut spec = spec();
        assert!(warnings_for(&hardware, &spec, true, true, false).is_empty());
        spec.bandwidth_bytes /= 2;
        spec.latency_bytes /= 2;
        let warnings = warnings_for(&hardware, &spec, true, true, false);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("per stream"));
        assert!(warnings[1].contains("touched cache footprint"));
        assert!(warnings.iter().all(|warning| warning.contains("below 4×")));
    }

    #[test]
    fn unknown_cache_data_does_not_imply_isolation() {
        let mut hardware = hardware();
        let spec = spec();
        hardware.cache_total_bytes = 0;
        hardware.cache_bytes = 0;
        let warnings = warnings_for(&hardware, &spec, true, true, false);
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|warning| warning.contains("unknown")));
        hardware.cache_bytes = 16 << 20;
        hardware.cache_line_bytes = 0;
        for line in [0, 1, 4, 63, 256] {
            assert_eq!(latency_footprint(&spec, line), None);
        }
        assert!(warnings_for(&hardware, &spec, false, true, false)[0].contains("unverified"));
    }

    #[test]
    fn dense_strides_count_partial_final_cache_lines() {
        let mut spec = spec();
        spec.stride = 8;
        for (span, line, expected) in [(8, 64, 64), (520, 64, 576), (520, 128, 640), (128, 64, 128)]
        {
            spec.latency_bytes = span;
            assert_eq!(latency_footprint(&spec, line), Some(expected));
        }
    }

    #[test]
    fn small_footprints_keep_exact_units_and_auto_span_saturates() {
        let hardware = hardware();
        let mut spec = spec();
        spec.latency_bytes = 32 << 10;
        let warnings = warnings_for(&hardware, &spec, false, true, false);
        assert!(warnings[0].contains("16 KiB"));
        spec.latency_bytes = 8;
        spec.stride = 8;
        assert!(warnings_for(&hardware, &spec, false, true, false)[0].contains("64 B"));
        assert_eq!(size(1032), "1032 B");
        let mut large = hardware;
        large.cache_bytes = usize::MAX;
        assert_eq!(automatic_latency_span(&large, 128), usize::MAX);
    }

    #[test]
    fn sparse_strides_reduce_effective_latency_footprint() {
        let hardware = hardware();
        let mut spec = spec();
        spec.stride = 1024;
        assert_eq!(latency_footprint(&spec, 64), Some(8 << 20));
        assert_eq!(warnings_for(&hardware, &spec, false, true, false).len(), 1);
        spec.stride = 32;
        assert_eq!(latency_footprint(&spec, 64), Some(spec.latency_bytes));
        assert!(warnings_for(&hardware, &spec, false, true, false).is_empty());
        spec.stride = 0;
        assert_eq!(latency_footprint(&spec, 64), None);
    }

    #[test]
    fn footprint_and_threshold_arithmetic_do_not_wrap() {
        let mut hardware = hardware();
        let mut spec = spec();
        spec.bandwidth_bytes = usize::MAX;
        spec.latency_bytes = usize::MAX - 7;
        spec.stride = 8;
        hardware.cache_bytes = usize::MAX;
        hardware.cache_total_bytes = usize::MAX;
        hardware.cache_line_bytes = 64;
        assert_eq!(latency_footprint(&spec, 64), None);
        assert_eq!(warnings_for(&hardware, &spec, true, true, false).len(), 2);
        spec.latency_bytes = usize::MAX - 63;
        assert_eq!(latency_footprint(&spec, 64), Some(spec.latency_bytes));
        assert_eq!(latency_footprint(&spec, usize::MAX), None);
    }

    #[test]
    fn warnings_follow_selected_scope_and_apple_slc_limit() {
        let hardware = hardware();
        let adequate = spec();
        let mut spec = adequate.clone();
        spec.bandwidth_bytes /= 2;
        spec.latency_bytes /= 2;
        assert!(warnings_for(&hardware, &spec, false, false, true).is_empty());
        assert!(warnings_for(&hardware, &spec, true, false, false)[0].starts_with("RAM bandwidth"));
        assert!(warnings_for(&hardware, &spec, false, true, false)[0].starts_with("RAM latency"));
        for (bandwidth, latency) in [(true, false), (false, true), (true, true)] {
            let warnings = warnings_for(&hardware, &adequate, bandwidth, latency, true);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("SLC"));
            assert!(warnings[0].contains("not reported"));
            assert!(warnings[0].contains("unverified"));
        }
    }
}
