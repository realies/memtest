//! Paired cache-capacity controls, deliberately without a residency classifier.
use crate::{cli, platform, portable};
use std::fmt::Write;

struct Controls {
    cpu: usize,
    lower: usize,
    outer: usize,
    source: &'static str,
    samples: usize,
}

fn controls(options: &cli::Options, hardware: &platform::Hardware) -> Result<Controls, String> {
    let cpu = match options.cpus.as_deref() {
        Some([cpu]) => *cpu,
        Some(_) => return Err("cache probe requires one CPU ID".into()),
        None => *hardware
            .preferred
            .first()
            .or(hardware.cpus.first())
            .ok_or("no available CPU")?,
    };
    if !hardware.cpus.contains(&cpu) {
        return Err(format!("cache probe CPU {cpu} is unavailable"));
    }
    let applicable = |cache: &&platform::Cache| cache.cpus.is_empty() || cache.cpus.contains(&cpu);
    let lower = hardware
        .caches
        .iter()
        .filter(applicable)
        .filter(|cache| cache.level == 2 && cache.bytes != 0)
        .map(|cache| cache.bytes)
        .max()
        .ok_or("cache probe requires a reported L2 capacity")?;
    let reported_outer = hardware
        .caches
        .iter()
        .filter(applicable)
        .filter(|cache| cache.level > 2 && cache.bytes > lower)
        .map(|cache| cache.bytes)
        .max();
    let (outer, source) = if let Some(bytes) = hardware.slc_capacity.filter(|&bytes| bytes > lower)
    {
        (bytes, "published_model")
    } else if let Some(bytes) = reported_outer {
        (bytes, "os")
    } else {
        (
            lower
                .checked_mul(4)
                .ok_or("cache probe control size overflow")?,
            "fallback_control",
        )
    };
    let samples = options
        .samples
        .unwrap_or(if options.quick { 5 } else { 21 });
    if !(1..=100).contains(&samples) {
        return Err("cache probe requires 1..100 samples".into());
    }
    Ok(Controls {
        cpu,
        lower,
        outer,
        source,
        samples,
    })
}

pub fn run(options: &cli::Options) -> Result<(), String> {
    let hardware = platform::hardware();
    let controls = controls(options, &hardware)?;
    let result = portable::cache_probe(
        controls.lower,
        controls.outer,
        hardware.page_size,
        hardware.cache_line_bytes,
        controls.cpu,
        controls.samples,
        hardware.memory_budget,
    )?;
    if options.json {
        println!("{}", render(&controls, &hardware, &result)?);
    } else {
        println!("Cache probe: {} trials per condition", controls.samples);
        let placement = if hardware.affinity == "scheduler" {
            "OS-scheduled".into()
        } else {
            format!("CPU {} ({})", controls.cpu, hardware.affinity)
        };
        let qos = if cfg!(target_os = "macos") {
            "; user-initiated QoS floor"
        } else {
            ""
        };
        println!("Placement: {placement}{qos}");
        println!(
            "Capacity controls: L2 {:.1} MiB; outer {:.1} MiB ({})",
            controls.lower as f64 / (1 << 20) as f64,
            controls.outer as f64 / (1 << 20) as f64,
            controls.source
        );
        println!(
            "Target: {} bytes / {} nodes; {} B lines / {} B base pages",
            result.target_bytes, result.nodes, result.cache_line_bytes, result.page_bytes
        );
        println!(
            "Eviction walks: {} / {} bytes",
            result.lower_eviction_bytes, result.cold_eviction_bytes
        );
        for (label, measurement, _) in conditions(&result) {
            println!(
                "{label:<18} {:>9.3} ns  {:>6.1}% range",
                measurement.value, measurement.spread_percent
            );
        }
        println!("Conditions do not prove cache residency or isolate SLC hits.");
    }
    Ok(())
}

fn conditions(result: &portable::CacheProbe) -> [(&'static str, portable::Measurement, &[f64]); 3] {
    [
        ("hot", result.hot, &result.hot_samples),
        (
            "capacity_evicted",
            result.capacity_evicted,
            &result.capacity_evicted_samples,
        ),
        ("cold", result.cold, &result.cold_samples),
    ]
}

fn quoted(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            c if c <= '\u{1f}' => write!(output, "\\u{:04x}", c as u32).unwrap(),
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

fn render(
    controls: &Controls,
    hardware: &platform::Hardware,
    result: &portable::CacheProbe,
) -> Result<String, String> {
    let mut measurements = Vec::new();
    for (label, measurement, samples) in conditions(result) {
        if samples.len() != controls.samples
            || !measurement.value.is_finite()
            || !measurement.spread_percent.is_finite()
            || samples.iter().any(|value| !value.is_finite())
        {
            return Err("cache probe output contains nonfinite or incomplete samples".into());
        }
        let samples = samples
            .iter()
            .map(f64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        measurements.push(format!(
            "{}: {{\"ns\": {}, \"range_percent\": {}, \"samples_ns\": [{}]}}",
            quoted(label),
            measurement.value,
            measurement.spread_percent,
            samples
        ));
    }
    let (affinity, cpu) = match hardware.affinity {
        "hard" => ("pinned", controls.cpu.to_string()),
        "hard/current-group" => ("pinned_current_group", controls.cpu.to_string()),
        _ => ("os_scheduled", "null".into()),
    };
    let qos = if cfg!(target_os = "macos") {
        "\"user_initiated\""
    } else {
        "null"
    };
    Ok(format!(
        r#"{{
  "schema": 1,
  "diagnostic": "cache_probe",
  "version": {},
  "os": {},
  "arch": {},
  "affinity": {},
  "cpu_id": {cpu},
  "qos_floor": {qos},
  "samples_per_condition": {},
  "lower_capacity_bytes": {},
  "outer_capacity_bytes": {},
  "outer_capacity_source": {},
  "outer_capacity_known": {},
  "memory_budget_bytes": {},
  "target_bytes": {},
  "nodes": {},
  "lower_eviction_bytes": {},
  "cold_eviction_bytes": {},
  "page_bytes": {},
  "cache_line_bytes": {},
  "results": {{{}}},
  "notes": "Conditions rotate per trial. Page translations are warmed separately. Capacity walks do not guarantee eviction or SLC residency; timing includes translation and scheduling effects."
}}"#,
        quoted(env!("CARGO_PKG_VERSION")),
        quoted(std::env::consts::OS),
        quoted(std::env::consts::ARCH),
        quoted(affinity),
        controls.samples,
        controls.lower,
        controls.outer,
        quoted(controls.source),
        controls.source != "fallback_control",
        hardware.memory_budget,
        result.target_bytes,
        result.nodes,
        result.lower_eviction_bytes,
        result.cold_eviction_bytes,
        result.page_bytes,
        result.cache_line_bytes,
        measurements.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hardware() -> platform::Hardware {
        platform::Hardware {
            cpus: vec![0, 2],
            preferred: vec![2],
            slc_capacity: None,
            caches: vec![platform::Cache {
                level: 2,
                bytes: 12 << 20,
                line_bytes: 128,
                cpus: Vec::new(),
            }],
            cache_bytes: 12 << 20,
            cache_total_bytes: 24 << 20,
            cache_line_bytes: 128,
            memory_budget: 1 << 30,
            affinity: "scheduler",
            page_size: 16384,
        }
    }

    #[test]
    fn capacity_sources_and_explicit_cpu_are_unambiguous() {
        let mut h = hardware();
        let mut options = cli::Options::default();
        let c = controls(&options, &h).unwrap();
        assert_eq!(
            (c.cpu, c.outer, c.source, c.samples),
            (2, 48 << 20, "fallback_control", 21)
        );
        options.quick = true;
        h.slc_capacity = Some(48 << 20);
        assert_eq!(controls(&options, &h).unwrap().source, "published_model");
        assert_eq!(controls(&options, &h).unwrap().samples, 5);
        h.slc_capacity = None;
        h.caches.push(platform::Cache {
            level: 3,
            bytes: 32 << 20,
            line_bytes: 128,
            cpus: vec![0],
        });
        options.cpus = Some(vec![0]);
        assert_eq!(controls(&options, &h).unwrap().source, "os");
        options.cpus = Some(vec![2]);
        assert_eq!(controls(&options, &h).unwrap().source, "fallback_control");
        options.cpus = Some(vec![999]);
        assert!(controls(&options, &h).is_err());
        h.caches.clear();
        assert!(controls(&cli::Options::default(), &h).is_err());
    }

    #[test]
    fn json_keeps_raw_order_and_rejects_nonfinite_or_missing_samples() {
        let h = hardware();
        let options = cli::Options {
            samples: Some(3),
            ..Default::default()
        };
        let c = controls(&options, &h).unwrap();
        let m = portable::Measurement {
            value: 2.0,
            spread_percent: 100.0,
        };
        let mut result = portable::CacheProbe {
            target_bytes: 256 << 10,
            nodes: 2016,
            lower_eviction_bytes: 24 << 20,
            cold_eviction_bytes: 240 << 20,
            page_bytes: 16384,
            cache_line_bytes: 128,
            hot: m,
            capacity_evicted: m,
            cold: m,
            hot_samples: vec![3.0, 1.0, 2.0],
            capacity_evicted_samples: vec![1.0, 2.0, 3.0],
            cold_samples: vec![2.0, 3.0, 1.0],
        };
        let output = render(&c, &h, &result).unwrap();
        assert!(output.contains("\"samples_ns\": [3, 1, 2]"));
        assert!(output.contains("\"outer_capacity_known\": false"));
        assert!(output.contains("\"cpu_id\": null"));
        result.hot_samples[0] = f64::NAN;
        assert!(render(&c, &h, &result).is_err());
        result.hot_samples[0] = 3.0;
        result.cold.spread_percent = f64::INFINITY;
        assert!(render(&c, &h, &result).is_err());
        result.cold.spread_percent = 100.0;
        result.cold_samples.pop();
        assert!(render(&c, &h, &result).is_err());
        assert_eq!(quoted("a\n\"\\\0"), "\"a\\u000a\\\"\\\\\\u0000\"");
    }
}
