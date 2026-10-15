use std::{fs::File, io::Read, path::Path};

pub const METHOD: u32 = 3;
pub const DEFAULT_WINDOW: usize = 2 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub threads: usize,
    pub cache_threads: usize,
    pub stores: crate::portable::Stores,
    pub bandwidth_bytes: usize,
    pub latency_bytes: usize,
    pub stride: usize,
    pub window: usize,
    pub seed: u64,
    pub samples: usize,
    pub min_time: f64,
}

impl Spec {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=2048).contains(&self.threads) || !(1..=2048).contains(&self.cache_threads) {
            return Err("worker count must be between 1 and 2048".into());
        }
        let minimum = self
            .threads
            .checked_mul(128)
            .ok_or("buffer size overflow")?;
        if self.bandwidth_bytes < minimum || !self.bandwidth_bytes.is_multiple_of(128) {
            return Err("total bandwidth bytes must be a multiple of 128, with at least 128 bytes per worker".into());
        }
        if self.stride < 8
            || !self.stride.is_power_of_two()
            || self.latency_bytes == 0
            || !self.latency_bytes.is_multiple_of(self.stride)
        {
            return Err(
                "latency bytes must be a positive multiple of a power-of-two stride (at least 8)"
                    .into(),
            );
        }
        if self.window != 0
            && (self.window > self.latency_bytes || !self.window.is_multiple_of(self.stride))
        {
            return Err(
                "shuffle window must be 0 or a stride multiple no larger than the latency buffer"
                    .into(),
            );
        }
        if !(1..=100).contains(&self.samples)
            || !self.min_time.is_finite()
            || !(0.001..=10.0).contains(&self.min_time)
        {
            return Err("use 1..100 samples and 0.001..10 seconds per sample".into());
        }
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let keys = [
            "method",
            "threads",
            "bandwidth_bytes",
            "latency_bytes",
            "stride",
            "window",
            "seed",
            "samples",
            "min_time",
            "cache_threads",
            "stores",
        ];
        let mut values = [None; 11];
        for line in text.lines() {
            let line = line.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or("profile lines need key=value")?;
            let index = keys
                .iter()
                .position(|name| *name == key.trim())
                .ok_or_else(|| format!("unknown profile key '{}'", key.trim()))?;
            if values[index].replace(value.trim()).is_some() {
                return Err(format!("duplicate profile key '{}'", keys[index]));
            }
        }
        let value = |index: usize| {
            values[index].ok_or_else(|| format!("missing profile key '{}'", keys[index]))
        };
        let integer = |index| -> Result<usize, String> {
            value(index)?
                .parse()
                .map_err(|_| format!("invalid profile integer '{}'", keys[index]))
        };
        if integer(0)? != METHOD as usize {
            return Err(format!(
                "unsupported benchmark method; this build supports method {METHOD}"
            ));
        }
        let spec = Self {
            threads: integer(1)?,
            bandwidth_bytes: integer(2)?,
            latency_bytes: integer(3)?,
            stride: integer(4)?,
            window: integer(5)?,
            seed: value(6)?.parse().map_err(|_| "invalid profile seed")?,
            samples: integer(7)?,
            min_time: value(8)?.parse().map_err(|_| "invalid profile min_time")?,
            cache_threads: integer(9)?,
            stores: crate::portable::Stores::parse(value(10)?)?,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let mut text = String::new();
        File::open(path)
            .and_then(|f| f.take(16385).read_to_string(&mut text))
            .map_err(|e| format!("cannot read profile '{}': {e}", path.display()))?;
        if text.len() > 16384 {
            return Err("profile exceeds 16 KiB".into());
        }
        Self::parse(&text)
    }

    pub fn text(&self) -> String {
        format!(
            "# memtest fixed workload; CPU placement is chosen on each host.\nmethod={METHOD}\nthreads={}\nbandwidth_bytes={}\nlatency_bytes={}\nstride={}\nwindow={}\nseed={}\nsamples={}\nmin_time={}\ncache_threads={}\nstores={}\n",
            self.threads,
            self.bandwidth_bytes,
            self.latency_bytes,
            self.stride,
            self.window,
            self.seed,
            self.samples,
            self.min_time,
            self.cache_threads,
            self.stores.key()
        )
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        std::fs::write(path, self.text())
            .map_err(|e| format!("cannot save profile '{}': {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> Spec {
        Spec {
            threads: 2,
            cache_threads: 4,
            stores: crate::portable::Stores::Streaming,
            bandwidth_bytes: 64 << 20,
            latency_bytes: 16 << 20,
            stride: 128,
            window: 0,
            seed: 42,
            samples: 3,
            min_time: 0.01,
        }
    }
    #[test]
    fn profile_roundtrip_preserves_all_settings() {
        let spec = example();
        assert_eq!(Spec::parse(&spec.text()).unwrap(), spec);
        assert_eq!(
            Spec::parse(&format!("\u{feff}{}", spec.text())).unwrap(),
            spec
        );
    }
    #[test]
    fn invalid_and_future_profiles_fail_instead_of_changing_work() {
        let text = example().text();
        for bad in [
            text.replace("method=3", "method=999"),
            text.replace("method=3", "method=2"),
            text.replace("cache_threads=4", "cache_threads=0"),
            text.replace("stores=streaming", "stores=unknown"),
            text.replace("threads=2", "threads=0"),
            text.replace("min_time=0.01", "min_time=NaN"),
            text.replace("seed=42", "seed=-1"),
            format!("{text}\nseed=2"),
            format!("{text}\nunknown=1"),
            text.replace("window=0", "window=129"),
            text.replace("stride=128", "stride=63"),
        ] {
            assert!(Spec::parse(&bad).is_err(), "{bad}");
        }
    }
}
