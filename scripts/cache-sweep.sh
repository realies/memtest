#!/bin/sh
# macOS cache/locality diagnostic; uses only the existing binary and OS tools.
set -eu
export LC_ALL=C

fail() { printf '%s\n' "$*" >&2; exit 1; }
binary=
quick=no
for argument do
    case "$argument" in
        --quick) quick=yes ;;
        -h|--help)
            printf '%s\n' 'Usage: /bin/sh cache-sweep.sh [memtest] [--quick]' \
                'Without a binary path, a file picker opens. Results stay in a new local folder.'
            exit 0 ;;
        --*) fail "Unknown option: $argument" ;;
        *) [ -z "$binary" ] || fail 'Specify only one executable'; binary=$argument ;;
    esac
done
[ "$(uname -s)" = Darwin ] || fail 'This diagnostic requires macOS.'
if [ -z "$binary" ]; then
    binary=$(osascript -e 'POSIX path of (choose file with prompt "Choose the memtest executable")') \
        || fail 'No executable selected.'
fi
[ -f "$binary" ] && [ -x "$binary" ] || fail "Not an executable file: $binary"
case "$binary" in /*) ;; *) binary="$(pwd -P)/$binary" ;; esac
destination=$(pwd -P)
if [ -d "$HOME/Desktop" ] && [ -w "$HOME/Desktop" ]; then destination=$HOME/Desktop; fi
output=$(mktemp -d "$destination/memtest-cache-sweep.XXXXXX")
complete=no
trap 'if [ "$complete" != yes ]; then printf "Incomplete sweep; outputs preserved: %s\n" "$output" >&2; fi' 0
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Results: %s\n' "$output"

sizes='6 12 18 24 36 64 96 256 512'
reverse_sizes='512 256 96 64 36 24 18 12 6'
samples=5
minimum=0.1
if [ "$quick" = yes ]; then
    sizes='6 24 96'
    reverse_sizes='96 24 6'
    samples=3
    minimum=0.01
fi
{
    date -u '+started_utc: %Y-%m-%dT%H:%M:%SZ'
    printf 'binary: %s\nmode: %s\n' "$binary" "$quick"
    uname -m
    sw_vers
    for key in machdep.cpu.brand_string hw.model hw.memsize hw.ncpu hw.physicalcpu \
        hw.logicalcpu hw.pagesize hw.cachelinesize hw.l1dcachesize hw.l2cachesize \
        hw.l3cachesize hw.nperflevels kern.hv_vmm_present; do
        sysctl "$key" 2>/dev/null || printf '%s: unavailable\n' "$key"
    done
    for level in 0 1 2 3; do
        for field in name physicalcpu logicalcpu cpusperl2 cpusperl3 l1dcachesize l2cachesize l3cachesize; do
            sysctl "hw.perflevel$level.$field" 2>/dev/null || :
        done
    done
} > "$output/host.txt"
"$binary" --cli --version > "$output/version.txt" 2> "$output/version.stderr.txt" \
    || fail 'Cannot run the selected executable; see version.stderr.txt.'
"$binary" --cli --help > "$output/help.txt" 2> "$output/help.stderr.txt" \
    || fail 'Cannot read executable options; see help.stderr.txt.'
shasum -a 256 "$binary" > "$output/binary.sha256"
snapshot() {
    date -u '+utc: %Y-%m-%dT%H:%M:%SZ'
    uptime
    vm_stat
    pmset -g therm 2>&1 || :
}
snapshot > "$output/before.txt"
cat > "$output/README.txt" <<EOF
Cache/locality diagnostic: one OS-scheduled worker, cached stores, 128-byte stride,
seed 1, $samples samples, $minimum seconds per sample target; two reversed-order repeats.
Footprint MiB is Read/Write storage, combined Copy source + destination, or latency span.
Copy uses half that size per stream and reports bytes read + written (decimal MB/s).
Latency compares full shuffle with 2 MiB blocks; values are ns per dependent load.
The JSON records actual touched cache-line footprint and reported cache/page geometry.
This is a CPU-visible workload sweep, not proof of SLC residency or capacity.
Locality differences also include translation/prefetch effects. time -l statistics include
startup/allocation; page faults are not TLB misses, and context switches do not identify
P/E-core residency. No CPU pinning or hardware counter collection is performed.
runs.tsv contains individual process medians/ranges; summary.tsv combines both repeats.
Raw JSON, exact profiles, stderr and process resource statistics are preserved per case.
If supported by the selected binary, cache-probe.json records paired latency controls;
cache-probe.tsv summarizes them without identifying any condition as an SLC hit.
EOF
cat > "$output/summarize.js" <<'JAVASCRIPT'
ObjC.import('Foundation');
function read(path) {
    var text = ObjC.unwrap($.NSString.stringWithContentsOfFileEncodingError(path, $.NSUTF8StringEncoding, Ref()));
    if (typeof text !== 'string') throw new Error('Cannot read ' + path);
    return text;
}
function number(value, positive) {
    return typeof value === 'number' && isFinite(value) && (positive ? value > 0 : value >= 0);
}
function run(args) {
    if (args[0] === 'probe') {
        var probe = JSON.parse(read(args[1])), count = +args[2];
        if (probe.schema !== 1 || probe.diagnostic !== 'cache_probe' || probe.os !== 'macos' ||
            probe.affinity !== 'os_scheduled' || probe.cpu_id !== null || probe.samples_per_condition !== count)
            throw new Error('Unexpected cache-probe schema, placement or trial count');
        var labels = ['hot', 'capacity_evicted', 'cold'];
        if (Object.keys(probe.results || {}).length !== labels.length) throw new Error('Missing probe conditions');
        var summary = ['condition\tmedian_ns\trange_percent\ttrials'];
        labels.forEach(function(label) {
            var value = probe.results[label];
            if (!value || !number(value.ns, true) || !number(value.range_percent, false) ||
                !Array.isArray(value.samples_ns) || value.samples_ns.length !== count ||
                !value.samples_ns.every(function(sample) { return number(sample, true); }))
                throw new Error('Invalid probe measurements: ' + label);
            summary.push([label, value.ns, value.range_percent, count].join('\t'));
        });
        return summary.join('\n');
    }
    if (args[0] === 'check') {
        var r = JSON.parse(read(args[1]));
        var repeat = +args[2], size = +args[3], test = args[4], locality = args[5];
        var expected = {threads: 1, cache_threads: 1, stores: 'cached',
            bandwidth_bytes: +args[6], latency_bytes: size * 1048576,
            stride: 128, window: +args[7], seed: 1, samples: +args[8], min_time: +args[9]};
        if (r.schema !== 3 || (r.method !== 3 && r.method !== 4) || r.os !== 'macos' ||
            r.worker_selection !== 'fixed' || r.affinity !== 'os_scheduled' || r.cpu_ids !== null ||
            r.store_implementation !== 'cached' || r.copy_convention !== 'read_plus_write_bytes' ||
            r.memory_copy_buffers !== 2 || !Array.isArray(r.caches) || r.caches.length || r.slc != null)
            throw new Error('Unexpected method, placement or workload scope');
        if (args[11] && r.method !== +args[11]) throw new Error('Benchmark method changed during sweep');
        Object.keys(expected).forEach(function(key) {
            if (!r.config || r.config[key] !== expected[key]) throw new Error('Changed setting: ' + key);
        });
        var measurement = r.results && r.results[test], unit = test === 'latency' ? 'ns' : 'mb_per_s';
        if (Object.keys(r.results || {}).length !== 1 || !measurement ||
            !number(measurement[unit], true) || !number(measurement.range_percent, false) ||
            !number(r.page_bytes, true) || !number(r.cache_line_bytes, false) ||
            (r.latency_cache_footprint_bytes !== null && !number(r.latency_cache_footprint_bytes, true)))
            throw new Error('Missing, nonnumeric or nonfinite measurement/geometry');
        return [r.method, repeat, size, test, locality, measurement[unit], unit, measurement.range_percent,
            r.page_bytes, r.cache_line_bytes, r.latency_cache_footprint_bytes === null ? 'unknown' : r.latency_cache_footprint_bytes,
            r.affinity, r.backend, args[10]].join('\t');
    }
    var lines = read(args[1]).trim().split(/\r?\n/).slice(1), groups = {};
    lines.forEach(function(line) {
        var row = line.split('\t'), key = row.slice(2, 5).join('\t');
        if (!groups[key]) groups[key] = [];
        groups[key].push(row);
    });
    var output = ['method\tfootprint_mib\ttest\tlocality\tunit\trepeat1_median\trepeat2_median\tmedian_of_medians\tmax_range_percent\trepeat_spread_percent'];
    Object.keys(groups).sort(function(a, b) {
        var x = a.split('\t'), y = b.split('\t');
        return +x[0] - +y[0] || a.localeCompare(b);
    }).forEach(function(key) {
        var rows = groups[key].sort(function(a, b) { return +a[1] - +b[1]; });
        if (rows.length !== 2 || rows[0][1] !== '1' || rows[1][1] !== '2')
            throw new Error('Incomplete repeats for ' + key);
        var first = +rows[0][5], second = +rows[1][5], median = (first + second) / 2;
        output.push([rows[0][0], key, rows[0][6], first, second, median,
            Math.max(+rows[0][7], +rows[1][7]), 100 * Math.abs(first - second) / median].join('\t'));
    });
    return output.join('\n');
}
JAVASCRIPT
printf 'method\trepeat\tfootprint_mib\ttest\tlocality\tmedian\tunit\trange_percent\tpage_bytes\tcache_line_bytes\tlatency_cache_footprint_bytes\taffinity\tbackend\traw_json\n' > "$output/runs.tsv"
printf 'case\ttest\tstream_mib\tlatency_mib\twindow_kib\n' > "$output/commands.tsv"
method=
for repeat in 1 2; do
    order=$sizes
    cases='read write copy full block'
    if [ "$repeat" = 2 ]; then order=$reverse_sizes; cases='block full copy write read'; fi
    for size in $order; do
        for case_name in $cases; do
            test=$case_name
            locality=contiguous
            window=0
            stream=$size
            case "$case_name" in
                copy) stream=$((size / 2)) ;;
                full) test=latency; locality=full ;;
                block) test=latency; locality=block; window=2048 ;;
            esac
            name="repeat-$repeat-${size}mib-$case_name"
            printf '%s\n' "$name"
            printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$test" "$stream" "$size" "$window" >> "$output/commands.tsv"
            /usr/bin/time -l -o "$output/$name.time.txt" "$binary" --cli --level memory \
                --test "$test" --threads 1 --size-mib "$stream" --latency-size-mib "$size" \
                --stores cached --isa auto --stride 128 --window-kib "$window" --seed 1 \
                --samples "$samples" --min-time "$minimum" --save-profile "$output/$name.profile" \
                --json > "$output/$name.json" 2> "$output/$name.stderr.txt" \
                || fail "$name failed; see its stderr/time files."
            osascript -l JavaScript "$output/summarize.js" check "$output/$name.json" \
                "$repeat" "$size" "$test" "$locality" "$((stream * 1048576))" \
                "$((window * 1024))" "$samples" "$minimum" "$name.json" "$method" \
                >> "$output/runs.tsv" 2> "$output/$name.validation.txt" \
                || fail "$name returned invalid JSON; see its validation.txt file."
            method=$(awk 'NR == 2 { print $1; exit }' "$output/runs.tsv")
        done
    done
done
osascript -l JavaScript "$output/summarize.js" summary "$output/runs.tsv" \
    > "$output/summary.tsv" 2> "$output/summary.stderr.txt" || fail 'Cannot summarize results.'
if grep -q -- '--cache-probe' "$output/help.txt"; then
    set -- --cli --cache-probe --json
    trials=21
    if [ "$quick" = yes ]; then set -- "$@" --quick; trials=5; fi
    printf '%s\n' 'cache-probe'
    /usr/bin/time -l -o "$output/cache-probe.time.txt" "$binary" "$@" \
        > "$output/cache-probe.json" 2> "$output/cache-probe.stderr.txt" \
        || fail 'Cache probe failed; main sweep outputs are preserved.'
    osascript -l JavaScript "$output/summarize.js" probe "$output/cache-probe.json" "$trials" \
        > "$output/cache-probe.tsv" 2> "$output/cache-probe.validation.txt" \
        || fail 'Cache probe returned invalid JSON; main sweep outputs are preserved.'
else
    printf '%s\n' 'Cache probe is not supported by this executable.' > "$output/cache-probe.txt"
fi
snapshot > "$output/after.txt"
date -u '+finished_utc: %Y-%m-%dT%H:%M:%SZ' >> "$output/host.txt"
complete=yes
printf 'Complete: %s/summary.tsv\n' "$output"
