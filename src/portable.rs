//! Streaming bandwidth with an explicit store policy and a deterministic dependent-load cycle.
use crate::platform;
use std::{
    alloc::{Layout, alloc, dealloc},
    hint::black_box,
    slice,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Instant,
};

#[derive(Clone, Copy, Debug)]
pub enum BandwidthKind {
    Read,
    Write,
    Copy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stores {
    Cached,
    Streaming,
}
impl Stores {
    pub fn parse(key: &str) -> Result<Self, String> {
        match key {
            "cached" => Ok(Self::Cached),
            "streaming" => Ok(Self::Streaming),
            _ => Err("stores must be cached or streaming".into()),
        }
    }
    pub const fn key(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Streaming => "streaming",
        }
    }
    /// ARM STNP supplies a locality hint; unlike x86 NT stores it need not bypass caches.
    pub const fn name(self, backend: Backend) -> &'static str {
        match (self, backend) {
            (Self::Cached, _) => "cached",
            (Self::Streaming, Backend::Neon) => "streaming hint (STNP)",
            (Self::Streaming, _) => "streaming (NT)",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Measurement {
    /// Decimal useful MB/s for bandwidth, nanoseconds per dependent load for latency.
    pub value: f64,
    /// Full observed range: 100 * (maximum - minimum) / median.
    pub spread_percent: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sse2,
    Avx2,
    Neon,
}
impl Backend {
    /// Only backends executable on this CPU and operating system are offered.
    pub fn available() -> &'static [Self] {
        #[cfg(target_arch = "x86_64")]
        {
            if std::is_x86_feature_detected!("avx2") {
                &[Self::Sse2, Self::Avx2]
            } else if std::is_x86_feature_detected!("sse2") {
                &[Self::Sse2]
            } else {
                &[]
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            if std::arch::is_aarch64_feature_detected!("neon") {
                &[Self::Neon]
            } else {
                &[]
            }
        }
    }
    pub fn resolve(key: &str) -> Result<Self, String> {
        let backend = match key {
            "auto" => Self::available()
                .last()
                .copied()
                .ok_or("no supported memory bandwidth backend is available")?,
            "sse2" => Self::Sse2,
            "avx2" => Self::Avx2,
            "neon" => Self::Neon,
            _ => return Err("ISA must be auto, sse2, avx2, or neon".into()),
        };
        backend.validate()?;
        Ok(backend)
    }
    pub const fn key(self) -> &'static str {
        match self {
            Self::Sse2 => "sse2",
            Self::Avx2 => "avx2",
            Self::Neon => "neon",
        }
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sse2 => "SSE2",
            Self::Avx2 => "AVX2",
            Self::Neon => "NEON",
        }
    }
    pub(crate) fn validate(self) -> Result<(), String> {
        if Self::available().contains(&self) {
            Ok(())
        } else {
            Err(format!(
                "ISA '{}' is unavailable on this CPU/OS",
                self.key()
            ))
        }
    }
}

struct Buffer {
    allocation: *mut u8,
    pointer: *mut u8,
    bytes: usize,
    layout: Layout,
}
impl Buffer {
    fn new(bytes: usize) -> Result<Self, String> {
        let size = bytes.checked_add(256).ok_or("buffer size overflow")?;
        let layout = Layout::from_size_align(size, 128).map_err(|_| "buffer too large")?;
        let allocation = unsafe { alloc(layout) };
        if allocation.is_null() {
            return Err("buffer allocation failed".into());
        }
        let pointer = unsafe { allocation.add(128) };
        unsafe {
            // Initialize before making typed slices; the owner first-touches every page.
            allocation.write_bytes(0xa5, size);
        }
        Ok(Self {
            allocation,
            pointer,
            bytes,
            layout,
        })
    }
    fn words(&mut self) -> &mut [u64] {
        unsafe { slice::from_raw_parts_mut(self.pointer.cast(), self.bytes / 8) }
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { dealloc(self.allocation, self.layout) }
    }
}

// SplitMix64, followed by rejection sampling, defines the permutation across platforms.
struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, bound: usize) -> usize {
        let bound = bound as u64;
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let n = self.next();
            if n >= threshold {
                return (n % bound) as usize;
            }
        }
    }
}
fn fill(words: &mut [u64]) {
    let mut random = Random(0x6a09e667f3bcc909);
    for word in words {
        *word = random.next();
    }
}

fn pointer_cycle(data: &mut Buffer, stride: usize, window: usize, seed: u64) -> Result<(), String> {
    let nodes = data.bytes / stride;
    let width = if window == 0 {
        nodes
    } else {
        (window / stride).min(nodes)
    };
    let mut order = Vec::new();
    order
        .try_reserve_exact(width)
        .map_err(|_| "permutation allocation failed")?;
    order.resize(width, 0usize);
    let windows = nodes.div_ceil(width);
    let mut window_order = Vec::new();
    window_order
        .try_reserve_exact(windows)
        .map_err(|_| "window permutation allocation failed")?;
    window_order.extend(0..windows);
    let mut random = Random(seed);
    for i in (1..windows).rev() {
        let j = random.below(i + 1);
        window_order.swap(i, j);
    }
    let (mut first, mut previous) = (None, None);
    for window_index in window_order {
        let start = window_index * width;
        let count = width.min(nodes - start);
        for (i, node) in order[..count].iter_mut().enumerate() {
            *node = start + i;
        }
        for i in (1..count).rev() {
            let j = random.below(i + 1);
            order.swap(i, j);
        }
        for &node in &order[..count] {
            if let Some(prior) = previous {
                unsafe {
                    data.pointer
                        .add(prior * stride)
                        .cast::<*const u8>()
                        .write(data.pointer.add(node * stride));
                }
            } else {
                first = Some(node);
            }
            previous = Some(node);
        }
    }
    unsafe {
        data.pointer
            .add(previous.unwrap() * stride)
            .cast::<*const u8>()
            .write(data.pointer.add(first.unwrap() * stride));
    }
    Ok(())
}

// Streaming batches require live, initialized, 128-byte-aligned buffers,
// positive bytes divisible by 128 and positive passes. Writes require exclusive
// access. Copy additionally requires a separate, nonoverlapping destination.
#[cfg(target_arch = "x86_64")]
unsafe fn sse2(
    stores: Stores,
    kind: BandwidthKind,
    source: *mut u8,
    destination: *mut u8,
    bytes: usize,
    passes: usize,
) {
    use std::arch::asm;
    match kind {
        BandwidthKind::Read => unsafe {
            asm!(
            "3:", "mov {p}, {base}", "mov {n}, {size}", "2:",
            "movdqa xmm0, [{p}]", "movdqa xmm1, [{p} + 16]",
            "movdqa xmm2, [{p} + 32]", "movdqa xmm3, [{p} + 48]",
            "movdqa xmm4, [{p} + 64]", "movdqa xmm5, [{p} + 80]",
            "movdqa xmm6, [{p} + 96]", "movdqa xmm7, [{p} + 112]",
            "add {p}, 128", "sub {n}, 128", "jnz 2b", "dec {passes}", "jnz 3b", "mfence",
            base = in(reg) source, size = in(reg) bytes, passes = inout(reg) passes => _,
            p = out(reg) _, n = out(reg) _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _, options(nostack));
        },
        BandwidthKind::Write => unsafe {
            macro_rules! store_loop {
                ($op:literal) => {{ asm!(
            "pxor xmm0, xmm0", "3:", "mov {p}, {base}", "mov {n}, {size}", "2:",
            concat!($op, " [{p}], xmm0"), concat!($op, " [{p} + 16], xmm0"),
            concat!($op, " [{p} + 32], xmm0"), concat!($op, " [{p} + 48], xmm0"),
            concat!($op, " [{p} + 64], xmm0"), concat!($op, " [{p} + 80], xmm0"),
            concat!($op, " [{p} + 96], xmm0"), concat!($op, " [{p} + 112], xmm0"),
            "add {p}, 128", "sub {n}, 128", "jnz 2b", "dec {passes}", "jnz 3b", "mfence",
            base = in(reg) source, size = in(reg) bytes, passes = inout(reg) passes => _,
            p = out(reg) _, n = out(reg) _, out("xmm0") _, options(nostack)); }};
            }
            match stores {
                Stores::Cached => store_loop!("movdqa"),
                Stores::Streaming => store_loop!("movntdq"),
            }
        },
        BandwidthKind::Copy => unsafe {
            macro_rules! store_loop {
                ($op:literal) => {{ asm!(
            "3:", "mov {p}, {base}", "mov {d}, {target}", "mov {n}, {size}", "2:",
            "movdqa xmm0, [{p}]", "movdqa xmm1, [{p} + 16]",
            "movdqa xmm2, [{p} + 32]", "movdqa xmm3, [{p} + 48]",
            "movdqa xmm4, [{p} + 64]", "movdqa xmm5, [{p} + 80]",
            "movdqa xmm6, [{p} + 96]", "movdqa xmm7, [{p} + 112]",
            concat!($op, " [{d}], xmm0"), concat!($op, " [{d} + 16], xmm1"),
            concat!($op, " [{d} + 32], xmm2"), concat!($op, " [{d} + 48], xmm3"),
            concat!($op, " [{d} + 64], xmm4"), concat!($op, " [{d} + 80], xmm5"),
            concat!($op, " [{d} + 96], xmm6"), concat!($op, " [{d} + 112], xmm7"),
            "add {p}, 128", "add {d}, 128", "sub {n}, 128", "jnz 2b",
            "dec {passes}", "jnz 3b", "mfence",
            base = in(reg) source, target = in(reg) destination, size = in(reg) bytes,
            passes = inout(reg) passes => _, p = out(reg) _, d = out(reg) _, n = out(reg) _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _, options(nostack)); }};
            }
            match stores {
                Stores::Cached => store_loop!("movdqa"),
                Stores::Streaming => store_loop!("movntdq"),
            }
        },
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2(
    stores: Stores,
    kind: BandwidthKind,
    source: *mut u8,
    destination: *mut u8,
    bytes: usize,
    passes: usize,
) {
    use std::arch::asm;
    match kind {
        BandwidthKind::Read => unsafe {
            asm!(
            "3:", "mov {p}, {base}", "mov {n}, {size}", "2:",
            "vmovdqa ymm0, [{p}]", "vmovdqa ymm1, [{p} + 32]",
            "vmovdqa ymm2, [{p} + 64]", "vmovdqa ymm3, [{p} + 96]",
            "add {p}, 128", "sub {n}, 128", "jnz 2b", "dec {passes}", "jnz 3b", "mfence",
            base = in(reg) source, size = in(reg) bytes, passes = inout(reg) passes => _,
            p = out(reg) _, n = out(reg) _,
            out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, options(nostack));
        },
        BandwidthKind::Write => unsafe {
            macro_rules! store_loop {
                ($op:literal) => {{ asm!(
            "vpxor ymm0, ymm0, ymm0", "3:", "mov {p}, {base}", "mov {n}, {size}", "2:",
            concat!($op, " [{p}], ymm0"), concat!($op, " [{p} + 32], ymm0"),
            concat!($op, " [{p} + 64], ymm0"), concat!($op, " [{p} + 96], ymm0"),
            "add {p}, 128", "sub {n}, 128", "jnz 2b", "dec {passes}", "jnz 3b", "mfence",
            base = in(reg) source, size = in(reg) bytes, passes = inout(reg) passes => _,
            p = out(reg) _, n = out(reg) _, out("ymm0") _, options(nostack)); }};
            }
            match stores {
                Stores::Cached => store_loop!("vmovdqa"),
                Stores::Streaming => store_loop!("vmovntdq"),
            }
        },
        BandwidthKind::Copy => unsafe {
            macro_rules! store_loop {
                ($op:literal) => {{ asm!(
            "3:", "mov {p}, {base}", "mov {d}, {target}", "mov {n}, {size}", "2:",
            "vmovdqa ymm0, [{p}]", "vmovdqa ymm1, [{p} + 32]",
            "vmovdqa ymm2, [{p} + 64]", "vmovdqa ymm3, [{p} + 96]",
            concat!($op, " [{d}], ymm0"), concat!($op, " [{d} + 32], ymm1"),
            concat!($op, " [{d} + 64], ymm2"), concat!($op, " [{d} + 96], ymm3"),
            "add {p}, 128", "add {d}, 128", "sub {n}, 128", "jnz 2b",
            "dec {passes}", "jnz 3b", "mfence",
            base = in(reg) source, target = in(reg) destination, size = in(reg) bytes,
            passes = inout(reg) passes => _, p = out(reg) _, d = out(reg) _, n = out(reg) _,
            out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, options(nostack)); }};
            }
            match stores {
                Stores::Cached => store_loop!("vmovdqa"),
                Stores::Streaming => store_loop!("vmovntdq"),
            }
        },
    }
    // One batch boundary; the intrinsic accounts for upper-vector clobbers.
    std::arch::x86_64::_mm256_zeroupper();
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon(
    stores: Stores,
    kind: BandwidthKind,
    source: *mut u8,
    destination: *mut u8,
    bytes: usize,
    passes: usize,
) {
    use std::arch::asm;
    match kind {
        BandwidthKind::Read => unsafe {
            asm!(
            "3:", "mov {p}, {base}", "mov {n}, {size}", "2:",
            "ldp q0, q1, [{p}]", "ldp q2, q3, [{p}, #32]",
            "ldp q4, q5, [{p}, #64]", "ldp q6, q7, [{p}, #96]",
            "add {p}, {p}, #128", "subs {n}, {n}, #128", "b.ne 2b",
            "subs {passes}, {passes}, #1", "b.ne 3b", "dsb ish",
            base = in(reg) source, size = in(reg) bytes, passes = inout(reg) passes => _,
            p = out(reg) _, n = out(reg) _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _,
            out("v4") _, out("v5") _, out("v6") _, out("v7") _, options(nostack));
        },
        BandwidthKind::Write => unsafe {
            macro_rules! store_loop {
                ($op:literal) => {{ asm!(
            "movi v0.16b, #0", "3:", "mov {p}, {base}", "mov {n}, {size}", "2:",
            concat!($op, " q0, q0, [{p}]"), concat!($op, " q0, q0, [{p}, #32]"),
            concat!($op, " q0, q0, [{p}, #64]"), concat!($op, " q0, q0, [{p}, #96]"),
            "add {p}, {p}, #128", "subs {n}, {n}, #128", "b.ne 2b",
            "subs {passes}, {passes}, #1", "b.ne 3b", "dsb ish",
            base = in(reg) source, size = in(reg) bytes, passes = inout(reg) passes => _,
            p = out(reg) _, n = out(reg) _, out("v0") _, options(nostack)); }};
            }
            match stores {
                Stores::Cached => store_loop!("stp"),
                Stores::Streaming => store_loop!("stnp"),
            }
        },
        BandwidthKind::Copy => unsafe {
            macro_rules! store_loop {
                ($op:literal) => {{ asm!(
            "3:", "mov {p}, {base}", "mov {d}, {target}", "mov {n}, {size}", "2:",
            "ldp q0, q1, [{p}]", "ldp q2, q3, [{p}, #32]",
            "ldp q4, q5, [{p}, #64]", "ldp q6, q7, [{p}, #96]",
            concat!($op, " q0, q1, [{d}]"), concat!($op, " q2, q3, [{d}, #32]"),
            concat!($op, " q4, q5, [{d}, #64]"), concat!($op, " q6, q7, [{d}, #96]"),
            "add {p}, {p}, #128", "add {d}, {d}, #128", "subs {n}, {n}, #128", "b.ne 2b",
            "subs {passes}, {passes}, #1", "b.ne 3b", "dsb ish",
            base = in(reg) source, target = in(reg) destination, size = in(reg) bytes,
            passes = inout(reg) passes => _, p = out(reg) _, d = out(reg) _, n = out(reg) _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _,
            out("v4") _, out("v5") _, out("v6") _, out("v7") _, options(nostack)); }};
            }
            match stores {
                Stores::Cached => store_loop!("stp"),
                Stores::Streaming => store_loop!("stnp"),
            }
        },
    }
}

// The caller satisfies the streaming-batch contract and validates the backend.
unsafe fn kernel(
    backend: Backend,
    stores: Stores,
    kind: BandwidthKind,
    source: *mut u8,
    destination: *mut u8,
    bytes: usize,
    passes: usize,
) {
    match backend {
        #[cfg(target_arch = "x86_64")]
        Backend::Sse2 => unsafe { sse2(stores, kind, source, destination, bytes, passes) },
        #[cfg(target_arch = "x86_64")]
        Backend::Avx2 => unsafe { avx2(stores, kind, source, destination, bytes, passes) },
        #[cfg(target_arch = "aarch64")]
        Backend::Neon => unsafe { neon(stores, kind, source, destination, bytes, passes) },
        _ => unreachable!("bandwidth validates backend availability before starting workers"),
    }
}
fn validate_sampling(samples: usize, minimum: f64) -> Result<(), String> {
    if samples == 0 || samples > 100 || !minimum.is_finite() || !(0.001..=10.0).contains(&minimum) {
        return Err("samples must be 1..100 and minimum sample seconds 0.001..10".into());
    }
    Ok(())
}
fn summarize(mut values: Vec<f64>) -> Measurement {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    };
    Measurement {
        value: median,
        spread_percent: 100.0 * (values[values.len() - 1] - values[0]) / median,
    }
}
fn calibrate(
    mut run: impl FnMut(usize) -> Result<f64, String>,
    minimum: f64,
) -> Result<usize, String> {
    let mut passes = 1;
    loop {
        if run(passes)? >= minimum {
            return Ok(passes);
        }
        passes = passes
            .checked_mul(2)
            .ok_or("measurement pass count overflow")?;
    }
}

fn worker_bytes(total: usize, workers: usize, index: usize) -> usize {
    let blocks = total / 128;
    (blocks / workers + usize::from(index < blocks % workers)) * 128
}
fn transferred_bytes(kind: BandwidthKind, bytes: usize, passes: usize) -> Result<usize, String> {
    let streams = if matches!(kind, BandwidthKind::Copy) {
        2
    } else {
        1
    };
    bytes
        .checked_mul(streams)
        .and_then(|bytes| bytes.checked_mul(passes))
        .ok_or_else(|| "measurement byte count overflow".into())
}

/// Split the exact total into 128-byte blocks, with worker sizes differing by at most
/// one block. Workers first-touch after pinning; one clock spans release and completion.
/// Copy uses two streams of this size and counts source reads + destination writes.
/// Useful bytes exclude physical bus traffic such as write allocation.
/// Store policy affects writes and copy destinations; reads remain cached.
#[allow(clippy::too_many_arguments)]
pub fn bandwidth(
    backend: Backend,
    stores: Stores,
    kind: BandwidthKind,
    cpus: &[usize],
    total_bytes: usize,
    samples: usize,
    min_seconds: f64,
    mut on_sample: impl FnMut(usize, f64),
) -> Result<Measurement, String> {
    backend.validate()?;
    validate_sampling(samples, min_seconds)?;
    if cpus.is_empty()
        || total_bytes == 0
        || !total_bytes.is_multiple_of(128)
        || total_bytes / 128 < cpus.len()
    {
        return Err(
            "bandwidth needs a total size divisible by 128 and at least 128 bytes per CPU".into(),
        );
    }
    for (i, cpu) in cpus.iter().enumerate() {
        if cpus[..i].contains(cpu) {
            return Err("CPU list contains duplicates".into());
        }
    }
    transferred_bytes(kind, total_bytes, 1)?;
    worker_bytes(total_bytes, cpus.len(), 0)
        .checked_add(256)
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or("buffer too large")?;
    let passes = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (mut starts, mut handles) = (Vec::new(), Vec::new());
        for (index, &cpu) in cpus.iter().enumerate() {
            let bytes_per_worker = worker_bytes(total_bytes, cpus.len(), index);
            let (start_tx, start_rx) = mpsc::channel::<Arc<Barrier>>();
            let ready = ready_tx.clone();
            let passes = &passes;
            handles.push(
                std::thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        let setup = (|| -> Result<_, String> {
                            let pin = platform::pin(cpu)?;
                            let mut data = Buffer::new(bytes_per_worker)?;
                            fill(data.words());
                            let destination = if matches!(kind, BandwidthKind::Copy) {
                                Some(Buffer::new(bytes_per_worker)?)
                            } else {
                                None
                            };
                            Ok((pin, data, destination))
                        })();
                        let (_pin, data, destination) = match setup {
                            Ok(value) => {
                                let _ = ready.send(Ok(()));
                                value
                            }
                            Err(error) => {
                                let _ = ready.send(Err(error.clone()));
                                return Err(error);
                            }
                        };
                        // Release the status channel before waiting on the startup gate.
                        drop(ready);
                        let gate = start_rx.recv().map_err(|_| "worker startup cancelled")?;
                        loop {
                            gate.wait();
                            let count = passes.load(Ordering::Relaxed);
                            if count == 0 {
                                break;
                            }
                            unsafe {
                                kernel(
                                    backend,
                                    stores,
                                    kind,
                                    data.pointer,
                                    destination
                                        .as_ref()
                                        .map_or(std::ptr::null_mut(), |d| d.pointer),
                                    data.bytes,
                                    count,
                                );
                            }
                            gate.wait();
                        }
                        Ok::<(), String>(())
                    })
                    .map_err(|e| e.to_string())?,
            );
            starts.push(start_tx);
        }
        drop(ready_tx);
        let mut failure = None;
        for _ in cpus {
            if let Err(error) = ready_rx
                .recv()
                .map_err(|_| "worker failed during startup")?
            {
                failure = Some(error);
            }
        }
        // Waiting workers have no barrier until all allocation and affinity checks succeed.
        // Dropping the startup channels on error lets every ready worker exit.
        if let Some(error) = failure {
            return Err(error);
        }
        let gate = Arc::new(Barrier::new(cpus.len() + 1));
        for start in starts {
            start
                .send(gate.clone())
                .map_err(|_| "worker failed before start")?;
        }
        let run = |count: usize| -> Result<f64, String> {
            transferred_bytes(kind, total_bytes, count)?;
            passes.store(count, Ordering::Relaxed);
            let began = Instant::now();
            gate.wait();
            gate.wait();
            Ok(began.elapsed().as_secs_f64())
        };
        let measured = (|| -> Result<Measurement, String> {
            run(1)?;
            let count = calibrate(run, min_seconds)?;
            let transferred = transferred_bytes(kind, total_bytes, count)? as f64;
            let mut values = Vec::with_capacity(samples);
            for index in 1..=samples {
                let value = transferred / run(count)? / 1e6;
                values.push(value);
                on_sample(index, value);
            }
            Ok(summarize(values))
        })();
        passes.store(0, Ordering::Relaxed);
        gate.wait();
        for handle in handles {
            handle.join().map_err(|_| "worker panicked")??;
        }
        measured
    })
}

// Every followed pointer must address an aligned, initialized pointer in live memory.
// The chain must remain unchanged for all `loads` dependent reads.
unsafe fn chase(mut pointer: *const u8, loads: usize) -> *const u8 {
    use std::arch::asm;
    let chunks = loads / 64;
    let tail = loads % 64;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if chunks != 0 {
            asm!("2:", ".rept 64", "mov {p}, [{p}]", ".endr", "dec {n}", "jnz 2b",
            p = inout(reg) pointer, n = inout(reg) chunks => _, options(nostack));
        }
        if tail != 0 {
            asm!("2:", "mov {p}, [{p}]", "dec {n}", "jnz 2b",
            p = inout(reg) pointer, n = inout(reg) tail => _, options(nostack));
        }
        asm!("lfence", options(nostack));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        if chunks != 0 {
            asm!("2:", ".rept 64", "ldr {p}, [{p}]", ".endr", "subs {n}, {n}, #1", "b.ne 2b",
            p = inout(reg) pointer, n = inout(reg) chunks => _, options(nostack));
        }
        if tail != 0 {
            asm!("2:", "ldr {p}, [{p}]", "subs {n}, {n}, #1", "b.ne 2b",
            p = inout(reg) pointer, n = inout(reg) tail => _, options(nostack));
        }
        asm!("dsb ish", options(nostack));
    }
    pointer
}

/// Windows and their nodes are shuffled and joined into one cycle, including partial
/// final windows. Window zero is a full-buffer shuffle.
/// Every timed batch traverses whole cycles, even for sizes not divisible by 64 nodes.
#[allow(clippy::too_many_arguments)]
pub fn latency(
    size: usize,
    stride: usize,
    window: usize,
    seed: u64,
    cpu: usize,
    samples: usize,
    min_seconds: f64,
    mut on_sample: impl FnMut(usize, f64),
) -> Result<Measurement, String> {
    validate_sampling(samples, min_seconds)?;
    if stride < 8
        || !stride.is_multiple_of(8)
        || size == 0
        || !size.is_multiple_of(stride)
        || (window != 0 && (window < stride || !window.is_multiple_of(stride)))
    {
        return Err("latency size and window must be multiples of a stride of at least 8 bytes; window 0 means the full buffer".into());
    }
    let _pin = platform::pin(cpu)?;
    let mut data = Buffer::new(size)?;
    fill(data.words());
    pointer_cycle(&mut data, stride, window, seed)?;
    let nodes = size / stride;
    let run = |count: usize| -> Result<f64, String> {
        let loads = nodes
            .checked_mul(count)
            .ok_or("measurement load count overflow")?;
        let began = Instant::now();
        black_box(unsafe { chase(data.pointer, loads) });
        Ok(began.elapsed().as_secs_f64())
    };
    run(1)?;
    let count = calibrate(run, min_seconds)?;
    let loads = nodes
        .checked_mul(count)
        .ok_or("measurement load count overflow")? as f64;
    let mut values = Vec::with_capacity(samples);
    for index in 1..=samples {
        let value = run(count)? * 1e9 / loads;
        values.push(value);
        on_sample(index, value);
    }
    Ok(summarize(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn guard_check(data: &Buffer) {
        unsafe {
            assert!(
                slice::from_raw_parts(data.allocation, 128)
                    .iter()
                    .all(|&b| b == 0xa5)
            );
            assert!(
                slice::from_raw_parts(data.pointer.add(data.bytes), 128)
                    .iter()
                    .all(|&b| b == 0xa5)
            );
        }
    }
    #[test]
    fn backend_selection_is_deterministic_and_checks_availability() {
        let available = Backend::available();
        assert!(!available.is_empty());
        assert_eq!(
            Backend::resolve("auto").unwrap(),
            *available.last().unwrap()
        );
        for backend in [Backend::Sse2, Backend::Avx2, Backend::Neon] {
            if available.contains(&backend) {
                assert_eq!(Backend::resolve(backend.key()).unwrap(), backend);
            } else {
                assert!(Backend::resolve(backend.key()).is_err());
                let error = bandwidth(
                    backend,
                    Stores::Cached,
                    BandwidthKind::Read,
                    &[],
                    128,
                    1,
                    0.001,
                    |_, _| panic!("an unavailable backend must not execute"),
                )
                .unwrap_err();
                assert!(error.contains("unavailable"));
            }
        }
        for key in ["", "Auto", "avx512"] {
            assert!(Backend::resolve(key).is_err());
        }
    }
    #[test]
    fn store_policy_is_explicit_and_arm_reports_a_hint() {
        for stores in [Stores::Cached, Stores::Streaming] {
            assert_eq!(Stores::parse(stores.key()).unwrap(), stores);
        }
        for key in ["", "auto", "Streaming", "nt"] {
            assert!(Stores::parse(key).is_err());
        }
        assert_eq!(Stores::Cached.name(Backend::Neon), "cached");
        assert_eq!(
            Stores::Streaming.name(Backend::Neon),
            "streaming hint (STNP)"
        );
        assert_eq!(Stores::Streaming.name(Backend::Sse2), "streaming (NT)");
    }
    #[test]
    fn both_store_policies_respect_bounds_and_data() {
        for &backend in Backend::available() {
            for stores in [Stores::Cached, Stores::Streaming] {
                for (bytes, passes) in [(128, 1), (128, 17), (384, 7), (4096, 3)] {
                    let mut data = Buffer::new(bytes).unwrap();
                    let mut destination = Buffer::new(bytes).unwrap();
                    fill(data.words());
                    let expected = data.words().to_vec();
                    unsafe {
                        kernel(
                            backend,
                            stores,
                            BandwidthKind::Read,
                            data.pointer,
                            std::ptr::null_mut(),
                            bytes,
                            passes,
                        );
                        assert_eq!(data.words(), expected);
                        guard_check(&data);
                        kernel(
                            backend,
                            stores,
                            BandwidthKind::Write,
                            data.pointer,
                            std::ptr::null_mut(),
                            bytes,
                            passes,
                        );
                        assert!(data.words().iter().all(|&word| word == 0));
                    }
                    guard_check(&data);
                    fill(data.words());
                    destination.words().fill(0x0123456789abcdef);
                    assert_ne!(destination.words(), expected);
                    unsafe {
                        kernel(
                            backend,
                            stores,
                            BandwidthKind::Copy,
                            data.pointer,
                            destination.pointer,
                            bytes,
                            passes,
                        );
                    }
                    assert_eq!(data.words(), expected);
                    assert_eq!(destination.words(), expected);
                    guard_check(&data);
                    guard_check(&destination);
                }
            }
        }
    }
    #[test]
    fn all_streams_validate_arguments_before_startup() {
        let backend = Backend::resolve("auto").unwrap();
        for kind in [
            BandwidthKind::Read,
            BandwidthKind::Write,
            BandwidthKind::Copy,
        ] {
            for (cpus, bytes) in [
                (&[][..], 128),
                (&[0][..], 0),
                (&[0][..], 127),
                (&[0][..], 129),
                (&[0, 1][..], 128),
                (&[0, 0][..], 256),
            ] {
                assert!(
                    bandwidth(
                        backend,
                        Stores::Cached,
                        kind,
                        cpus,
                        bytes,
                        1,
                        0.001,
                        |_, _| { panic!("invalid arguments must not reach measurement") }
                    )
                    .is_err()
                );
            }
            for (samples, minimum) in [(0, 0.001), (101, 0.001), (1, 0.0), (1, f64::NAN)] {
                assert!(
                    bandwidth(
                        backend,
                        Stores::Cached,
                        kind,
                        &[0],
                        128,
                        samples,
                        minimum,
                        |_, _| { panic!("invalid sampling must not reach measurement") }
                    )
                    .is_err()
                );
            }
        }
        let error = bandwidth(
            backend,
            Stores::Cached,
            BandwidthKind::Copy,
            &[0],
            usize::MAX - 127,
            1,
            0.001,
            |_, _| panic!("overflow must not reach measurement"),
        )
        .unwrap_err();
        assert!(error.contains("byte count overflow"));
    }
    #[test]
    fn copied_useful_bytes_count_both_streams_and_check_overflow() {
        assert_eq!(
            transferred_bytes(BandwidthKind::Read, 384, 7).unwrap(),
            2688
        );
        assert_eq!(
            transferred_bytes(BandwidthKind::Write, 384, 7).unwrap(),
            2688
        );
        assert_eq!(
            transferred_bytes(BandwidthKind::Copy, 384, 7).unwrap(),
            5376
        );
        assert!(transferred_bytes(BandwidthKind::Copy, usize::MAX / 2 + 1, 1).is_err());
        assert!(transferred_bytes(BandwidthKind::Copy, usize::MAX / 2, 2).is_err());
        assert!(transferred_bytes(BandwidthKind::Read, usize::MAX, 2).is_err());
    }
    #[test]
    fn reproducible_single_cycle_and_complete_traversals() {
        assert_eq!(Random(0).next(), 0xe220a8397b1dcdaf);
        for (bytes, stride, window) in [
            (8, 8, 0),
            (520, 8, 0),
            (4096, 128, 0),
            (4096, 128, 1536),
            (384, 128, 128),
            (4096, 128, 8192),
        ] {
            let mut a = Buffer::new(bytes).unwrap();
            let mut b = Buffer::new(bytes).unwrap();
            pointer_cycle(&mut a, stride, window, 123).unwrap();
            pointer_cycle(&mut b, stride, window, 123).unwrap();
            let nodes = bytes / stride;
            let mut seen = vec![false; nodes];
            let mut node = 0;
            for _ in 0..nodes {
                assert!(!seen[node]);
                seen[node] = true;
                let pa = unsafe { a.pointer.add(node * stride).cast::<*const u8>().read() };
                let pb = unsafe { b.pointer.add(node * stride).cast::<*const u8>().read() };
                assert!((a.pointer as usize..a.pointer as usize + bytes).contains(&(pa as usize)));
                assert!((b.pointer as usize..b.pointer as usize + bytes).contains(&(pb as usize)));
                assert_eq!((pa as usize - a.pointer as usize) % stride, 0);
                assert_eq!((pb as usize - b.pointer as usize) % stride, 0);
                let next = (pa as usize - a.pointer as usize) / stride;
                assert_eq!(next, (pb as usize - b.pointer as usize) / stride);
                assert!(next < nodes);
                node = next;
            }
            assert_eq!(node, 0);
            assert!(seen.iter().all(|&v| v));
            assert_eq!(unsafe { chase(a.pointer, nodes * 3) }, a.pointer);
            guard_check(&a);
            guard_check(&b);
        }
    }
    #[test]
    fn windows_are_shuffled_without_losing_locality_or_partial_nodes() {
        let (bytes, stride, window) = (4096, 128, 512);
        let mut data = Buffer::new(bytes).unwrap();
        pointer_cycle(&mut data, stride, window, 123).unwrap();
        for local_window in [window, 1536] {
            let mut local = Buffer::new(bytes).unwrap();
            pointer_cycle(&mut local, stride, local_window, 123).unwrap();
            let mut node = 0;
            let mut window_path = Vec::new();
            for _ in 0..bytes / stride {
                let current = node * stride / local_window;
                if window_path.last() != Some(&current) {
                    window_path.push(current);
                }
                let pointer =
                    unsafe { local.pointer.add(node * stride).cast::<*const u8>().read() };
                node = (pointer as usize - local.pointer as usize) / stride;
            }
            assert_eq!(node, 0);
            // Starting at node zero may split its window across the traversal boundary.
            if window_path.first() == window_path.last() {
                window_path.pop();
            }
            let mut unique = window_path.clone();
            unique.sort_unstable();
            assert_eq!(
                unique,
                (0..bytes.div_ceil(local_window)).collect::<Vec<_>>()
            );
            if local_window == window {
                assert_ne!(
                    window_path, unique,
                    "window order must not be linear for this seed"
                );
            }
            guard_check(&local);
        }

        let mut full = Buffer::new(bytes).unwrap();
        let mut oversized_window = Buffer::new(bytes).unwrap();
        let mut other_seed = Buffer::new(bytes).unwrap();
        pointer_cycle(&mut full, stride, 0, 123).unwrap();
        pointer_cycle(&mut oversized_window, stride, bytes * 2, 123).unwrap();
        pointer_cycle(&mut other_seed, stride, window, 124).unwrap();
        let successors = |buffer: &Buffer| {
            (0..bytes / stride)
                .map(|node| {
                    let pointer =
                        unsafe { buffer.pointer.add(node * stride).cast::<*const u8>().read() };
                    (pointer as usize - buffer.pointer as usize) / stride
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(successors(&full), successors(&oversized_window));
        assert_ne!(successors(&data), successors(&other_seed));
    }
    #[test]
    fn workers_complete_and_bad_startup_returns() {
        assert_eq!(
            (0..3)
                .map(|i| worker_bytes(128 * 5, 3, i))
                .collect::<Vec<_>>(),
            vec![256, 256, 128]
        );
        let cpus = platform::cpus();
        let cpus = &cpus[..cpus.len().min(3)];
        let backend = Backend::resolve("auto").unwrap();
        for kind in [
            BandwidthKind::Read,
            BandwidthKind::Write,
            BandwidthKind::Copy,
        ] {
            let mut samples = Vec::new();
            let result = bandwidth(
                backend,
                Stores::Cached,
                kind,
                cpus,
                4096,
                3,
                0.001,
                |index, value| {
                    assert_eq!(index, samples.len() + 1);
                    samples.push(value);
                },
            )
            .unwrap();
            assert_eq!(samples.len(), 3);
            assert_eq!(result.value, summarize(samples).value);
            assert!(result.value.is_finite() && result.value > 0.0);
            assert!(result.spread_percent.is_finite() && result.spread_percent >= 0.0);
        }
        #[cfg(any(windows, target_os = "linux"))]
        assert!(
            bandwidth(
                backend,
                Stores::Cached,
                BandwidthKind::Read,
                &[cpus[0], usize::MAX],
                4096,
                3,
                0.001,
                |_, _| {}
            )
            .is_err()
        );
        assert!(
            bandwidth(
                backend,
                Stores::Cached,
                BandwidthKind::Read,
                &[],
                4096,
                3,
                0.001,
                |_, _| {}
            )
            .is_err()
        );
        assert!(
            bandwidth(
                backend,
                Stores::Cached,
                BandwidthKind::Read,
                &cpus[..1],
                usize::MAX - 127,
                3,
                0.001,
                |_, _| {}
            )
            .is_err()
        );
        assert!(
            bandwidth(
                backend,
                Stores::Cached,
                BandwidthKind::Read,
                &[cpus[0], usize::MAX],
                128,
                3,
                0.001,
                |_, _| {}
            )
            .is_err()
        );
        let mut samples = Vec::new();
        let result = latency(520, 8, 0, 123, cpus[0], 3, 0.001, |index, value| {
            assert_eq!(index, samples.len() + 1);
            samples.push(value);
        })
        .unwrap();
        assert_eq!(samples.len(), 3);
        assert_eq!(result.value, summarize(samples).value);
        assert!(result.value.is_finite() && result.value > 0.0);
    }
    #[test]
    fn median_and_full_range() {
        let result = summarize(vec![4.0, 1.0, 3.0, 2.0]);
        assert_eq!(result.value, 2.5);
        assert_eq!(result.spread_percent, 120.0);
        assert!(validate_sampling(0, 0.001).is_err());
        assert!(validate_sampling(3, f64::NAN).is_err());
        assert!(validate_sampling(101, 0.001).is_err());
        assert!(validate_sampling(3, 0.000999).is_err());
        assert!(validate_sampling(3, 10.001).is_err());
        assert!(validate_sampling(1, 0.001).is_ok());
        assert!(validate_sampling(100, 10.0).is_ok());
    }
}
