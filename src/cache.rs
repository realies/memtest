//! Cache workloads sized against reported data/unified cache-sharing domains.
use crate::platform::{Cache, Hardware};

#[derive(Clone, Debug)]
pub struct Plan {
    pub level: u8,
    pub capacity_bytes: usize,
    pub working_set_bytes: usize,
    pub stride: usize,
    pub cpu: usize,
    /// Bandwidth workers; latency still uses the representative `cpu` above.
    pub cpus: Vec<usize>,
    /// Total private Read/Write storage. Copy divides this equally between streams.
    pub bandwidth_bytes: usize,
}

pub struct Result {
    pub plan: Plan,
    pub results: crate::json::Results,
}

fn applies(cache: &Cache, cpu: usize) -> bool {
    cache.cpus.is_empty() || cache.cpus.contains(&cpu)
}

fn valid(cache: &Cache) -> bool {
    cache.bytes != 0 && cache.line_bytes >= 8 && cache.line_bytes.is_power_of_two()
}

fn lower_capacity(hardware: &Hardware, cpu: usize, level: u8) -> Option<usize> {
    let mut largest = 0;
    for lower in 1..level {
        // Unknown mapping must accommodate the largest possible lower cache,
        // rather than assume that scheduling selected the smaller core type.
        let bytes = hardware
            .caches
            .iter()
            .filter(|cache| cache.level == lower && cache.bytes != 0 && applies(cache, cpu))
            .map(|cache| cache.bytes)
            .max()?;
        largest = largest.max(bytes);
    }
    Some(largest)
}

fn latency_geometry(hardware: &Hardware, cpu: usize, level: u8) -> Option<(usize, usize, usize)> {
    let cache = hardware
        .caches
        .iter()
        .filter(|cache| cache.level == level && applies(cache, cpu) && valid(cache))
        .min_by_key(|cache| cache.bytes)?;
    let alignment = 256.max(cache.line_bytes);
    let bytes = cache.bytes / 2 / alignment * alignment;
    (bytes > lower_capacity(hardware, cpu, level)?
        && bytes >= alignment
        && (hardware.memory_budget == 0 || bytes <= hardware.memory_budget))
        .then_some((cache.bytes, bytes, cache.line_bytes))
}

fn private_bytes(hardware: &Hardware, cpus: &[usize], level: u8) -> Option<usize> {
    let domains: Vec<_> = hardware
        .caches
        .iter()
        .filter(|cache| {
            cache.level == level && valid(cache) && cpus.iter().any(|&cpu| applies(cache, cpu))
        })
        .collect();
    let alignment = domains.iter().map(|cache| cache.line_bytes).max()?.max(256);
    let mut bytes = usize::MAX;
    for domain in domains {
        let workers = cpus.iter().filter(|&&cpu| applies(domain, cpu)).count();
        // Empty domain mappings share one global budget; never multiply it by CPUs.
        bytes = bytes.min(domain.bytes / 2 / workers);
    }
    if hardware.memory_budget != 0 {
        bytes = bytes.min(hardware.memory_budget / cpus.len());
    }
    let bytes = bytes / alignment * alignment;
    if bytes < alignment {
        return None;
    }
    for &cpu in cpus {
        if bytes <= lower_capacity(hardware, cpu, level)? {
            return None;
        }
    }
    Some(bytes)
}

pub fn plans(
    hardware: &Hardware,
    cpus: &[usize],
    selected: &str,
) -> std::result::Result<Vec<Plan>, String> {
    if selected == "memory" {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    let mut unique = std::collections::BTreeSet::new();
    let cpus: Vec<_> = cpus
        .iter()
        .copied()
        .filter(|cpu| hardware.cpus.contains(cpu) && unique.insert(*cpu))
        .collect();
    for level in 1..=3 {
        if selected != "all" && selected != format!("l{level}") {
            continue;
        }
        let Some((cpu, (capacity_bytes, working_set_bytes, stride))) =
            cpus.iter().find_map(|&cpu| {
                latency_geometry(hardware, cpu, level).map(|geometry| (cpu, geometry))
            })
        else {
            continue;
        };
        let mut workers = Vec::new();
        let mut per_worker = 0;
        for &candidate in &cpus {
            if !hardware
                .caches
                .iter()
                .any(|cache| cache.level == level && applies(cache, candidate) && valid(cache))
            {
                continue;
            }
            workers.push(candidate);
            if let Some(bytes) = private_bytes(hardware, &workers, level) {
                per_worker = bytes;
            } else {
                // Keep caller order; a worker that prevents isolation is omitted.
                workers.pop();
            }
        }
        let Some(bandwidth_bytes) = per_worker.checked_mul(workers.len()) else {
            continue;
        };
        if workers.is_empty() {
            continue;
        }
        plans.push(Plan {
            level,
            capacity_bytes,
            working_set_bytes,
            stride,
            cpu,
            cpus: workers,
            bandwidth_bytes,
        });
    }
    if selected != "all" && plans.is_empty() {
        return Err(format!(
            "{selected} cache geometry is unavailable or cannot support an isolated working set on the selected CPUs"
        ));
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Cache;
    fn hardware() -> Hardware {
        Hardware {
            cpus: vec![0, 1],
            preferred: vec![0, 1],
            cache_bytes: 8 << 20,
            cache_total_bytes: 8 << 20,
            cache_line_bytes: 64,
            memory_budget: 1 << 30,
            affinity: "hard",
            page_size: 4096,
            caches: vec![
                Cache {
                    level: 1,
                    bytes: 32 << 10,
                    line_bytes: 64,
                    cpus: vec![0],
                },
                Cache {
                    level: 1,
                    bytes: 64 << 10,
                    line_bytes: 64,
                    cpus: vec![1],
                },
                Cache {
                    level: 2,
                    bytes: 512 << 10,
                    line_bytes: 64,
                    cpus: vec![0],
                },
                Cache {
                    level: 3,
                    bytes: 8 << 20,
                    line_bytes: 64,
                    cpus: vec![0, 1],
                },
            ],
        }
    }
    #[test]
    fn footprints_fit_target_and_exceed_lower_cache() {
        let h = hardware();
        let p = plans(&h, &[0], "all").unwrap();
        assert_eq!(
            p.iter().map(|p| p.working_set_bytes).collect::<Vec<_>>(),
            vec![16 << 10, 256 << 10, 4 << 20]
        );
        assert!(p.iter().all(|p| p.stride == 64 && p.cpu == 0));
        assert_eq!(
            plans(&h, &[1], "l1").unwrap()[0].working_set_bytes,
            32 << 10
        );
        assert!(plans(&h, &[1], "l2").is_err());
        assert!(plans(&h, &[0], "memory").unwrap().is_empty());
        assert!(
            p.windows(2)
                .all(|p| p[1].working_set_bytes > p[0].capacity_bytes)
        );
    }
    #[test]
    fn unknown_or_overlapping_geometry_is_not_invented() {
        let mut h = hardware();
        h.caches[2].bytes = 64 << 10;
        assert!(plans(&h, &[0], "l2").is_err());
        h.caches[0].line_bytes = 0;
        assert!(plans(&h, &[0], "l1").is_err());
        h.caches.clear();
        assert!(plans(&h, &[0], "all").unwrap().is_empty());
    }
    #[test]
    fn unpinned_geometry_uses_the_smallest_reported_capacity() {
        let mut h = hardware();
        for c in &mut h.caches {
            c.cpus.clear();
        }
        assert_eq!(plans(&h, &[1], "l1").unwrap()[0].capacity_bytes, 32 << 10);
    }

    fn cache(level: u8, bytes: usize, cpus: Vec<usize>) -> Cache {
        Cache {
            level,
            bytes,
            line_bytes: 64,
            cpus,
        }
    }

    #[test]
    fn heterogeneous_private_caches_use_uniform_private_buffers() {
        let mut h = hardware();
        h.caches.push(cache(2, 1 << 20, vec![1]));
        let p = plans(&h, &[0, 1], "all").unwrap();
        assert!(p.iter().all(|p| p.cpus == [0, 1]));
        assert_eq!(
            p.iter().map(|p| p.bandwidth_bytes).collect::<Vec<_>>(),
            vec![32 << 10, 512 << 10, 4 << 20]
        );
        assert_eq!(p[2].working_set_bytes, 4 << 20); // single-worker latency
        assert_eq!(p[2].bandwidth_bytes / p[2].cpus.len(), 2 << 20);
        let reverse = plans(&h, &[1, 0], "l1").unwrap();
        assert_eq!(reverse[0].cpu, 1);
        assert_eq!(reverse[0].cpus, [1, 0]);
        assert_eq!(reverse[0].working_set_bytes, 32 << 10);
        assert_eq!(reverse[0].bandwidth_bytes, 32 << 10);
    }

    #[test]
    fn shared_cache_budget_is_not_multiplied_by_worker_count() {
        let mut h = hardware();
        h.caches[2].cpus = vec![0, 1];
        let p = plans(&h, &[0, 1], "l2").unwrap();
        assert_eq!(p[0].cpus, [0, 1]);
        assert_eq!(p[0].bandwidth_bytes, 256 << 10);
        assert_eq!(p[0].bandwidth_bytes / 2, 128 << 10);
        assert!(p[0].bandwidth_bytes / 2 > 64 << 10);
    }

    #[test]
    fn scarce_llc_reduces_workers_to_exceed_lower_cache_capacity() {
        let mut h = hardware();
        h.caches.push(cache(2, 512 << 10, vec![1]));
        h.caches[3].bytes = 1536 << 10;
        let p = plans(&h, &[0, 1], "l3").unwrap();
        assert_eq!(p[0].cpus, [0]);
        assert_eq!(p[0].bandwidth_bytes, 768 << 10);
        h.caches[3].bytes = 1 << 20;
        assert!(plans(&h, &[0, 1], "l3").is_err());
    }

    #[test]
    fn distinct_shared_domains_each_keep_their_own_half_capacity_limit() {
        let mut h = hardware();
        h.cpus = vec![0, 1, 2, 3];
        h.preferred = h.cpus.clone();
        h.caches.clear();
        for cpu in 0..4 {
            h.caches.push(cache(1, 32 << 10, vec![cpu]));
            h.caches.push(cache(2, 512 << 10, vec![cpu]));
        }
        h.caches.push(cache(3, 4 << 20, vec![0, 1]));
        h.caches.push(cache(3, 8 << 20, vec![2, 3]));
        let p = plans(&h, &[0, 1, 2, 3], "l3").unwrap();
        assert_eq!(p[0].cpus, [0, 1, 2, 3]);
        assert_eq!(p[0].bandwidth_bytes, 4 << 20);
        let per_worker = p[0].bandwidth_bytes / p[0].cpus.len();
        for domain in h.caches.iter().filter(|c| c.level == 3) {
            let workers = p[0]
                .cpus
                .iter()
                .filter(|cpu| domain.cpus.contains(cpu))
                .count();
            assert!(per_worker * workers <= domain.bytes / 2);
        }
    }

    #[test]
    fn unknown_mapping_uses_one_budget_and_missing_lower_geometry_is_omitted() {
        let mut h = hardware();
        for c in &mut h.caches {
            c.cpus.clear();
        }
        let p = plans(&h, &[0, 1], "all").unwrap();
        assert_eq!(
            p.iter().map(|p| p.bandwidth_bytes).collect::<Vec<_>>(),
            vec![16 << 10, 256 << 10, 4 << 20]
        );
        assert!(p.iter().all(|p| p.cpus == [0, 1]));
        h.caches.retain(|c| c.level != 1);
        assert!(plans(&h, &[0, 1], "l2").is_err());
        assert!(plans(&h, &[0, 1], "l3").is_err());
    }

    #[test]
    fn workers_are_available_unique_and_allocation_budget_is_respected() {
        let mut h = hardware();
        let p = plans(&h, &[0, 0, 999, 1], "l1").unwrap();
        assert_eq!(p[0].cpus, [0, 1]);
        h.memory_budget = 16 << 10;
        let p = plans(&h, &[0, 1], "l1").unwrap();
        assert_eq!(p[0].cpus, [0, 1]);
        assert!(p[0].bandwidth_bytes <= h.memory_budget);
        assert!(plans(&h, &[], "all").unwrap().is_empty());
        assert!(plans(&h, &[], "l1").is_err());
    }
}
