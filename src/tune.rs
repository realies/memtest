//! Choose a worker count from median pilots; final measurements run separately.
use crate::portable::{self, Backend, BandwidthKind, Stores};

const SATURATION: f64 = 0.95;

fn candidates(maximum: usize) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut count = 1usize;
    while count < maximum {
        counts.push(count);
        count = count.checked_mul(2).unwrap_or(maximum);
    }
    if maximum != 0 {
        counts.push(maximum);
    }
    counts
}

fn select(
    test: &str,
    maximum: usize,
    mut measure: impl FnMut(BandwidthKind, usize) -> Result<f64, String>,
) -> Result<usize, String> {
    if maximum == 0 {
        return Err("no CPUs available for worker selection".into());
    }
    let active = match test {
        "read" => [true, false, false],
        "write" => [false, true, false],
        "copy" => [false, false, true],
        "all" => [true, true, true],
        "latency" => return Ok(1),
        _ => return Err("unknown test for worker selection".into()),
    };
    if maximum == 1 {
        return Ok(1);
    }
    let mut rows = Vec::new();
    let mut best = [0.0f64; 3];
    for count in candidates(maximum) {
        let mut rates = [0.0; 3];
        for (metric, kind) in [
            BandwidthKind::Read,
            BandwidthKind::Write,
            BandwidthKind::Copy,
        ]
        .into_iter()
        .enumerate()
        {
            if active[metric] {
                let rate = measure(kind, count)?;
                if !rate.is_finite() || rate <= 0.0 {
                    return Err("worker pilot returned a non-positive or non-finite rate".into());
                }
                rates[metric] = rate;
                best[metric] = best[metric].max(rate);
            }
        }
        rows.push((count, rates));
    }
    if let Some((count, _)) = rows.iter().find(|(_, rates)| {
        (0..3).all(|metric| !active[metric] || rates[metric] >= SATURATION * best[metric])
    }) {
        return Ok(*count);
    }
    // Incompatible bandwidth plateaus: maximize the weakest relative result.
    // Ascending counts and strict comparison retain the smaller count on ties.
    let mut winner = (1, -1.0);
    for (count, rates) in rows {
        let score = (0..3)
            .filter(|&metric| active[metric])
            .map(|metric| rates[metric] / best[metric])
            .fold(1.0f64, f64::min);
        if score > winner.1 {
            winner = (count, score);
        }
    }
    Ok(winner.0)
}

/// Pilots keep the same total buffer and access method at every candidate count.
pub fn workers(
    backend: Backend,
    stores: Stores,
    test: &str,
    cpus: &[usize],
    bytes: usize,
) -> Result<usize, String> {
    backend.validate()?;
    if test != "latency" && (bytes == 0 || !bytes.is_multiple_of(128) || bytes / 128 < cpus.len()) {
        return Err(
            "worker selection needs a 128-byte-aligned total and at least 128 bytes per CPU".into(),
        );
    }
    select(test, cpus.len(), |kind, count| {
        portable::bandwidth(
            backend,
            stores,
            kind,
            &cpus[..count],
            bytes,
            3,
            0.05,
            |_, _| {},
        )
        .map(|result| result.value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_include_exact_upper_bound_without_duplicates_or_overflow() {
        assert_eq!(candidates(0), Vec::<usize>::new());
        assert_eq!(candidates(1), vec![1]);
        assert_eq!(candidates(6), vec![1, 2, 4, 6]);
        assert_eq!(candidates(8), vec![1, 2, 4, 8]);
        let counts = candidates(usize::MAX);
        assert_eq!(counts.last(), Some(&usize::MAX));
        assert!(counts.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn selects_smallest_count_near_both_plateaus() {
        let chosen = select("all", 8, |kind, count| {
            Ok(match (kind, count) {
                (BandwidthKind::Read, 1) => 30.0,
                (BandwidthKind::Read, 2) => 70.0,
                (BandwidthKind::Read, 4) => 100.0,
                (BandwidthKind::Read, _) => 102.0,
                (BandwidthKind::Write, 1) => 60.0,
                (BandwidthKind::Write, 2) => 100.0,
                (BandwidthKind::Write, 4) => 98.0,
                (BandwidthKind::Write, _) => 96.0,
                (BandwidthKind::Copy, _) => 100.0,
            })
        })
        .unwrap();
        assert_eq!(chosen, 4);
        assert_eq!(
            select("read", 4, |_, n| Ok(if n == 1 { 95.0 } else { 100.0 })).unwrap(),
            1
        );
    }

    #[test]
    fn incompatible_plateaus_balance_relative_rates_and_tie_small() {
        let chosen = select("all", 8, |kind, count| {
            Ok(match (kind, count) {
                (BandwidthKind::Read, 1) | (BandwidthKind::Write, 8) => 100.0,
                (BandwidthKind::Write, 1) | (BandwidthKind::Read, 8) => 50.0,
                _ => 75.0,
            })
        })
        .unwrap();
        assert_eq!(chosen, 2);
    }

    #[test]
    fn measures_only_selected_metrics_in_fixed_count_order() {
        for test in ["read", "write", "copy"] {
            let mut calls = Vec::new();
            let chosen = select(test, 6, |kind, count| {
                let name = match kind {
                    BandwidthKind::Read => "read",
                    BandwidthKind::Write => "write",
                    BandwidthKind::Copy => "copy",
                };
                calls.push((name, count));
                Ok(if count == 1 { 50.0 } else { 100.0 })
            })
            .unwrap();
            assert_eq!(chosen, 2);
            assert_eq!(calls, vec![(test, 1), (test, 2), (test, 4), (test, 6)]);
        }
    }

    #[test]
    fn errors_propagate_and_single_core_skips_pilots() {
        assert_eq!(
            select("all", 4, |_, _| Err("pilot failed".into())),
            Err("pilot failed".into())
        );
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(select("read", 2, |_, _| Ok(rate)).is_err());
        }
        assert!(select("read", 0, |_, _| panic!("must not pilot")).is_err());
        assert!(select("unknown", 1, |_, _| panic!("must not pilot")).is_err());
        assert_eq!(
            select("all", 1, |_, _| panic!("must not pilot")).unwrap(),
            1
        );
        assert_eq!(
            select("latency", 8, |_, _| panic!("must not pilot")).unwrap(),
            1
        );
        let backend = Backend::resolve("auto").unwrap();
        assert_eq!(
            workers(backend, Stores::Streaming, "all", &[7], 128).unwrap(),
            1
        );
        assert!(workers(backend, Stores::Streaming, "all", &[7, 9], 128).is_err());
        let mut calls = Vec::new();
        assert_eq!(
            select("read", 4, |_, count| {
                calls.push(count);
                if count == 2 {
                    Err("later pilot failed".into())
                } else {
                    Ok(100.0)
                }
            }),
            Err("later pilot failed".into())
        );
        assert_eq!(calls, vec![1, 2]);
    }
}
