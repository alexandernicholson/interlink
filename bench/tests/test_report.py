"""Deterministic consumer and publication tests for the sole echo-capacity capture."""

import copy
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

REPORTER = Path(__file__).resolve().parents[1] / "scripts" / "aggregate.py"
sys.path.insert(0, str(REPORTER.parent))
import aggregate as reporter
import run_capture as capture_runner

BEGIN = "<!-- BEGIN GENERATED MESH HEADLINE -->"
END = "<!-- END GENERATED MESH HEADLINE -->"
MESHES = ("interlink", "linkerd", "istio-ambient")
EPOCH = datetime(2026, 11, 17, tzinfo=timezone.utc).timestamp()


def histogram(count, value):
    return dict(Count=count, Min=value if count else 0, Max=value if count else 0,
                Sum=count * value, Avg=value if count else 0, StdDev=0,
                Data=[dict(Start=value, End=value, Percent=100, Count=count)] if count else None)


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.results = self.root / "results"
        self.results.mkdir()
        self.output = self.results / "comparison.md"
        self.readme = self.root / "README.md"
        self.before = "# Project\n\nHand-written introduction.\n\n"
        self.after = "\n\nHand-written conclusion.\n"
        self.readme.write_text(self.before + BEGIN + "\nold headline\n" + END + self.after)
        self.capture = {
            "schema_version": 2, "benchmark": "echo-capacity",
            "revision": "a" * 40, "run_id": "matched-test-capture",
            "started_at": "2026-11-17T00:00:00Z", "completed_at": "2026-11-17T00:01:00Z",
            "duration_seconds": 10, "warmup_seconds": 10, "server_delay_ms": 0,
            "request_timeout_seconds": 5,
            "workloads": [dict(name=name, payload_bytes=size, concurrency=[1, 4])
                          for name, size in reporter.WORKLOADS],
            "meshes": list(MESHES),
            "harness_sha256": {
                name: hashlib.sha256(b"fixture input").hexdigest()
                for name in (*reporter.HARNESS_ROOT_FILES,
                             *(directory + "/fixture" for directory in reporter.HARNESS_INPUT_DIRS))},
            "host": dict(system="Linux", architecture="x86_64", kernel="test-kernel",
                         logical_cpus=8, memory_bytes=16 * 2**30),
            "versions": {"kind": "v0.24.0", "kubernetes": "v1.30.0",
                         "linkerd": "stable-2.14.10", "istio": "1.24.0", "fortio": "1.66.0"},
        }
        self.records = {}
        for index, mesh in enumerate(MESHES):
            (self.results / mesh).mkdir()
            for name, size in reporter.WORKLOADS:
                for concurrency in (1, 4):
                    rps = ((100 if concurrency == 1 else 200) + index * 10 if name == "rps"
                           else (30 if concurrency == 1 else 20) + index * 3)
                    count = rps * 10
                    record = {
                        "RequestedQPS": "max", "RequestedDuration": "10s", "NumThreads": concurrency,
                        "DisableFastClient": name == "bulk",
                        "HTTPReqTimeOut": 5_000_000_000,
                        "PayloadBytes": size, "PayloadSHA256": hashlib.sha256(b"x" * size).hexdigest(),
                        "StartTime": "2026-11-17T00:00:10Z", "ActualDuration": 10_000_000_000,
                        "ActualQPS": rps, "RetCodes": {"200": count}, "WarmupRetCodes": {"200": 100},
                        "Version": "1.66.0", "DurationHistogram": histogram(count, 0.004 + index * 0.001),
                        "ErrorsDurationHistogram": histogram(0, 0),
                        "Sizes": histogram(count, size if name == "bulk" else size + 118),
                        "HeaderSizes": histogram(0, 0) if name == "bulk" else histogram(count, 118),
                    }
                    record["DurationHistogram"]["Percentiles"] = [
                        {"Percentile": 99, "Value": 0.004 + index * 0.001}]
                    self.records[mesh, name, concurrency] = record
                    self.write_record(mesh, record, name, concurrency)
                    self.write_metrics(mesh, name, concurrency)
        self.write_capture()
        self.update_peaks()

    def write_capture(self):
        (self.results / "capture.json").write_text(json.dumps(self.capture), encoding="utf-8")

    def update_peaks(self):
        self.write_capture()
        _, points = reporter.load_results(self.results)
        self.capture["peaks"] = reporter.peak_metadata(points)
        self.write_capture()

    def fortio_path(self, mesh="interlink", name="rps", concurrency=4):
        return self.results / mesh / f"fortio-{mesh}-{name}-c{concurrency}.json"

    def proxy_path(self, mesh="interlink", name="rps", concurrency=4):
        return self.results / mesh / f"metrics-{mesh}-{name}-c{concurrency}.csv"

    def host_path(self, mesh="interlink", name="rps", concurrency=4):
        return self.proxy_path(mesh, name, concurrency).with_suffix(".host.csv")

    def write_record(self, mesh, record, name="rps", concurrency=4):
        self.fortio_path(mesh, name, concurrency).write_text(json.dumps(record), encoding="utf-8")

    def write_metrics(self, mesh, name, concurrency, end=20):
        index = MESHES.index(mesh)
        cpu, memory = 100 + index * 10, (50 + index * 5) * 1024
        self.proxy_path(mesh, name, concurrency).write_text(
            "timestamp,cpu_millicores,memory_rss_kb\n"
            + "\n".join(f"{EPOCH + second},{cpu},{memory}" for second in (10, end)) + "\n")
        self.host_path(mesh, name, concurrency).write_text(
            "timestamp,host_util_percent\n"
            + "\n".join(f"{EPOCH + second},98" for second in (10, end)) + "\n")

    def run_report(self, check=False):
        command = [sys.executable, str(REPORTER), "--results", str(self.results),
                   "--readme", str(self.readme)]
        if check:
            command.append("--check")
        return subprocess.run(command, capture_output=True, text=True, timeout=10, check=False)

    def published_views(self):
        return self.output.read_bytes(), self.readme.read_bytes()

    def test_complete_capture_selects_independent_peaks_and_actual_payload_units(self):
        result = self.run_report()
        self.assertEqual(result.returncode, 0, result.stderr)
        table = [line for line in self.output.read_text().splitlines() if line.startswith("| ")]
        headline = [line for line in self.readme.read_text().splitlines() if line.startswith("| ")]
        self.assertEqual(table[2:], [
            f"| {mesh} | {200 + i * 10:.1f} | {4 + i:.2f} | {30 + i * 3:.2f} | {100 + i * 10:.1f} | {50 + i * 5:.1f} |"
            for i, mesh in enumerate(MESHES)])
        self.assertEqual(headline[2:], [
            f"| {mesh} | {200 + i * 10:.1f} | {30 + i * 3:.2f} | {4 + i:.2f} |"
            for i, mesh in enumerate(MESHES)])
        self.assertEqual(self.capture["peaks"]["interlink"], dict(rps_concurrency=4, bulk_concurrency=1))
        self.assertIn("Largest tested concurrency still wins for rps", self.output.read_text())
        self.assertIn("16.0 GiB RAM", self.output.read_text())
        self.assertIn(self.capture["revision"], self.readme.read_text())
        self.assertIn(self.capture["completed_at"], self.readme.read_text())
        self.assertTrue(self.readme.read_text().startswith(self.before + BEGIN))
        self.assertTrue(self.readme.read_text().endswith(END + self.after))
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_measured_and_warmup_error_boundaries_remain_raw_but_cannot_win(self):
        for mesh in MESHES:
            record = copy.deepcopy(self.records[mesh, "rps", 4])
            count = record["DurationHistogram"]["Count"]
            record["RetCodes"] = {"503": count}
            record["ErrorsDurationHistogram"] = histogram(count, 0.004)
            self.write_record(mesh, record)
            bulk = copy.deepcopy(self.records[mesh, "bulk", 1])
            bulk["WarmupRetCodes"] = {"200": 99, "503": 1}
            self.write_record(mesh, bulk, "bulk", 1)
        self.update_peaks()
        self.assertEqual(self.capture["peaks"]["interlink"], dict(rps_concurrency=1, bulk_concurrency=4))
        result = self.run_report()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("| interlink | 100.0 | 20.00 | 4.00 |", self.readme.read_text())
        self.assertIn("2000/2000 measured errors", self.output.read_text())
        self.assertIn("1 warm-up errors", self.output.read_text())
        self.assertEqual(json.loads(self.fortio_path().read_text())["RetCodes"], {"503": 2000})
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_no_error_free_point_preserves_both_published_views(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        for concurrency in (1, 4):
            record = copy.deepcopy(self.records["interlink", "bulk", concurrency])
            record["WarmupRetCodes"] = {"503": 100}
            self.write_record("interlink", record, "bulk", concurrency)
        result = self.run_report()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no error-free measured point", result.stderr)
        self.assertEqual(self.published_views(), previous)

    def test_inflight_drain_uses_actual_duration_without_shaped_upper_limit(self):
        record = copy.deepcopy(self.records["interlink", "bulk", 1])
        record["ActualDuration"] = 30_000_000_000
        record["ActualQPS"] = 10
        self.write_record("interlink", record, "bulk", 1)
        self.write_metrics("interlink", "bulk", 1, end=40)
        self.update_peaks()
        self.assertEqual(self.capture["peaks"]["interlink"]["bulk_concurrency"], 4)
        result = self.run_report()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("| interlink | 200.0 | 20.00 | 4.00 |", self.readme.read_text())

    def test_documented_unlimited_qps_representations_are_accepted(self):
        for value in ("max", "0", 0):
            record = copy.deepcopy(self.records["interlink", "rps", 4])
            record["RequestedQPS"] = value
            self.write_record("interlink", record)
            result = self.run_report()
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_peak_tie_uses_first_predeclared_concurrency(self):
        record = copy.deepcopy(self.records["interlink", "rps", 4])
        record["NumThreads"] = 1
        self.write_record("interlink", record, "rps", 1)
        self.update_peaks()
        self.assertEqual(self.capture["peaks"]["interlink"]["rps_concurrency"], 1)
        result = self.run_report()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("winning concurrency RPS 1, bulk 1", self.output.read_text())

    def test_invalid_data_never_overwrites_either_view(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        invalid = [
            ("RequestedQPS", "1600"), ("RequestedQPS", False), ("RequestedDuration", "11s"),
            ("RequestedDuration", "bad"), ("NumThreads", 16), ("Version", "wrong"),
            ("HTTPReqTimeOut", 120_000_000_000),
            ("PayloadBytes", 1023), ("PayloadSHA256", "bad"), ("Payload", "eA=="),
            ("ActualDuration", 1_000_000_000), ("ActualQPS", float("nan")), ("ActualQPS", 199),
            ("StartTime", "2026-11-18T00:00:10Z"), ("StartTime", "2026-11-17T00:00:10"),
            ("RetCodes", {"200": -1}), ("WarmupRetCodes", {}),
        ]
        original = self.records["interlink", "rps", 4]
        for field, value in invalid:
            with self.subTest(field=field, value=value):
                record = copy.deepcopy(original)
                record[field] = value
                self.write_record("interlink", record)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)
        for name, field, value in [
                ("Sizes", "Count", 1999), ("Sizes", "Sum", 1), ("HeaderSizes", "Sum", 1),
                ("Sizes", "Avg", float("nan")), ("Sizes", "Avg", 1), ("Sizes", "Max", 1),
                ("Sizes", "Data", [dict(Start=float("nan"), End=1, Percent=100, Count=2000)]),
                ("ErrorsDurationHistogram", "Count", 1),
                ("DurationHistogram", "Percentiles", [{"Percentile": 50, "Value": 0.01}]),
                ("DurationHistogram", "Percentiles", [{"Percentile": 101, "Value": 0.01}]),
                ("DurationHistogram", "Percentiles", [{"Percentile": 99, "Value": float("nan")}])]:
            with self.subTest(name=name, field=field):
                record = copy.deepcopy(original)
                record[name][field] = value
                self.write_record("interlink", record)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)
        self.write_record("interlink", original)
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_client_modes_and_bulk_body_accounting_are_required_not_fabricated(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        original = self.records["interlink", "bulk", 1]
        invalid = []
        for field, value in [
                ("DisableFastClient", False), ("DisableFastClient", 1),
                ("HeaderSizes", histogram(300, 118)),
                ("HeaderSizes", dict(Count=0, Min=1, Max=1, Sum=0, Avg=0, StdDev=0)),
                ("HeaderSizes", histogram(299, 0)),
                ("HeaderSizes", dict(Count=300, Min=0, Max=0, Sum=0, Avg=0, StdDev=0,
                                     Data=[dict(Start=1, End=1, Percent=100, Count=300)])),
                ("Sizes", histogram(300, 1048576 + 118)),
                ("Sizes", histogram(300, 1048575))]:
            record = copy.deepcopy(original)
            record[field] = value
            invalid.append(record)
        missing = copy.deepcopy(original)
        del missing["HeaderSizes"]
        invalid.append(missing)
        for record in invalid:
            with self.subTest(record=record):
                self.write_record("interlink", record, "bulk", 1)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)
        self.write_record("interlink", original, "bulk", 1)
        for standard, header_bytes in ((True, 118), (False, 0)):
            fast = copy.deepcopy(self.records["interlink", "rps", 4])
            fast["DisableFastClient"] = standard
            fast["HeaderSizes"] = histogram(2000, header_bytes)
            fast["Sizes"] = histogram(2000, 1024 + header_bytes)
            self.write_record("interlink", fast)
            self.assertNotEqual(self.run_report().returncode, 0)
            self.assertEqual(self.published_views(), previous)
        self.write_record("interlink", self.records["interlink", "rps", 4])
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_standard_zero_header_observations_are_valid_with_either_documented_count(self):
        for count in (0, 300):
            with self.subTest(count=count):
                bulk = copy.deepcopy(self.records["interlink", "bulk", 1])
                bulk["HeaderSizes"] = histogram(count, 0)
                self.write_record("interlink", bulk, "bulk", 1)
                result = self.run_report()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("| interlink | 200.0 | 30.00 | 4.00 |", self.readme.read_text())
                self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_standard_failed_read_sizes_are_retained_but_ineligible_not_zeroed(self):
        bulk = copy.deepcopy(self.records["interlink", "bulk", 1])
        bulk["RetCodes"] = {"503": 300}
        bulk["ErrorsDurationHistogram"] = histogram(300, 0.004)
        bulk["Sizes"] = histogram(300, -1)
        bulk["HeaderSizes"] = histogram(300, 0)
        self.write_record("interlink", bulk, "bulk", 1)
        self.update_peaks()
        self.assertEqual(self.capture["peaks"]["interlink"]["bulk_concurrency"], 4)
        result = self.run_report()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("| interlink | 200.0 | 20.00 | 4.00 |", self.readme.read_text())
        self.assertIn("bulk c1: 300/300 measured errors", self.output.read_text())
        self.assertEqual(json.loads(self.fortio_path("interlink", "bulk", 1).read_text())["Sizes"]["Sum"], -300)
        previous = self.published_views()
        bulk["Sizes"] = histogram(300, -2)
        self.write_record("interlink", bulk, "bulk", 1)
        self.assertNotEqual(self.run_report().returncode, 0)
        self.assertEqual(self.published_views(), previous)

    def test_fortio_success_statuses_with_incomplete_echoes_remain_ineligible(self):
        for code in ("204", "418"):
            with self.subTest(code=code):
                bulk = copy.deepcopy(self.records["interlink", "bulk", 1])
                bulk["RetCodes"] = {code: 300}
                bulk["ErrorsDurationHistogram"] = histogram(0, 0)
                bulk["Sizes"] = histogram(300, 1024)
                self.write_record("interlink", bulk, "bulk", 1)
                self.update_peaks()
                self.assertEqual(self.capture["peaks"]["interlink"]["bulk_concurrency"], 4)
                result = self.run_report()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("| interlink | 200.0 | 20.00 | 4.00 |", self.readme.read_text())
                self.assertIn("bulk c1: 300/300 measured errors", self.output.read_text())
                previous = self.published_views()
                bulk["ErrorsDurationHistogram"] = histogram(300, 0.004)
                self.write_record("interlink", bulk, "bulk", 1)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)

    def test_bad_provenance_and_wrong_peak_metadata_cannot_publish(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        original = copy.deepcopy(self.capture)
        for field, value in [
                ("schema_version", 1), ("benchmark", "shaped"), ("revision", "unknown"),
                ("meshes", ["interlink"]), ("server_delay_ms", 200),
                ("request_timeout_seconds", 0),
                ("completed_at", "2026-11-16T00:00:00Z"), ("profiles", []),
                ("peaks", {mesh: dict(rps_concurrency=1, bulk_concurrency=4) for mesh in MESHES}),
                ("workloads", [dict(name="rps", payload_bytes=1024, concurrency=[1, 1])])]:
            with self.subTest(field=field):
                self.capture = copy.deepcopy(original)
                self.capture[field] = value
                self.write_capture()
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)
        self.capture = original
        self.write_capture()
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_harness_hash_provenance_rejects_missing_unsafe_or_invalid_maps_without_publication(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        original = copy.deepcopy(self.capture["harness_sha256"])
        invalid = [None, {}]
        for key, digest in [
                ("Cargo.lock", "invalid"), ("/private/absolute", "a" * 64),
                ("bench/scripts/../secret", "a" * 64), ("bench/scripts//capture.py", "a" * 64),
                ("bench/scripts/input.py", 1)]:
            invalid.append({**original, key: digest})
        invalid.append({key: digest for key, digest in original.items()
                        if not key.startswith("bench/manifests/")})
        for hashes in invalid:
            with self.subTest(hashes=hashes):
                self.capture["harness_sha256"] = hashes
                self.write_capture()
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)
        self.capture["harness_sha256"] = original
        self.write_capture()
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_harness_hashes_record_actual_relative_inputs_without_docs_results_or_private_paths(self):
        root = self.root / "input-tree"
        expected = set(reporter.HARNESS_ROOT_FILES)
        for relative in reporter.HARNESS_ROOT_FILES:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"abc")
        for directory, name in zip(reporter.HARNESS_INPUT_DIRS, ("capture.py", "echo.go", "mesh.yaml")):
            path = root / directory / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"abc")
            expected.add(path.relative_to(root).as_posix())
        (root / "README.md").write_text("later documentation")
        (root / "bench/results").mkdir()
        (root / "bench/results/private.key").write_text("not an input")
        hashes = capture_runner.harness_hashes(root)
        self.assertEqual(set(hashes), expected)
        self.assertEqual(set(hashes.values()), {
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
        (root / "README.md").write_text("documentation advanced after capture")
        self.assertEqual(capture_runner.harness_hashes(root), hashes)
        (root / "bench/workloads/echo.go").write_bytes(b"changed workload")
        changed = capture_runner.harness_hashes(root)
        self.assertNotEqual(changed["bench/workloads/echo.go"], hashes["bench/workloads/echo.go"])
        self.assertEqual(changed["Cargo.lock"], hashes["Cargo.lock"])
        (root / "Cargo.lock").unlink()
        with self.assertRaisesRegex(ValueError, "not a regular file"):
            capture_runner.harness_hashes(root)
        (root / "Cargo.lock").write_bytes(b"abc")
        (root / "bench/manifests/mesh.yaml").unlink()
        (root / "bench/manifests").rmdir()
        with self.assertRaisesRegex(ValueError, "missing capture input directory"):
            capture_runner.harness_hashes(root)
        (root / "bench/manifests").mkdir()
        (root / "bench/manifests/mesh.yaml").write_bytes(b"abc")
        (root / "bench/workloads/echo.go").unlink()
        (root / "bench/workloads/echo.go").symlink_to(root / "README.md")
        with self.assertRaisesRegex(ValueError, "not a regular file"):
            capture_runner.harness_hashes(root)

    def test_incomplete_grid_and_stale_evidence_cannot_publish(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        self.fortio_path().unlink()
        self.assertNotEqual(self.run_report().returncode, 0)
        self.assertEqual(self.published_views(), previous)
        self.write_record("interlink", self.records["interlink", "rps", 4])
        extra = self.results / "interlink" / "metrics-interlink-rps-c99.csv"
        extra.write_text("stale")
        self.assertNotEqual(self.run_report().returncode, 0)
        self.assertEqual(self.published_views(), previous)
        extra.unlink()
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_missing_zero_stale_or_nonfinite_metrics_cannot_publish(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        original = self.proxy_path().read_text()
        for bad in [
                "timestamp,cpu_millicores,memory_rss_kb\n",
                original.replace("100,51200", "100,0"),
                original.replace("100,51200", "nan,51200"),
                original.replace(str(EPOCH + 20), str(EPOCH + 11))]:
            with self.subTest(bad=bad):
                self.proxy_path().write_text(bad)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertEqual(self.published_views(), previous)
        self.proxy_path().write_text(original)
        self.host_path().unlink()
        self.assertNotEqual(self.run_report().returncode, 0)
        self.assertEqual(self.published_views(), previous)
        self.write_metrics("interlink", "rps", 4)
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_check_detects_raw_changes_without_modifying_or_redating_views(self):
        self.assertEqual(self.run_report().returncode, 0)
        previous = self.published_views()
        record = copy.deepcopy(self.records["interlink", "rps", 4])
        record["DurationHistogram"]["Percentiles"][0]["Value"] = 0.025
        self.write_record("interlink", record)
        self.assertNotEqual(self.run_report(check=True).returncode, 0)
        self.assertEqual(self.published_views(), previous)
        self.assertEqual(self.run_report().returncode, 0)
        self.assertIn("| interlink | 200.0 | 30.00 | 25.00 |", self.readme.read_text())
        self.assertIn(self.capture["completed_at"], self.readme.read_text())
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_check_rejects_stale_headline_without_touching_either_view(self):
        self.assertEqual(self.run_report().returncode, 0)
        published = self.output.read_bytes()
        stale = self.readme.read_text().replace("| 4.00 |", "| 999.00 |")
        self.readme.write_text(stale)
        result = self.run_report(check=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("README headline is stale", result.stderr)
        self.assertEqual(self.output.read_bytes(), published)
        self.assertEqual(self.readme.read_text(), stale)
        self.assertEqual(self.run_report().returncode, 0)
        self.assertIn("| interlink | 200.0 | 30.00 | 4.00 |", self.readme.read_text())
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_broken_or_absent_markers_reject_both_generation_and_check(self):
        self.assertEqual(self.run_report().returncode, 0)
        published = self.output.read_bytes()
        broken = [
            "No markers\n", BEGIN + "\n", END + "\n",
            END + "\n" + BEGIN + "\n",
            BEGIN + "\n" + BEGIN + "\n" + END + "\n",
            BEGIN + "\n" + END + "\n" + END + "\n",
            "prefix " + BEGIN + "\n" + END + "\n",
            BEGIN + "\n" + END + " suffix\n",
        ]
        for text in broken:
            for check in (False, True):
                with self.subTest(text=text, check=check):
                    self.readme.write_text(text)
                    result = self.run_report(check=check)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("markers", result.stderr)
                    self.assertEqual(self.output.read_bytes(), published)
                    self.assertEqual(self.readme.read_text(), text)
        self.readme.unlink()
        self.assertNotEqual(self.run_report().returncode, 0)
        self.assertEqual(self.output.read_bytes(), published)
        self.assertFalse(self.readme.exists())

    def test_headline_preserves_readme_bytes_outside_markers(self):
        prefix = b"# Project\r\n\r\nHand-written \xc3\xa9.\r\n\r\n"
        suffix = b"\n\nUnchanged ending.\r\n"
        self.readme.write_bytes(prefix + BEGIN.encode() + b"\r\nold\r\n" + END.encode() + suffix)
        self.assertEqual(self.run_report().returncode, 0)
        self.assertTrue(self.readme.read_bytes().startswith(prefix + BEGIN.encode()))
        self.assertTrue(self.readme.read_bytes().endswith(END.encode() + suffix))
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_comparison_and_readme_must_be_distinct_files(self):
        prior = self.readme.read_bytes()
        result = subprocess.run(
            [sys.executable, str(REPORTER), "--results", str(self.results),
             "--readme", str(self.readme), "--output", str(self.readme)],
            capture_output=True, text=True, timeout=10, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("separate files", result.stderr)
        self.assertEqual(self.readme.read_bytes(), prior)

    def test_regeneration_write_failure_restores_both_published_views(self):
        self.assertEqual(self.run_report().returncode, 0)
        published = self.output.read_bytes()
        original = self.readme.read_bytes()
        replace = os.replace

        def fail_headline(source, destination):
            if destination == self.readme:
                raise OSError("injected headline replacement failure")
            return replace(source, destination)

        with mock.patch.object(reporter.os, "replace", side_effect=fail_headline):
            with self.assertRaisesRegex(OSError, "injected"):
                reporter.write_reports(self.output, "new comparison", self.readme,
                                       original, "new headline")
        self.assertEqual(self.output.read_bytes(), published)
        self.assertEqual(self.readme.read_bytes(), original)
        self.assertEqual(list(self.root.glob(".mesh-report-*")), [])
        self.assertEqual(list(self.results.glob(".mesh-report-*")), [])

    def test_first_generation_write_failure_leaves_no_partial_comparison(self):
        original = self.readme.read_bytes()
        replace = os.replace

        def fail_headline(source, destination):
            if destination == self.readme:
                raise OSError("injected headline replacement failure")
            return replace(source, destination)

        with mock.patch.object(reporter.os, "replace", side_effect=fail_headline):
            with self.assertRaisesRegex(OSError, "injected"):
                reporter.write_reports(self.output, "comparison", self.readme, original, "headline")
        self.assertFalse(self.output.exists())
        self.assertEqual(self.readme.read_bytes(), original)

    def live_capture(self, source_changed=False, publication_failure=False, invalid=False):
        self.assertEqual(self.run_report().returncode, 0)
        repository = self.root / "repository"
        bench = repository / "bench"
        source_root = REPORTER.parents[2]
        for relative in reporter.HARNESS_ROOT_FILES:
            destination = repository / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_root / relative, destination)
        for relative in reporter.HARNESS_INPUT_DIRS:
            shutil.copytree(source_root / relative, repository / relative)
        readme = repository / "README.md"
        shutil.copy2(self.readme, readme)
        published = bench / "results"
        shutil.copytree(self.results, published)
        previous = {p.relative_to(published): p.read_bytes() for p in published.rglob("*") if p.is_file()}
        old_readme = readme.read_bytes()
        record = copy.deepcopy(self.records["interlink", "rps", 4])
        record["DurationHistogram"]["Percentiles"][0]["Value"] = 0.025
        if invalid:
            record["ActualQPS"] = float("nan")
        self.write_record("interlink", record)
        run = subprocess.run
        rename = Path.rename
        source_identity = capture_runner.source_identity
        staged_readmes = []

        def run_script(script, env, log, timeout):
            if script == "setup.sh":
                shutil.copytree(self.results, Path(env["BENCH_CAPTURE_DIR"]), dirs_exist_ok=True)

        def command(args, **kwargs):
            if args[0] == "kind":
                return subprocess.CompletedProcess(args, 0)
            self.assertEqual(readme.read_bytes(), old_readme)
            staged_readme = Path(args[args.index("--readme") + 1])
            self.assertNotEqual(staged_readme, readme)
            try:
                return run(args, **kwargs)
            finally:
                self.assertEqual(readme.read_bytes(), old_readme)
                staged_readmes.append(staged_readme.read_text())
                if source_changed:
                    (repository / "source.rs").write_text("source advanced during validation")

        def fail_publication(path, destination):
            if publication_failure and path.name == "README.md" and path != readme:
                raise OSError("injected live headline replacement failure")
            return rename(path, destination)

        environment = {
            "BENCH_REVISION": self.capture["revision"],
            "BENCH_RPS_CONCURRENCY": "1,4", "BENCH_BULK_CONCURRENCY": "1,4",
            "BENCH_DURATION": "10",
            **{key.upper() + "_VERSION": value for key, value in self.capture["versions"].items()
               if key != "kubernetes"},
            "KUBECTL_VERSION": self.capture["versions"]["kubernetes"],
        }
        old_umask = os.umask(0o022)
        self.addCleanup(os.umask, old_umask)
        with mock.patch.multiple(capture_runner, ROOT=repository, BENCH=bench), \
                mock.patch.object(capture_runner, "source_identity",
                                  side_effect=lambda: source_identity(repository)), \
                mock.patch.object(capture_runner, "timestamp",
                                  side_effect=[self.capture["started_at"], self.capture["completed_at"]]), \
                mock.patch.object(capture_runner, "run_script", side_effect=run_script), \
                mock.patch.object(capture_runner, "cleanup"), \
                mock.patch.object(capture_runner.subprocess, "run", side_effect=command), \
                mock.patch.object(capture_runner.os, "uname",
                                  return_value=os.uname_result(("Linux", "test", "test-kernel", "test", "x86_64"))), \
                mock.patch.object(capture_runner.os, "sysconf", return_value=4096), \
                mock.patch.object(Path, "rename", fail_publication), \
                mock.patch.dict(os.environ, environment):
            if source_changed:
                with self.assertRaisesRegex(RuntimeError, "archive source changed"):
                    capture_runner.capture()
            elif publication_failure:
                with self.assertRaisesRegex(OSError, "injected live headline"):
                    capture_runner.capture()
            elif invalid:
                with self.assertRaisesRegex(ValueError, "achieved RPS"):
                    capture_runner.capture()
            else:
                capture_runner.capture()
        if source_changed or publication_failure or invalid:
            self.assertEqual(readme.read_bytes(), old_readme)
            self.assertEqual(
                {p.relative_to(published): p.read_bytes() for p in published.rglob("*") if p.is_file()},
                previous)
        else:
            self.assertIn("| interlink | 200.0 | 30.00 | 25.00 |", readme.read_text())
            self.assertIn("| interlink | 200.0 | 25.00 | 30.00 |",
                          (published / "comparison.md").read_text())
            self.assertEqual(
                subprocess.run([sys.executable, str(REPORTER), "--results", str(published),
                                "--readme", str(readme), "--check"],
                               capture_output=True, text=True, timeout=10).returncode, 0)
        self.assertEqual(bool(staged_readmes), not invalid)

    def test_live_capture_publishes_same_capture_without_dirtying_source_before_guard(self):
        self.live_capture()

    def test_live_capture_source_change_preserves_dataset_and_headline(self):
        self.live_capture(source_changed=True)

    def test_live_capture_invalid_evidence_preserves_dataset_and_headline(self):
        self.live_capture(invalid=True)

    def test_live_capture_headline_publication_failure_restores_dataset_and_headline(self):
        self.live_capture(publication_failure=True)


if __name__ == "__main__":
    unittest.main()
