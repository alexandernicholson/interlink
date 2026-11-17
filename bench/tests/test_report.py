"""Consumer-visible validation for the sole generated mesh comparison."""

import base64
import copy
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPORTER = Path(__file__).resolve().parents[1] / "scripts" / "aggregate.py"
MESHES = ("interlink", "linkerd", "istio-ambient")
EPOCH = datetime(2026, 11, 17, tzinfo=timezone.utc).timestamp()


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.results = Path(self.temporary.name)
        self.output = self.results / "comparison.md"
        self.capture = {
            "schema_version": 1,
            "revision": "a" * 40,
            "run_id": "matched-test-capture",
            "started_at": "2026-11-17T00:00:00Z",
            "completed_at": "2026-11-17T00:01:00Z",
            "duration_seconds": 10,
            "warmup_seconds": 10,
            "payload_bytes": 1024,
            "server_delay_ms": 200,
            "profiles": [{"qps": 100, "connections": 10}],
            "meshes": list(MESHES),
            "versions": {"kind": "v0.24.0", "kubernetes": "v1.30.0",
                         "linkerd": "stable-2.14.10", "istio": "1.24.0", "fortio": "1.66.0"},
        }
        self.record = {
            "RequestedQPS": "100",
            "NumThreads": 10,
            "Payload": base64.b64encode(b"x" * 1024).decode(),
            "StartTime": "2026-11-17T00:00:10Z",
            "ActualDuration": 10_000_000_000,
            "ActualQPS": 99,
            "RetCodes": {"200": 990},
            "Version": "1.66.0",
            "DurationHistogram": {"Count": 990, "Percentiles": [
                {"Percentile": 50, "Value": 0.2}, {"Percentile": 99, "Value": 0.21}]},
        }
        self.write_capture()
        for mesh in MESHES:
            (self.results / mesh).mkdir()
            self.write_record(mesh, self.record)
            self.proxy_path(mesh).write_text(
                "timestamp,cpu_millicores,memory_rss_kb\n"
                + "\n".join(f"{EPOCH + second},{cpu},153600" for second, cpu in [(10, 100), (15, 200), (20, 100)])
                + "\n", encoding="utf-8")
            self.host_path(mesh).write_text(
                "timestamp,host_util_percent\n"
                + "\n".join(f"{EPOCH + second},20" for second in [10, 15, 20]) + "\n", encoding="utf-8")

    def write_capture(self):
        (self.results / "capture.json").write_text(json.dumps(self.capture), encoding="utf-8")

    def fortio_path(self, mesh="interlink"):
        return self.results / mesh / f"fortio-{mesh}-q100-c10.json"

    def proxy_path(self, mesh="interlink"):
        return self.results / mesh / f"metrics-{mesh}-q100-c10.csv"

    def host_path(self, mesh="interlink"):
        return self.proxy_path(mesh).with_suffix(".host.csv")

    def write_record(self, mesh, record):
        self.fortio_path(mesh).write_text(json.dumps(record), encoding="utf-8")

    def run_report(self, check=False):
        command = [sys.executable, str(REPORTER), "--results", str(self.results)]
        if check:
            command.append("--check")
        return subprocess.run(command, capture_output=True, text=True, timeout=10, check=False)

    def test_complete_capture_publishes_all_mesh_values_with_correct_units(self):
        for index, mesh in enumerate(MESHES):
            record = copy.deepcopy(self.record)
            record["DurationHistogram"]["Percentiles"][1]["Value"] = 0.21 + index * 0.01
            self.write_record(mesh, record)
        result = self.run_report()
        self.assertEqual(result.returncode, 0, result.stderr)
        table = [[cell.strip() for cell in line.strip("|").split("|")]
                 for line in self.output.read_text().splitlines() if line.startswith("| ")]
        self.assertEqual(table[2:], [[mesh, "100", "99.0", "200.00", f"{210 + index * 10:.2f}", "133.3", "150.0"]
                                    for index, mesh in enumerate(MESHES)])
        self.assertEqual(self.run_report(check=True).returncode, 0)

    def test_mismatched_or_failed_load_cannot_publish(self):
        invalid = [
            ("RequestedQPS", "101"), ("NumThreads", 11),
            ("Version", "different-fortio-version"),
            ("Payload", base64.b64encode(b"short").decode()),
            ("ActualDuration", 1_000_000_000), ("ActualQPS", 96.9),
            ("ActualQPS", float("nan")),
            ("StartTime", "2026-11-18T00:00:10Z"),
            ("StartTime", "2026-11-17T00:00:10"),
            ("RetCodes", {"200": 989, "503": 1}),
            ("RetCodes", {"200": 989}),
        ]
        for field, value in invalid:
            with self.subTest(field=field, value=value):
                record = copy.deepcopy(self.record)
                record[field] = value
                self.write_record("interlink", record)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertFalse(self.output.exists())
        self.write_record("interlink", self.record)
        self.assertEqual(self.run_report().returncode, 0)

    def test_missing_invalid_or_warmup_only_metrics_cannot_publish(self):
        original_proxy = self.proxy_path().read_text()
        original_host = self.host_path().read_text()
        bad_proxy = [
            "timestamp,cpu_millicores,memory_rss_kb\n",
            f"timestamp,cpu_millicores,memory_rss_kb\n{EPOCH},100,153600\n{EPOCH + 1},100,153600\n",
            f"timestamp,cpu_millicores,memory_rss_kb\n{EPOCH + 10},nan,153600\n{EPOCH + 20},100,153600\n",
            f"timestamp,cpu_millicores,memory_rss_kb\n{EPOCH + 10},100,0\n{EPOCH + 20},100,0\n",
            f"timestamp,cpu_millicores,memory_rss_kb\n{EPOCH + 20},100,153600\n{EPOCH + 10},100,153600\n",
        ]
        for contents in bad_proxy:
            with self.subTest(contents=contents):
                self.proxy_path().write_text(contents)
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertFalse(self.output.exists())
        self.proxy_path().write_text(original_proxy)
        self.host_path().unlink()
        self.assertNotEqual(self.run_report().returncode, 0)
        self.host_path().write_text(original_host.replace(",20", ",85"))
        self.assertNotEqual(self.run_report().returncode, 0)
        self.host_path().write_text(original_host)
        self.assertEqual(self.run_report().returncode, 0)

    def test_incomplete_and_mixed_cohorts_are_rejected(self):
        self.fortio_path("linkerd").unlink()
        self.assertNotEqual(self.run_report().returncode, 0)
        self.write_record("linkerd", self.record)
        extra = self.results / "linkerd" / "fortio-linkerd-q200-c10.json"
        extra.write_text(json.dumps(self.record))
        self.assertNotEqual(self.run_report().returncode, 0)
        extra.unlink()
        (self.results / "interlink-nomux").mkdir()
        self.assertNotEqual(self.run_report().returncode, 0)
        (self.results / "interlink-nomux").rmdir()
        self.assertEqual(self.run_report().returncode, 0)

    def test_bad_provenance_cannot_publish(self):
        valid = copy.deepcopy(self.capture)
        invalid = [
            ("revision", "unknown"), ("meshes", ["interlink"]),
            ("completed_at", "2026-11-16T00:00:00Z"),
            ("profiles", [{"qps": 100, "connections": 10}] * 2),
        ]
        for key, value in invalid:
            with self.subTest(key=key):
                self.capture = copy.deepcopy(valid)
                self.capture[key] = value
                self.write_capture()
                self.assertNotEqual(self.run_report().returncode, 0)
                self.assertFalse(self.output.exists())
        self.capture = valid
        self.write_capture()
        self.assertEqual(self.run_report().returncode, 0)

    def test_failed_refresh_preserves_the_published_complete_table(self):
        self.assertEqual(self.run_report().returncode, 0)
        published = self.output.read_bytes()
        record = copy.deepcopy(self.record)
        record["RetCodes"] = {"200": 989, "503": 1}
        self.write_record("istio-ambient", record)
        self.assertNotEqual(self.run_report().returncode, 0)
        self.assertEqual(self.output.read_bytes(), published)

    def test_check_detects_changed_evidence_without_redating_the_table(self):
        self.assertEqual(self.run_report().returncode, 0)
        published = self.output.read_bytes()
        record = copy.deepcopy(self.record)
        record["DurationHistogram"]["Percentiles"][1]["Value"] = 0.25
        self.write_record("interlink", record)
        self.assertNotEqual(self.run_report(check=True).returncode, 0)
        self.assertEqual(self.output.read_bytes(), published)
        self.assertEqual(self.run_report().returncode, 0)
        self.assertIn("| interlink | 100 | 99.0 | 200.00 | 250.00 |", self.output.read_text())
        self.assertEqual(self.run_report(check=True).returncode, 0)


if __name__ == "__main__":
    unittest.main()
