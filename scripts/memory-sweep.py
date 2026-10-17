#!/usr/bin/env python3
"""Compare fixed-worker cached/streaming workloads; no performance pass threshold."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time

SIZES = (64, 256, 512, 1024)
MODES = ("cached", "streaming")
TESTS = ("read", "write", "copy", "latency")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False, allow_nan=False) + "\n",
                    encoding="utf-8")


def host_details():
    details = {"os": platform.system(), "release": platform.release(),
               "arch": platform.machine(), "logical_cpus": os.cpu_count(),
               "python": platform.python_version()}
    command = None
    if sys.platform == "win32":
        command = ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                   "Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,"
                   "NumberOfLogicalProcessors,MaxClockSpeed | ConvertTo-Json -Compress"]
    elif sys.platform == "darwin":
        command = ["sysctl", "machdep.cpu.brand_string", "hw.model", "hw.memsize",
                   "hw.ncpu", "hw.physicalcpu", "hw.logicalcpu", "hw.cachelinesize",
                   "hw.l1dcachesize", "hw.l2cachesize", "hw.l3cachesize"]
    elif sys.platform.startswith("linux"):
        try:
            with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as source:
                text = source.read(1 << 20)
            details["cpu"] = list(dict.fromkeys(
                line.strip() for line in text.splitlines()
                if line.split(":", 1)[0].strip() in ("model name", "Hardware", "Processor")))
        except OSError as error:
            details["cpu_error"] = str(error)
    if command:
        try:
            result = subprocess.run(command, capture_output=True, text=True,
                                    encoding="utf-8", errors="replace", timeout=10)
            details["cpu_query"] = {"command": command, "returncode": result.returncode,
                                    "stdout": result.stdout[:16384],
                                    "stderr": result.stderr[:16384]}
        except (OSError, subprocess.TimeoutExpired) as error:
            details["cpu_query"] = {"command": command, "error": str(error)}
    details["ci"] = {key: os.environ[key] for key in
                     ("GITHUB_SHA", "GITHUB_RUN_ID", "RUNNER_OS", "RUNNER_ARCH",
                      "ImageOS", "ImageVersion") if key in os.environ}
    return details


def validate(result, size, mode):
    if result.get("schema") != 3 or result.get("method") not in (3, 4):
        raise ValueError("expected memtest schema 3, method 3 or 4")
    expected = {"threads": 2, "bandwidth_bytes": size << 20,
                "latency_bytes": size << 20, "stores": mode, "samples": 7,
                "min_time": 0.2, "stride": 128, "window": 2048 << 10, "seed": 1}
    if any(result.get("config", {}).get(key) != value for key, value in expected.items()):
        raise ValueError("benchmark changed the requested fixed settings")
    if result.get("worker_selection") != "fixed" or result.get("caches") != []:
        raise ValueError("expected fixed workers and memory-only results")
    if result.get("copy_convention") != "read_plus_write_bytes" or result.get("memory_copy_buffers") != 2:
        raise ValueError("unexpected copy footprint/accounting")
    if set(result.get("results", {})) != set(TESTS):
        raise ValueError("expected all four memory tests")
    for test in TESTS:
        measurement = result["results"][test]
        for key in ("ns" if test == "latency" else "mb_per_s", "range_percent"):
            value = measurement.get(key)
            if (isinstance(value, bool) or not isinstance(value, (int, float))
                    or not math.isfinite(value) or value < 0 or (key != "range_percent" and value == 0)):
                raise ValueError(f"invalid {test} {key}")


def summary(records):
    lines = ["### Fixed-worker memory sweep", "",
             "2 bandwidth workers; latency uses 1. Seven samples × 0.2 s target; two process repeats in reversed order.",
             "Sizes are total MiB per stream across workers; Copy allocates two streams and counts read + write bytes.",
             "Read and latency are unaffected by store policy. The 64 MiB control may fit cache; no size guarantees RAM isolation.",
             "ARM STNP is a streaming hint, not guaranteed cache bypass. Host scheduling and cache effects remain part of these measurements.",
             "Values are medians of the two process medians. Change = 100 × (streaming / cached − 1); positive latency change means slower.",
             "", "| MiB / stream | Test | Cached | Streaming | Change |",
             "| ---: | --- | ---: | ---: | ---: |"]
    for size in SIZES:
        for test in TESTS:
            key = "ns" if test == "latency" else "mb_per_s"
            values = {}
            for mode in MODES:
                measurements = [record["result"]["results"][test][key] for record in records
                                if record["size_mib"] == size and record["stores"] == mode
                                and "result" in record]
                if len(measurements) == 2:
                    values[mode] = statistics.median(measurements)
            if len(values) == 2:
                cached, streaming = values["cached"], values["streaming"]
                unit = "ns" if test == "latency" else "MB/s"
                lines.append(f"| {size} | {test} ({unit}) | {cached:.2f} | {streaming:.2f} | "
                             f"{100 * (streaming / cached - 1):+.1f}% |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path, help="new or empty directory for raw JSON and metadata")
    args = parser.parse_args()
    binary, output = args.binary.resolve(), args.output.resolve()
    if not binary.is_file():
        raise ValueError(f"binary does not exist: {binary}")
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise ValueError(f"output directory is not empty: {output}")
    binary_hash = hashlib.sha256()
    with binary.open("rb") as source:
        while chunk := source.read(1 << 20):
            binary_hash.update(chunk)
    manifest = {"schema": 1, "started_utc": datetime.now(timezone.utc).isoformat(),
                "binary": str(binary), "binary_sha256": binary_hash.hexdigest(),
                "host": host_details(), "records": []}
    cases = [(size, mode) for size in SIZES for mode in MODES]
    try:
        for repeat in (1, 2):
            for size, mode in cases if repeat == 1 else reversed(cases):
                name = f"repeat-{repeat}-{size}mib-{mode}"
                command = [str(binary), "--cli", "--level", "memory", "--test", "all",
                           "--threads", "2", "--size-mib", str(size), "--latency-size-mib", str(size),
                           "--stores", mode, "--samples", "7", "--min-time", "0.2", "--isa", "auto",
                           "--stride", "128", "--window-kib", "2048", "--seed", "1", "--json"]
                record = {"repeat": repeat, "size_mib": size, "stores": mode,
                          "command": command, "raw_json": name + ".json"}
                manifest["records"].append(record)
                print(f"{name}: running", flush=True)
                began = time.monotonic()
                try:
                    result = subprocess.run(command, capture_output=True, timeout=180)
                except subprocess.TimeoutExpired as error:
                    record.update(elapsed_seconds=time.monotonic() - began, timed_out=True)
                    (output / record["raw_json"]).write_bytes(error.stdout or b"")
                    if error.stderr:
                        (output / (name + ".stderr.txt")).write_bytes(error.stderr)
                    raise
                record.update(elapsed_seconds=time.monotonic() - began, returncode=result.returncode)
                (output / record["raw_json"]).write_bytes(result.stdout)
                if result.stderr:
                    (output / (name + ".stderr.txt")).write_bytes(result.stderr)
                if result.returncode:
                    raise ValueError(f"{name} failed ({result.returncode}); see raw output")
                parsed = json.loads(result.stdout)
                validate(parsed, size, mode)
                record["result"] = parsed
                write_json(output / "manifest.json", manifest)
                print(f"{name}: {record['elapsed_seconds']:.1f} s", flush=True)
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        manifest["error"] = str(error)
        raise
    finally:
        manifest["finished_utc"] = datetime.now(timezone.utc).isoformat()
        manifest["complete"] = sum("result" in record for record in manifest["records"]) == 2 * len(cases)
        write_json(output / "manifest.json", manifest)
        report = summary(manifest["records"])
        if not manifest["complete"]:
            report += "\nSweep incomplete; see manifest.json for completed runs and any error.\n"
        (output / "summary.md").write_text(report, encoding="utf-8")
        if os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as destination:
                destination.write(report)
    print(output / "summary.md")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        sys.exit(str(error))
