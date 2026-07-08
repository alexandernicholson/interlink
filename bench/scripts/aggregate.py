#!/usr/bin/env python3
"""Aggregate Fortio JSON and metrics CSVs into a comparison table."""

import csv
import json
import re
import os
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent.parent
RESULTS_DIR = BENCH_DIR / "results"
OUTPUT = RESULTS_DIR / "comparison.md"

MESHES = ["interlink", "interlink-nomux", "linkerd", "istio-ambient"]
PROFILES = [
    ("light", 320, 160),
    ("medium", 3200, 1600),
    ("heavy", 12800, 6400),
]


def parse_fortio(path: Path) -> dict:
    # Robust to non-JSON status lines interleaved into the file (fortio writes a
    # "Successfully wrote..." line to stdout alongside the JSON).
    text = path.read_text()
    dec = json.JSONDecoder()
    i = 0
    d = None
    while True:
        i = text.find("{", i)
        if i < 0:
            break
        try:
            o, e = dec.raw_decode(text, i)
            if isinstance(o, dict) and "DurationHistogram" in o:
                d = o
                break
            i = e
        except json.JSONDecodeError:
            i += 1
    if d is None:
        return {"p50": 0, "p90": 0, "p99": 0, "avg": 0, "actual_qps": 0, "errors": -1}
    hist = d.get("DurationHistogram", {})
    percentiles = {p["Percentile"]: p["Value"] * 1000 for p in hist.get("Percentiles", [])}
    return {
        "p50": percentiles.get(50, 0),
        "p90": percentiles.get(90, 0),
        "p99": percentiles.get(99, 0),
        "avg": hist.get("Avg", 0) * 1000,
        "actual_qps": d.get("ActualQPS", 0),
        "errors": sum(d.get("RetCodes", {}).values()) - d.get("RetCodes", {}).get("200", 0),
    }


def aggregate_csv(path: Path) -> dict:
    cpu_vals = []
    mem_vals = []
    if path.exists():
        with path.open() as f:
            for row in csv.DictReader(f):
                try:
                    cpu_vals.append(float(row.get("cpu_millicores", row.get("cpu_percent", 0))))
                    mem_vals.append(float(row.get("memory_rss_kb", 0)))
                except ValueError:
                    continue
    # Peak host utilization from the sibling .host.csv — the headroom signal.
    peak_host = 0.0
    host_path = path.with_suffix(".host.csv")
    if host_path.exists():
        with host_path.open() as f:
            for row in csv.DictReader(f):
                try:
                    peak_host = max(peak_host, float(row.get("host_util_percent", 0)))
                except ValueError:
                    continue
    out = {"avg_cpu": 0, "peak_cpu": 0, "avg_mem": 0, "peak_mem": 0, "peak_host": peak_host}
    if cpu_vals:
        out.update(
            avg_cpu=sum(cpu_vals) / len(cpu_vals),
            peak_cpu=max(cpu_vals),
            avg_mem=sum(mem_vals) / len(mem_vals),
            peak_mem=max(mem_vals),
        )
    return out


# A profile's numbers are trustworthy only if the load actually ran as
# specified AND the host was not itself the bottleneck. Otherwise latency is
# queuing-dominated and must not be published as a mesh comparison.
HOST_SATURATION_PCT = 85.0
RPS_TOLERANCE = 0.97


def validity(target_rps: int, achieved_rps: float, errors: int, peak_host: float) -> tuple:
    reasons = []
    if errors and errors > 0:
        reasons.append(f"{errors} errors")
    if target_rps and achieved_rps < RPS_TOLERANCE * target_rps:
        reasons.append(f"RPS {achieved_rps:.0f}/{target_rps} ({achieved_rps/target_rps*100:.0f}%)")
    if peak_host >= HOST_SATURATION_PCT:
        reasons.append(f"host {peak_host:.0f}% CPU")
    return (not reasons, "; ".join(reasons))


def main() -> None:
    label_re = re.compile(r"fortio-(?P<mesh>.+)-q(?P<qps>\d+)-c(?P<conns>\d+)\.json$")
    rows = []
    for mesh in MESHES:
        mesh_dir = RESULTS_DIR / mesh
        if not mesh_dir.exists():
            continue
        # Discover profiles from the result files (works with any BENCH_PROFILES).
        found = []
        for fp in sorted(mesh_dir.glob("fortio-*.json")):
            m = label_re.search(fp.name)
            if m:
                found.append((int(m.group("qps")), int(m.group("conns"))))
        for qps, conns in sorted(set(found)):
            profile = {320: "light", 800: "small", 1600: "medium-", 3200: "medium", 12800: "heavy"}.get(qps, f"q{qps}")
            label = f"{mesh}-q{qps}-c{conns}"
            fortio_path = mesh_dir / f"fortio-{label}.json"
            metrics_path = mesh_dir / f"metrics-{label}.csv"
            if not fortio_path.exists():
                continue
            f = parse_fortio(fortio_path)
            m = aggregate_csv(metrics_path)
            ok, why = validity(qps, f["actual_qps"], f["errors"], m.get("peak_host", 0.0))
            rows.append(
                {
                    "mesh": mesh,
                    "profile": profile,
                    "p50": f"{f['p50']:.2f}",
                    "p90": f"{f['p90']:.2f}",
                    "p99": f"{f['p99']:.2f}",
                    "avg_cpu": f"{m.get('avg_cpu', 0):.1f}",
                    "peak_cpu": f"{m.get('peak_cpu', 0):.1f}",
                    "avg_mem": f"{m.get('avg_mem', 0):.1f}",
                    "peak_mem": f"{m.get('peak_mem', 0):.1f}",
                    "qps": f"{f['actual_qps']:.1f}",
                    "errors": str(f["errors"]),
                    "peak_host": f"{m.get('peak_host', 0.0):.0f}",
                    "valid": ok,
                    "why": why,
                }
            )

    def mem_mb(kb: str) -> str:
        try:
            return f"{float(kb) / 1024:.1f}"
        except ValueError:
            return kb

    with OUTPUT.open("w") as f:
        f.write("# Service Mesh Benchmark Comparison\n\n")
        f.write("Generated by `bench/scripts/aggregate.py`. All meshes run on the **same** "
                "3-node kind topology (`bench/manifests/kind-3node.yaml`): load generator "
                "and echo server pinned to separate worker nodes so mesh traffic crosses the "
                "node boundary. Identical workload (Go echo server, 200 ms fixed delay, 1 KB "
                "payload), identical Fortio load, CPU/memory sampled via `kubectl top pod`.\n\n")
        f.write("A profile is **valid** only if the achieved RPS was within 3 % of "
                "target, there were zero errors, and peak host CPU stayed below "
                f"{HOST_SATURATION_PCT:.0f} %. Invalid profiles are host-bound "
                "(the machine, not the mesh, was the bottleneck) and are excluded "
                "from the comparison below — rerun on a less loaded / dedicated host.\n\n")
        f.write("| Mesh | Profile | valid | p50 ms | p90 ms | p99 ms | avg CPU (m) | avg mem MB | QPS | errors | peak host % |\n")
        f.write("|------|---------|:-----:|--------|--------|--------|-------------|------------|-----|--------|-------------|\n")
        for r in rows:
            mark = "✅" if r["valid"] else "❌"
            note = "" if r["valid"] else f" _({r['why']})_"
            f.write(
                f"| {r['mesh']} | {r['profile']} | {mark} | {r['p50']} | {r['p90']} | {r['p99']} | "
                f"{r['avg_cpu']} | {mem_mb(r['avg_mem'])} | {r['qps']} | {r['errors']} | {r['peak_host']}{note} |\n"
            )

        valid_rows = [r for r in rows if r["valid"]]
        f.write("\n## Proxy latency overhead (p99 above the 200 ms workload delay)\n\n")
        f.write("_Valid profiles only._\n\n")
        f.write("| Mesh | light | medium | heavy |\n|------|-------|--------|-------|\n")
        by = {}
        for r in valid_rows:
            try:
                by.setdefault(r["mesh"], {})[r["profile"]] = float(r["p99"]) - 200.0
            except ValueError:
                pass
        for mesh in MESHES:
            if mesh in by:
                b = by[mesh]
                order = sorted(b.keys(), key=lambda k: list(b.keys()).index(k))
                cells = " | ".join(f"{p}: +{b[p]:.1f} ms" for p in b)
                f.write(f"| {mesh} | {cells} |\n")
        f.write("\n")
        invalid = [r for r in rows if not r["valid"]]
        if invalid:
            f.write(f"> {len(invalid)} of {len(rows)} profiles were host-bound and "
                    "excluded. Rerun on a dedicated host for a complete table.\n")

    n_valid = sum(1 for r in rows if r["valid"])
    print(f"Wrote comparison to {OUTPUT} ({n_valid}/{len(rows)} profiles valid)")


if __name__ == "__main__":
    main()
