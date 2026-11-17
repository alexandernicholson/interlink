#!/usr/bin/env python3
"""Validate one complete echo-capacity capture and synchronize both generated views."""

import argparse
from dataclasses import dataclass
import csv
from datetime import datetime
import json
import math
import os
from pathlib import Path
import re
import sys
import tempfile

from plot import render

RESULTS_DIR = Path(__file__).resolve().parent.parent / "results"
README_PATH = RESULTS_DIR.parents[1] / "README.md"
HEADLINE_BEGIN = "<!-- BEGIN GENERATED MESH HEADLINE -->"
HEADLINE_END = "<!-- END GENERATED MESH HEADLINE -->"
MESHES = ("interlink", "linkerd", "istio-ambient")
VERSION_KEYS = ("kind", "kubernetes", "linkerd", "istio", "fortio")
WORKLOADS = (("rps", 1024), ("bulk", 1048576))
HARNESS_ROOT_FILES = (
    "Dockerfile", "Cargo.toml", "Cargo.lock",
    "bench/Makefile", "bench/run.sh", "bench/setup.sh", "bench/teardown.sh",
)
HARNESS_INPUT_DIRS = ("bench/scripts", "bench/workloads", "bench/manifests")


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
    if capture["schema_version"] != 2 or capture["benchmark"] != "echo-capacity":
        raise ValueError("expected a fresh schema-2 echo-capacity capture, not fixed-load data")
    if capture["meshes"] != list(MESHES):
        raise ValueError("capture must contain exactly interlink, linkerd and istio-ambient")
    if not re.fullmatch(r"[0-9a-f]{40}", capture["revision"]):
        raise ValueError("capture revision must be a full source commit SHA")
    hashes = capture["harness_sha256"]
    if not isinstance(hashes, dict) or not set(HARNESS_ROOT_FILES) <= hashes.keys():
        raise ValueError("capture needs harness/workload/build-input SHA256 provenance")
    for relative, digest in hashes.items():
        if (not isinstance(relative, str) or "\\" in relative or Path(relative).as_posix() != relative
                or ".." in Path(relative).parts
                or not (relative in HARNESS_ROOT_FILES
                        or any(relative.startswith(directory + "/") for directory in HARNESS_INPUT_DIRS))
                or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest)):
            raise ValueError("invalid relative-path harness SHA256 provenance")
    if any(not any(name.startswith(directory + "/") for name in hashes) for directory in HARNESS_INPUT_DIRS):
        raise ValueError("capture is missing a harness/workload/manifest input group")
    printable(capture["run_id"], "capture run identifier")
    started = instant(capture["started_at"], "capture start")
    completed = instant(capture["completed_at"], "capture completion")
    if completed <= started:
        raise ValueError("capture completion must follow its start")
    for key in ("duration_seconds", "warmup_seconds", "request_timeout_seconds"):
        positive_integer(capture[key], key)
    if type(capture["server_delay_ms"]) is not int or capture["server_delay_ms"] != 0:
        raise ValueError("echo capacity requires zero application delay")
    if "profiles" in capture or "payload_bytes" in capture:
        raise ValueError("fixed-load profile metadata cannot describe echo capacity")
    workloads = capture["workloads"]
    if [(w["name"], w["payload_bytes"]) for w in workloads] != list(WORKLOADS):
        raise ValueError("capture needs the declared 1 KiB RPS and 1 MiB bulk workloads")
    for workload in workloads:
        positive_integer(workload["payload_bytes"], "workload payload")
        grid = workload["concurrency"]
        if not isinstance(grid, list) or not grid or len(set(grid)) != len(grid):
            raise ValueError("workload needs a distinct concurrency grid")
        for concurrency in grid:
            positive_integer(concurrency, "workload concurrency")
    for key in VERSION_KEYS:
        printable(capture["versions"][key], f"{key} version")
    host = capture["host"]
    if host["system"] != "Linux":
        raise ValueError("echo capacity must be measured on Linux")
    for key in ("architecture", "kernel"):
        printable(host[key], f"host {key}")
    for key in ("logical_cpus", "memory_bytes"):
        positive_integer(host[key], f"host {key}")
    return capture, started, completed


def printable(value, label):
    if not isinstance(value, str) or not value or any(c in value for c in "\n\r|`"):
        raise ValueError(f"{label}: expected printable text")
    return value


def requested_seconds(value):
    """Parse Fortio's Go duration spelling, e.g. 20s or 1m0s."""
    if not isinstance(value, str):
        raise ValueError("missing requested duration")
    parts = re.findall(r"([0-9]+(?:\.[0-9]+)?)(h|ms|us|µs|ns|m|s)", value)
    if not parts or "".join(number + unit for number, unit in parts) != value:
        raise ValueError("invalid requested duration")
    scale = {"h": 3600, "m": 60, "s": 1, "ms": 1e-3, "us": 1e-6, "µs": 1e-6, "ns": 1e-9}
    return sum(float(number) * scale[unit] for number, unit in parts)


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


def response_counts(codes, label):
    if not isinstance(codes, dict) or not codes:
        raise ValueError(f"{label}: missing response counts")
    for code, count in codes.items():
        if (not isinstance(code, str) or not re.fullmatch(r"-1|[1-5][0-9]{2}", code)
                or type(count) is not int or count < 0):
            raise ValueError(f"{label}: invalid response counts")
    total = sum(codes.values())
    positive_integer(total, f"{label} response count")
    return total, total - codes.get("200", 0)


def histogram(record, name, count, label, minimum=0, sum_minimum=0):
    data = record[name]
    if type(data["Count"]) is not int or data["Count"] != count:
        raise ValueError(f"{label}: {name} count differs from responses")
    values = {key: finite(data[key], f"{label} {name} {key}",
                          minimum=sum_minimum if key == "Sum" else 0 if key == "StdDev" else minimum)
              for key in ("Min", "Max", "Sum", "Avg", "StdDev")}
    if values["Max"] < values["Min"]:
        raise ValueError(f"{label}: reversed {name} bounds")
    if (not math.isclose(values["Sum"], values["Avg"] * count, rel_tol=1e-6, abs_tol=1e-6)
            or values["Sum"] < values["Min"] * count - 1e-6
            or values["Sum"] > values["Max"] * count + 1e-6):
        raise ValueError(f"{label}: inconsistent {name} totals")
    for bucket in data.get("Data") or []:
        for key in ("Start", "End", "Percent", "Count"):
            finite(bucket[key], f"{label} {name} bucket {key}",
                   minimum=minimum if key in ("Start", "End") else 0)
    return data


@dataclass(frozen=True)
class Point:
    mesh: str
    workload: str
    concurrency: int
    rps: float
    p99_ms: float
    payload_mib_s: float
    cpu_millicores: float
    rss_mib: float
    requests: int
    errors: int
    warmup_errors: int

    @property
    def eligible(self):
        return self.errors == 0 and self.warmup_errors == 0


def measured_point(directory, mesh, workload, concurrency, capture, capture_start, capture_end, requested_qps=0):
    label = f"{mesh}-{workload['name']}-c{concurrency}"
    record = json.loads((directory / mesh / f"fortio-{label}.json").read_text(encoding="utf-8"))
    if record["Version"] != capture["versions"]["fortio"]:
        raise ValueError(f"{label}: load generator version differs from this capture")
    standard_client = workload["name"] == "bulk"
    if record["DisableFastClient"] is not standard_client:
        raise ValueError(f"{label}: Fortio client mode differs from the declared workload")
    if finite(record["HTTPReqTimeOut"], f"{label} request timeout", minimum=1) != capture["request_timeout_seconds"] * 1e9:
        raise ValueError(f"{label}: request timeout differs from the declared experiment")
    if positive_integer(record["NumThreads"], label) != concurrency:
        raise ValueError(f"{label}: concurrency differs from the declared measurement")
    if requested_qps:
        if finite(record["RequestedQPS"], f"{label} offered RPS", minimum=1) != requested_qps:
            raise ValueError(f"{label}: offered load differs from the shared target")
    elif record["RequestedQPS"] not in ("max", "0", 0) or isinstance(record["RequestedQPS"], bool):
        raise ValueError(f"{label}: expected unlimited offered load")
    if ("Payload" in record or positive_integer(record["PayloadBytes"], label) != workload["payload_bytes"]
            or not re.fullmatch(r"[0-9a-f]{64}", record["PayloadSHA256"])):
        raise ValueError(f"{label}: invalid normalized request payload")
    duration = finite(record["ActualDuration"], f"{label} duration", minimum=1) / 1e9
    if (requested_seconds(record["RequestedDuration"]) != capture["duration_seconds"]
            or duration < capture["duration_seconds"] * 0.95):
        raise ValueError(f"{label}: duration differs from the declared measurement phase")
    started = instant(record["StartTime"], f"{label} start")
    completed = started + duration
    if started < capture_start - 1 or completed > capture_end + 1:
        raise ValueError(f"{label}: result is outside this capture")
    total, errors = response_counts(record["RetCodes"], label)
    _, warmup_errors = response_counts(record["WarmupRetCodes"], f"{label} warmup")
    rps = finite(record["ActualQPS"], f"{label} achieved RPS")
    if not math.isclose(rps, total / duration, rel_tol=1e-6, abs_tol=1e-6):
        raise ValueError(f"{label}: achieved RPS differs from response count and duration")
    if requested_qps and (errors or warmup_errors or not 0.97 * requested_qps <= rps <= 1.03 * requested_qps):
        raise ValueError(f"{label}: shared load needs error-free requests within 3% of target")
    latency = histogram(record, "DurationHistogram", total, label)
    # Fortio 1.66 considers 2xx and 418 successful, even its synthetic 204
    # after a partial standard-client read. Echo eligibility still requires 200.
    fortio_errors = sum(count for code, count in record["RetCodes"].items()
                        if not (200 <= int(code) < 300 or code == "418"))
    histogram(record, "ErrorsDurationHistogram", fortio_errors, label)
    for percentile in latency["Percentiles"]:
        if finite(percentile["Percentile"], f"{label} percentile") > 100:
            raise ValueError(f"{label}: invalid latency percentile")
        finite(percentile["Value"], f"{label} latency")
    p99_values = [finite(p["Value"], f"{label} p99") for p in latency["Percentiles"] if p["Percentile"] == 99]
    if len(p99_values) != 1 or (not errors and p99_values[0] <= 0):
        raise ValueError(f"{label}: missing or invalid p99 latency")
    sizes = histogram(record, "Sizes", total, label,
                      minimum=-1 if standard_client and errors else 0,
                      sum_minimum=-errors if standard_client else 0)
    if standard_client:
        # The standard client records body-only Sizes, with -1 on failed reads.
        # Header byte observations are zero; pinned Fortio may count those zeros.
        header_count = record["HeaderSizes"]["Count"]
        if type(header_count) is not int or header_count not in (0, total):
            raise ValueError(f"{label}: invalid standard-client header sample count")
        headers = histogram(record, "HeaderSizes", header_count, label)
        if (any(headers[key] != 0 for key in ("Min", "Max", "Sum", "Avg", "StdDev"))
                or any(bucket[key] != 0 for bucket in headers.get("Data") or []
                       for key in ("Start", "End"))):
            raise ValueError(f"{label}: standard client must have empty header statistics")
        payload_bytes = finite(sizes["Sum"], f"{label} echoed payload bytes", minimum=-errors)
    else:
        headers = histogram(record, "HeaderSizes", total, label)
        if not errors and headers["Sum"] <= 0:
            raise ValueError(f"{label}: fast client is missing measured response headers")
        payload_bytes = finite(sizes["Sum"] - headers["Sum"], f"{label} echoed payload bytes")
    if not errors and payload_bytes != total * workload["payload_bytes"]:
        raise ValueError(f"{label}: response payload differs from the captured request payload")
    metrics = directory / mesh / f"metrics-{label}.csv"
    proxy = read_metrics(metrics, ("cpu_millicores", "memory_rss_kb"), started, completed)
    host = read_metrics(metrics.with_suffix(".host.csv"), ("host_util_percent",), started, completed)
    if any(memory <= 0 for _, memory in proxy):
        raise ValueError(f"{label}: missing proxy RSS measurements")
    if any(value[0] > 100 for value in host):
        raise ValueError(f"{label}: invalid host CPU utilization")
    return Point(mesh, workload["name"], concurrency, rps, p99_values[0] * 1000,
                 payload_bytes / duration / 2**20,
                 sum(cpu for cpu, _ in proxy) / len(proxy),
                 sum(memory for _, memory in proxy) / len(proxy) / 1024,
                 total, errors, warmup_errors)


def load_results(directory):
    capture, started, completed = read_capture(directory)
    if {p.name for p in directory.iterdir() if p.is_dir()} != set(MESHES):
        raise ValueError("capture contains missing or unrelated mesh results")
    points = []
    for mesh in MESHES:
        labels = [f"{mesh}-{w['name']}-c{c}" for w in capture["workloads"] for c in w["concurrency"]]
        if "shared_load" in capture:
            labels.append(f"{mesh}-shared-c{positive_integer(capture['shared_load']['concurrency'], 'shared concurrency')}")
        expected = {f"{prefix}-{label}{suffix}" for label in labels
                    for prefix, suffix in (("fortio", ".json"), ("metrics", ".csv"), ("metrics", ".host.csv"))}
        if {p.name for p in (directory / mesh).iterdir()} != expected:
            raise ValueError(f"{mesh}: incomplete grid or stale/unrelated raw evidence")
        for workload in capture["workloads"]:
            for concurrency in workload["concurrency"]:
                points.append(measured_point(directory, mesh, workload, concurrency, capture, started, completed))
    return capture, points


def load_shared_points(directory, capture):
    shared = capture["shared_load"]
    for key in ("target_rps", "concurrency", "duration_seconds", "warmup_seconds", "request_timeout_seconds"):
        positive_integer(shared[key], f"shared {key}")
    for key in ("host", "versions", "revision", "server_delay_ms", "request_timeout_seconds"):
        if shared[key] != capture[key]:
            raise ValueError(f"shared load {key} differs from the capacity sweep")
    started = instant(shared["started_at"], "shared load start")
    completed = instant(shared["completed_at"], "shared load completion")
    if completed <= started:
        raise ValueError("shared load completion must follow its start")
    return [
        measured_point(directory, mesh, {"name": "shared", "payload_bytes": 1024},
                       shared["concurrency"], shared, started, completed, shared["target_rps"])
        for mesh in MESHES
    ]


def selected_peaks(points):
    peaks = {}
    for mesh in MESHES:
        winners = {}
        for name, _ in WORKLOADS:
            eligible = [p for p in points if p.mesh == mesh and p.workload == name and p.eligible]
            if not eligible:
                raise ValueError(f"{mesh} {name}: no error-free measured point")
            # max retains the first declared point on ties; no winning reruns.
            winners[name] = max(eligible, key=lambda p: p.rps if name == "rps" else p.payload_mib_s)
        peaks[mesh] = winners
    return peaks


def peak_metadata(points):
    return {mesh: {f"{name}_concurrency": point.concurrency for name, point in winners.items()}
            for mesh, winners in selected_peaks(points).items()}


def reports(directory):
    capture, points = load_results(directory)
    if capture["peaks"] != peak_metadata(points):
        raise ValueError("capture peak metadata differs from measured winners")
    shared = load_shared_points(directory, capture)
    target = capture["shared_load"]["target_rps"]
    clients = capture["shared_load"]["concurrency"]
    graph = render(points, shared, capture["host"], target_rps=target)
    conditions = f"All meshes at **{target:,} RPS with {clients} clients**; lines show capacity sweeps."
    workload = "Zero-delay HTTP echo: 1 KiB requests/responses; bulk uses 1 MiB each way."
    host = capture["host"]
    text = "\n".join([
        "# Service mesh benchmarks", "",
        "![Latency, memory, CPU and throughput](comparison.svg)", "",
        conditions, "", workload,
        f"One Linux host: {host['logical_cpus']} logical CPUs, {host['memory_bytes'] / 2**30:.1f} GiB RAM; "
        "client and server on separate kind workers. Interlink/Linkerd use sidecars; Istio ambient uses L4 ztunnel.", "",
        f"{capture['duration_seconds']} s per capacity point after {capture['warmup_seconds']} s warm-up; "
        f"{capture['request_timeout_seconds']} s request timeout. Error-free measurements only.", "",
        "[Run the benchmarks](../README.md)", "",
    ])
    headline = "\n".join([
        HEADLINE_BEGIN,
        "![Service mesh latency, memory, CPU and throughput](bench/results/comparison.svg)", "",
        conditions, "",
        "[Workload and results](bench/results/comparison.md) · [Run manually](bench/README.md)",
        HEADLINE_END,
    ])
    return text, headline, graph


def replace_headline(readme, headline):
    if readme.count(HEADLINE_BEGIN) != 1 or readme.count(HEADLINE_END) != 1:
        raise ValueError("README needs exactly one pair of generated mesh headline markers")
    start = readme.index(HEADLINE_BEGIN)
    end = readme.index(HEADLINE_END)
    if end < start:
        raise ValueError("README generated mesh headline markers are reversed")
    for position, marker in ((start, HEADLINE_BEGIN), (end, HEADLINE_END)):
        suffix = readme[position + len(marker):]
        if ((position and readme[position - 1] != "\n")
                or (suffix and not suffix.startswith(("\n", "\r\n")))):
            raise ValueError("README generated mesh headline markers must occupy their own lines")
    return readme[:start] + headline + readme[end + len(HEADLINE_END):]


def write_reports(output, text, readme, original, updated, graph):
    """Replace the graph, report and README together; roll back on failure."""
    pending = []
    replaced = []
    try:
        graph_path = output.with_suffix(".svg")
        for path, content, previous in (
                (graph_path, graph, graph_path.read_bytes() if graph_path.exists() else None),
                (output, text, output.read_bytes() if output.exists() else None),
                (readme, updated, original)):
            with tempfile.NamedTemporaryFile("wb", dir=path.parent,
                                             prefix=".mesh-report-", delete=False) as temporary:
                temporary.write(content.encode("utf-8"))
                pending.append((path, Path(temporary.name), previous))
            if path.exists():
                os.chmod(temporary.name, path.stat().st_mode & 0o777)
        for path, temporary, previous in pending:
            os.replace(temporary, path)
            replaced.append((path, previous))
    except BaseException:
        for path, previous in reversed(replaced):
            if previous is None:
                path.unlink()
            else:
                path.write_bytes(previous)
        raise
    finally:
        for _, temporary, _ in pending:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, default=RESULTS_DIR)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--readme", type=Path, default=README_PATH)
    parser.add_argument("--check", action="store_true", help="fail if either generated view is stale")
    args = parser.parse_args()
    output = args.output or args.results / "comparison.md"
    try:
        if len({output.resolve(), output.with_suffix(".svg").resolve(), args.readme.resolve()}) != 3:
            raise ValueError("graph, comparison and README must be separate files")
        text, headline, graph = reports(args.results)
        original = args.readme.read_bytes()
        updated = replace_headline(original.decode("utf-8"), headline)
        if args.check:
            if output.read_text(encoding="utf-8") != text:
                raise ValueError("comparison report is stale; regenerate it from its measurements")
            if original != updated.encode("utf-8"):
                raise ValueError("README headline is stale; regenerate it from its capture")
            if output.with_suffix(".svg").read_text(encoding="utf-8") != graph:
                raise ValueError("benchmark graph is stale; regenerate it from its measurements")
        else:
            write_reports(output, text, args.readme, original, updated, graph)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"benchmark report: {error}", file=sys.stderr)
        return 1
    print(f"{'Verified' if args.check else 'Wrote'} comparison: {output}; headline: {args.readme}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
