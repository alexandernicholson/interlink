#!/usr/bin/env python3
"""Validate one matched service-mesh capture and generate its comparison table."""

import argparse
import base64
import csv
from datetime import datetime
import json
import math
import os
from pathlib import Path
import re
import sys
import tempfile

RESULTS_DIR = Path(__file__).resolve().parent.parent / "results"
MESHES = ("interlink", "linkerd", "istio-ambient")
VERSION_KEYS = ("kind", "kubernetes", "linkerd", "istio", "fortio")


def finite(value, label, minimum=0):
    if isinstance(value, bool):
        raise ValueError(f"{label}: expected a number")
    number = float(value)
    if not math.isfinite(number) or number < minimum:
        raise ValueError(f"{label}: invalid number")
    return number


def positive_integer(value, label):
    if type(value) is not int or value <= 0:
        raise ValueError(f"{label}: expected a positive integer")
    return value


def instant(value, label):
    if not isinstance(value, str):
        raise ValueError(f"{label}: missing timestamp")
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError(f"{label}: timestamp needs a timezone")
    return parsed.timestamp()


def read_capture(directory):
    capture = json.loads((directory / "capture.json").read_text(encoding="utf-8"))
    if capture["schema_version"] != 1 or capture["meshes"] != list(MESHES):
        raise ValueError("capture must contain exactly interlink, linkerd and istio-ambient")
    if not re.fullmatch(r"[0-9a-f]{40}", capture["revision"]):
        raise ValueError("capture revision must be a full source commit SHA")
    if not capture["run_id"] or not isinstance(capture["run_id"], str):
        raise ValueError("capture needs its run identifier")
    started = instant(capture["started_at"], "capture start")
    completed = instant(capture["completed_at"], "capture completion")
    if completed <= started:
        raise ValueError("capture completion must follow its start")
    for key in ("duration_seconds", "warmup_seconds", "payload_bytes", "server_delay_ms"):
        positive_integer(capture[key], key)
    profiles = []
    for profile in capture["profiles"]:
        profiles.append((
            positive_integer(profile["qps"], "profile RPS"),
            positive_integer(profile["connections"], "profile connections"),
        ))
    if not profiles or len(set(profiles)) != len(profiles):
        raise ValueError("capture needs distinct load profiles")
    for key in VERSION_KEYS:
        value = capture["versions"][key]
        if not isinstance(value, str) or not value or any(c in value for c in "\n\r|"):
            raise ValueError(f"capture needs a printable {key} version")
    return capture, profiles, started, completed


def read_fortio(path):
    text = path.read_text(encoding="utf-8")
    decoder = json.JSONDecoder()
    offset = 0
    while (offset := text.find("{", offset)) >= 0:
        try:
            record, end = decoder.raw_decode(text, offset)
        except json.JSONDecodeError:
            offset += 1
            continue
        if isinstance(record, dict) and "DurationHistogram" in record:
            return record
        offset = end
    raise ValueError(f"{path.name}: no Fortio result")


def read_metrics(path, columns, started, completed):
    rows = []
    with path.open(encoding="utf-8", newline="") as source:
        reader = csv.DictReader(source)
        if not reader.fieldnames or not {"timestamp", *columns} <= set(reader.fieldnames):
            raise ValueError(f"{path.name}: missing metric columns")
        for row in reader:
            timestamp = finite(row["timestamp"], f"{path.name} timestamp")
            values = [finite(row[key], f"{path.name} {key}") for key in columns]
            if rows and timestamp <= rows[-1][0]:
                raise ValueError(f"{path.name}: unordered metric timestamps")
            rows.append((timestamp, values))
    if len(rows) < 2 or rows[0][0] > started + 3 or rows[-1][0] < completed - 3:
        raise ValueError(f"{path.name}: metrics do not cover the measured load")
    measured = [values for timestamp, values in rows if started - 1 <= timestamp <= completed + 1]
    if len(measured) < 2:
        raise ValueError(f"{path.name}: missing measured-window samples")
    return measured


def measured_row(directory, mesh, qps, connections, capture, capture_start, capture_end):
    label = f"{mesh}-q{qps}-c{connections}"
    record = read_fortio(directory / mesh / f"fortio-{label}.json")
    if record["Version"] != capture["versions"]["fortio"]:
        raise ValueError(f"{label}: load generator version differs from this capture")
    if finite(record["RequestedQPS"], label) != qps or record["NumThreads"] != connections:
        raise ValueError(f"{label}: load differs from the capture manifest")
    if len(base64.b64decode(record["Payload"], validate=True)) != capture["payload_bytes"]:
        raise ValueError(f"{label}: payload differs from the capture manifest")
    duration = finite(record["ActualDuration"], f"{label} duration", minimum=1) / 1e9
    expected_duration = capture["duration_seconds"]
    if not expected_duration * 0.95 <= duration <= expected_duration * 1.10:
        raise ValueError(f"{label}: measured duration differs from the capture manifest")
    started = instant(record["StartTime"], f"{label} start")
    completed = started + duration
    if started < capture_start - 1 or completed > capture_end + 1:
        raise ValueError(f"{label}: result is outside this capture")
    codes = record["RetCodes"]
    if not codes or any(type(count) is not int or count < 0 for count in codes.values()):
        raise ValueError(f"{label}: invalid response counts")
    if not codes.get("200") or any(count for code, count in codes.items() if code != "200"):
        raise ValueError(f"{label}: requests failed")
    achieved = finite(record["ActualQPS"], f"{label} achieved RPS")
    if achieved < qps * 0.97:
        raise ValueError(f"{label}: achieved less than 97% of target RPS")
    histogram = record["DurationHistogram"]
    if histogram["Count"] != sum(codes.values()):
        raise ValueError(f"{label}: response and latency counts differ")
    percentiles = {entry["Percentile"]: finite(entry["Value"], label) * 1000
                   for entry in histogram["Percentiles"]}
    p50, p99 = percentiles[50], percentiles[99]
    if p50 <= 0 or p99 < p50:
        raise ValueError(f"{label}: invalid latency percentiles")
    metrics = directory / mesh / f"metrics-{label}.csv"
    proxy = read_metrics(metrics, ("cpu_millicores", "memory_rss_kb"), started, completed)
    host = read_metrics(metrics.with_suffix(".host.csv"), ("host_util_percent",), started, completed)
    if any(memory <= 0 for _, memory in proxy):
        raise ValueError(f"{label}: missing proxy memory measurements")
    if max(value[0] for value in host) >= 85:
        raise ValueError(f"{label}: host CPU reached the 85% saturation limit")
    return (
        mesh, qps, achieved, p50, p99,
        sum(cpu for cpu, _ in proxy) / len(proxy),
        sum(memory for _, memory in proxy) / len(proxy) / 1024,
    )


def comparison(directory):
    capture, profiles, started, completed = read_capture(directory)
    if {path.name for path in directory.iterdir() if path.is_dir()} != set(MESHES):
        raise ValueError("capture contains missing or unrelated mesh results")
    rows = []
    for mesh in MESHES:
        expected = {f"fortio-{mesh}-q{qps}-c{connections}.json" for qps, connections in profiles}
        actual = {path.name for path in (directory / mesh).glob("fortio-*.json")}
        if actual != expected:
            raise ValueError(f"{mesh}: incomplete capture or stale profiles")
        for qps, connections in profiles:
            rows.append(measured_row(directory, mesh, qps, connections, capture, started, completed))
    revision = capture["revision"]
    versions = capture["versions"]
    lines = [
        "# Service Mesh Benchmark Comparison",
        "",
        f"Captured {capture['completed_at']}. Interlink revision "
        f"[`{revision[:12]}`](https://github.com/alexandernicholson/interlink/commit/{revision}).",
        "",
        f"HTTP echo: {capture['payload_bytes']:,} B payload, {capture['server_delay_ms']} ms "
        f"application delay, {capture['duration_seconds']} s measured per profile. "
        "Client and server use separate workers in the same three-node kind layout. "
        "Interlink and Linkerd use sidecars; Istio ambient uses node-level ztunnel.",
        "",
        "Pinned versions: " + ", ".join(f"{key.replace('_', '-')} {versions[key]}" for key in VERSION_KEYS) + ".",
        "",
        "| Mesh | Target RPS | Achieved RPS | p50 ms | p99 ms | Proxy CPU (m) | Memory (MiB) |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for mesh, qps, achieved, p50, p99, cpu, memory in rows:
        lines.append(f"| {mesh} | {qps} | {achieved:.1f} | {p50:.2f} | {p99:.2f} | {cpu:.1f} | {memory:.1f} |")
    lines.extend([
        "",
        "Only complete matched captures with zero request errors, at least 97% of target RPS "
        "and host CPU below 85% are published. CPU and RSS sum the proxy containers "
        "using kubelet statistics; these are not per-connection allocations.",
        "",
        "[Capture metadata](capture.json) · [Run the comparison](../README.md)",
        "",
    ])
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, default=RESULTS_DIR)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--check", action="store_true", help="fail if the published table is stale")
    args = parser.parse_args()
    output = args.output or args.results / "comparison.md"
    try:
        text = comparison(args.results)
        if args.check:
            if output.read_text(encoding="utf-8") != text:
                raise ValueError("comparison table is stale; regenerate it from its capture")
        else:
            with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=output.parent,
                                             prefix=".comparison-", delete=False) as temporary:
                temporary.write(text)
                pending = temporary.name
            os.replace(pending, output)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"benchmark report: {error}", file=sys.stderr)
        return 1
    print(f"Verified comparison: {output}" if args.check else f"Wrote comparison: {output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
