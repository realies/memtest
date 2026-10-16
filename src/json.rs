//! Dependency-free JSON output. Nonfinite numbers are rejected before serialization.
use crate::{
    platform::Hardware,
    portable::{Backend, Measurement},
    profile::Spec,
};
use std::fmt::Write;

#[derive(Default)]
pub struct Results {
    pub read: Option<Measurement>,
    pub write: Option<Measurement>,
    pub copy: Option<Measurement>,
    pub latency: Option<Measurement>,
}

fn quoted(text: &str) -> String {
    let mut output = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            c if c <= '\u{1f}' => write!(output, "\\u{:04x}", c as u32).unwrap(),
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

fn measurements(results: &Results, latency_cpu: &str) -> Result<String, String> {
    let mut fields = Vec::new();
    for (name, value) in [
        ("read", results.read),
        ("write", results.write),
        ("copy", results.copy),
        ("latency", results.latency),
    ] {
        if let Some(value) = value {
            if !value.value.is_finite() || !value.spread_percent.is_finite() {
                return Err("cannot encode nonfinite measurement as JSON".into());
            }
            let unit = if name == "latency" { "ns" } else { "mb_per_s" };
            let cpu = if name == "latency" {
                format!(", \"cpu_id\": {latency_cpu}")
            } else {
                String::new()
            };
            fields.push(format!(
                "{}: {{\"{unit}\": {}, \"range_percent\": {}{cpu}}}",
                quoted(name),
                value.value,
                value.spread_percent
            ));
        }
    }
    Ok(fields.join(", "))
}

pub fn render(
    spec: &Spec,
    hardware: &Hardware,
    cpus: &[usize],
    worker_selection: &str,
    backend: Backend,
    results: &Results,
    caches: &[crate::cache::Result],
) -> Result<String, String> {
    if !spec.min_time.is_finite() {
        return Err("cannot encode nonfinite minimum sample time as JSON".into());
    }
    let affinity = match hardware.affinity {
        "hard" => "pinned",
        "hard/current-group" => "pinned_current_group",
        _ => "os_scheduled",
    };
    let scheduled = affinity == "os_scheduled";
    let cpu_ids = if scheduled {
        "null".to_owned()
    } else {
        format!(
            "[{}]",
            cpus.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let latency_cpu = if scheduled {
        "null".to_owned()
    } else {
        cpus.first()
            .map(usize::to_string)
            .unwrap_or_else(|| "null".into())
    };
    let mut cache_rows = Vec::new();
    let warnings = crate::memory::warnings(
        hardware,
        spec,
        results.read.is_some() || results.write.is_some() || results.copy.is_some(),
        results.latency.is_some(),
    )
    .iter()
    .map(|message| quoted(message))
    .collect::<Vec<_>>()
    .join(", ");
    let latency_footprint = results
        .latency
        .and_then(|_| crate::memory::latency_footprint(spec, hardware.cache_line_bytes))
        .map_or_else(|| "null".into(), |bytes| bytes.to_string());
    for row in caches {
        let p = &row.plan;
        let cpu = if scheduled {
            "null".into()
        } else {
            p.cpu.to_string()
        };
        let worker_cpus = if scheduled {
            "null".into()
        } else {
            format!("{:?}", p.cpus)
        };
        cache_rows.push(format!(
            "{{\"level\": {}, \"capacity_bytes\": {}, \"working_set_bytes\": {}, \"bandwidth_bytes\": {}, \"copy_bytes_per_stream\": {}, \"stride\": {}, \"window\": 0, \"stores\": \"cached\", \"threads\": {}, \"cpu_ids\": {}, \"cpu_id\": {}, \"results\": {{{}}}}}",
            p.level, p.capacity_bytes, p.working_set_bytes, p.bandwidth_bytes, p.bandwidth_bytes / 2, p.stride, p.cpus.len(), worker_cpus, cpu,
            measurements(&row.results, &cpu)?));
    }
    Ok(format!(
        r#"{{
  "schema": 3,
  "method": {},
  "version": {},
  "os": {},
  "arch": {},
  "backend": {},
  "store_implementation": {},
  "affinity": {},
  "available_cpus": {},
  "cpu_ids": {},
  "worker_selection": {},
  "page_bytes": {},
  "cache_line_bytes": {},
  "reported_cache_bytes": {},
  "reported_cache_total_bytes": {},
  "latency_cache_footprint_bytes": {},
  "warnings": [{}],
  "copy_convention": "read_plus_write_bytes",
  "memory_copy_buffers": 2,
  "config": {{
    "threads": {},
    "cache_threads": {},
    "stores": {},
    "bandwidth_bytes": {},
    "latency_bytes": {},
    "stride": {},
    "window": {},
    "seed": {},
    "samples": {},
    "min_time": {}
  }},
  "results": {{{}}},
  "caches": [{}]
}}
"#,
        crate::profile::METHOD,
        quoted(env!("CARGO_PKG_VERSION")),
        quoted(std::env::consts::OS),
        quoted(std::env::consts::ARCH),
        quoted(backend.name()),
        quoted(spec.stores.name(backend)),
        quoted(affinity),
        hardware.cpus.len(),
        cpu_ids,
        quoted(worker_selection),
        hardware.page_size,
        hardware.cache_line_bytes,
        hardware.cache_bytes,
        hardware.cache_total_bytes,
        latency_footprint,
        warnings,
        spec.threads,
        spec.cache_threads,
        quoted(spec.stores.key()),
        spec.bandwidth_bytes,
        spec.latency_bytes,
        spec.stride,
        spec.window,
        spec.seed,
        spec.samples,
        spec.min_time,
        measurements(results, &latency_cpu)?,
        cache_rows.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Spec, Hardware) {
        (
            Spec {
                threads: 2,
                cache_threads: 2,
                stores: crate::portable::Stores::Streaming,
                bandwidth_bytes: 4096,
                latency_bytes: 4096,
                stride: 128,
                window: 0,
                seed: u64::MAX,
                samples: 3,
                min_time: 0.001,
            },
            Hardware {
                cpus: vec![2, 4, 6],
                preferred: vec![2, 4],
                cache_bytes: 0,
                cache_total_bytes: 0,
                cache_line_bytes: 64,
                caches: Vec::new(),
                memory_budget: 1 << 20,
                affinity: "hard",
                page_size: 4096,
            },
        )
    }
    #[test]
    fn escapes_strings_and_controls() {
        assert_eq!(
            quoted("\"\\\n\r\t\u{8}\u{c}\0\u{1f}λ"),
            r#""\"\\\n\r\t\b\f\u0000\u001fλ""#
        );
    }
    #[test]
    fn optional_results_and_cpu_identity() {
        let (spec, mut hardware) = fixture();
        let empty = render(
            &spec,
            &hardware,
            &[2, 4],
            "fixed",
            Backend::Sse2,
            &Results::default(),
            &[],
        )
        .unwrap();
        assert!(empty.contains("\"results\": {}"));
        assert!(empty.contains("\"available_cpus\": 3"));
        assert!(empty.contains("\"cpu_ids\": [2, 4]"));
        assert!(empty.contains("\"worker_selection\": \"fixed\""));
        assert!(empty.contains("\"backend\": \"SSE2\""));
        assert!(empty.contains("\"stores\": \"streaming\""));
        assert!(empty.contains("\"seed\": 18446744073709551615"));
        let results = Results {
            read: Some(Measurement {
                value: 123.25,
                spread_percent: 4.5,
            }),
            latency: Some(Measurement {
                value: 10.0,
                spread_percent: 2.0,
            }),
            ..Results::default()
        };
        let pinned = render(
            &spec,
            &hardware,
            &[2, 4],
            "calibrated",
            Backend::Sse2,
            &results,
            &[],
        )
        .unwrap();
        assert!(pinned.contains("\"read\": {\"mb_per_s\": 123.25, \"range_percent\": 4.5}"));
        assert!(pinned.contains("\"worker_selection\": \"calibrated\""));
        assert!(!pinned.contains("\"write\""));
        assert!(pinned.contains("\"cpu_id\": 2"));
        hardware.affinity = "hard/current-group";
        assert!(
            render(
                &spec,
                &hardware,
                &[2, 4],
                "fixed",
                Backend::Sse2,
                &results,
                &[]
            )
            .unwrap()
            .contains("\"affinity\": \"pinned_current_group\"")
        );
        hardware.affinity = "scheduler";
        let scheduled = render(
            &spec,
            &hardware,
            &[2, 4],
            "fixed",
            Backend::Sse2,
            &results,
            &[],
        )
        .unwrap();
        assert!(scheduled.contains("\"affinity\": \"os_scheduled\""));
        assert!(scheduled.contains("\"cpu_ids\": null"));
        assert!(scheduled.contains("\"cpu_id\": null"));
    }
    #[test]
    fn memory_notes_follow_measured_tests_and_preserve_footprints() {
        let (spec, mut hardware) = fixture();
        hardware.cache_bytes = 4096;
        hardware.cache_total_bytes = 8192;
        let results = Results {
            latency: Some(Measurement {
                value: 10.0,
                spread_percent: 1.0,
            }),
            ..Results::default()
        };
        let text = render(
            &spec,
            &hardware,
            &[2],
            "fixed",
            Backend::Sse2,
            &results,
            &[],
        )
        .unwrap();
        assert!(text.contains("\"reported_cache_bytes\": 4096"));
        assert!(text.contains("\"reported_cache_total_bytes\": 8192"));
        assert!(text.contains("\"latency_cache_footprint_bytes\": 2048"));
        assert!(text.contains("RAM latency:"));
        assert!(!text.contains("RAM bandwidth:"));
        let empty = render(
            &spec,
            &hardware,
            &[2],
            "fixed",
            Backend::Sse2,
            &Results::default(),
            &[],
        )
        .unwrap();
        assert!(empty.contains("\"latency_cache_footprint_bytes\": null"));
        assert!(empty.contains("\"warnings\": []"));
    }
    #[test]
    fn rejects_nonfinite_numbers() {
        let (mut spec, hardware) = fixture();
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            spec.min_time = invalid;
            assert!(
                render(
                    &spec,
                    &hardware,
                    &[2, 4],
                    "fixed",
                    Backend::Sse2,
                    &Results::default(),
                    &[],
                )
                .is_err()
            );
            spec.min_time = 0.001;
            for measurement in [
                Measurement {
                    value: invalid,
                    spread_percent: 0.0,
                },
                Measurement {
                    value: 1.0,
                    spread_percent: invalid,
                },
            ] {
                for field in 0..4 {
                    let mut results = Results::default();
                    match field {
                        0 => results.read = Some(measurement),
                        1 => results.write = Some(measurement),
                        2 => results.copy = Some(measurement),
                        _ => results.latency = Some(measurement),
                    }
                    assert!(
                        render(
                            &spec,
                            &hardware,
                            &[2, 4],
                            "fixed",
                            Backend::Sse2,
                            &results,
                            &[]
                        )
                        .is_err()
                    );
                }
            }
        }
    }
    #[test]
    fn cache_results_preserve_workload_units_and_scheduling() {
        let (spec, mut h) = fixture();
        let mut caches = vec![crate::cache::Result {
            plan: crate::cache::Plan {
                level: 1,
                capacity_bytes: 32768,
                working_set_bytes: 16384,
                bandwidth_bytes: 32768,
                cpus: vec![0, 1],
                stride: 64,
                cpu: 2,
            },
            results: Results {
                copy: Some(Measurement {
                    value: 400000.0,
                    spread_percent: 1.0,
                }),
                ..Default::default()
            },
        }];
        let text = render(
            &spec,
            &h,
            &[2],
            "fixed",
            Backend::Sse2,
            &Results::default(),
            &caches,
        )
        .unwrap();
        assert!(text.contains("\"copy_convention\": \"read_plus_write_bytes\""));
        assert!(text.contains("\"working_set_bytes\": 16384"));
        assert!(text.contains("\"copy_bytes_per_stream\": 16384"));
        assert!(text.contains("\"threads\": 2, \"cpu_ids\": [0, 1]"));
        assert!(text.contains("\"copy\": {\"mb_per_s\": 400000"));
        h.affinity = "scheduler";
        let text = render(
            &spec,
            &h,
            &[2],
            "fixed",
            Backend::Sse2,
            &Results::default(),
            &caches,
        )
        .unwrap();
        assert!(text.contains("\"cpu_id\": null"));
        caches[0].results.copy.as_mut().unwrap().value = f64::NAN;
        assert!(
            render(
                &spec,
                &h,
                &[2],
                "fixed",
                Backend::Sse2,
                &Results::default(),
                &caches
            )
            .is_err()
        );
    }
}
