"""Deterministic producer tests; no Docker, Kubernetes, downloads or workloads.

Run: python3 -B -m unittest discover -s bench/tests -p 'test_run.py'
"""
import base64
import csv
import datetime as dt
import hashlib
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'))
import capture_profile as profile
import run_capture as capture


class Deadline:
    def __enter__(self):
        self.previous = signal.signal(signal.SIGALRM, self.expired)
        signal.setitimer(signal.ITIMER_REAL, 5)

    def expired(self, signum, frame):
        raise TimeoutError('test coordination exceeded five seconds')

    def __exit__(self, *args):
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, self.previous)


class HarnessTests(unittest.TestCase):
    def test_predeclared_workloads_preserve_full_grids_and_reject_invalid_concurrency(self):
        self.assertEqual(profile.workload_grids('1,4,16,64,256,1024,4096', '1,4,16,64,256,1024'), [
            {'name': 'rps', 'payload_bytes': 1024, 'concurrency': [1, 4, 16, 64, 256, 1024, 4096]},
            {'name': 'bulk', 'payload_bytes': 1_048_576, 'concurrency': [1, 4, 16, 64, 256, 1024]}])
        for value in ('0', '-1', '', '1,1', 'nan', '1:4', '1,,4'):
            with self.subTest(value=value), self.assertRaises(ValueError):
                profile.concurrency_grid(value)

    def load_record(self, payload_bytes=1024, connections=80, duration=1, codes=None, requested_qps=0):
        codes = {'200': requested_qps * duration if requested_qps else 10} if codes is None else codes
        count = sum(codes.values())
        standard_client = payload_bytes == 1_048_576
        header_bytes = 0 if standard_client else 120
        return {'RetCodes': codes, 'RequestedQPS': str(requested_qps) if requested_qps else 'max',
                'NumThreads': connections,
                'ActualDuration': duration * 1_000_000_000, 'ActualQPS': count / duration,
                'HTTPReqTimeOut': 5_000_000_000,
                'StartTime': dt.datetime.now(dt.timezone.utc).isoformat(), 'Version': '1.66.0',
                'Payload': base64.b64encode(b'x' * payload_bytes).decode(), 'DisableFastClient': standard_client,
                'DurationHistogram': {'Count': count, 'Percentiles': [{'Percentile': 99, 'Value': 0.001}]},
                'Sizes': {'Count': count, 'Min': payload_bytes + header_bytes, 'Max': payload_bytes + header_bytes,
                          'Sum': count * (payload_bytes + header_bytes)},
                'HeaderSizes': {'Count': 0 if standard_client else count, 'Min': header_bytes, 'Max': header_bytes,
                                'Sum': count * header_bytes}}

    def test_load_keeps_error_boundaries_and_compacts_actual_payload_without_changing_stats(self):
        for codes in ({'200': 10}, {'200': 8, '503': 2}, {'-1': 10}):
            data = self.load_record(payload_bytes=1_048_576, codes=codes)
            result = profile.validate_load(json.dumps(data), 1_048_576, 80, 1)
            self.assertNotIn('Payload', result)
            self.assertEqual(result['PayloadBytes'], 1_048_576)
            self.assertEqual(result['PayloadSHA256'], hashlib.sha256(b'x' * 1_048_576).hexdigest())
            self.assertEqual({key: value for key, value in result.items()
                              if key not in ('PayloadBytes', 'PayloadSHA256')},
                             {key: value for key, value in data.items() if key != 'Payload'})
            self.assertEqual(profile.request_errors(result['RetCodes']), sum(
                value for key, value in codes.items() if key != '200'))
        # Legitimate in-flight drains use their actual elapsed time and RPS.
        data = self.load_record(duration=3)
        self.assertEqual(profile.validate_load(json.dumps(data), 1024, 80, 1)['ActualQPS'], 10 / 3)

    def test_load_rejects_malformed_measurements_and_payload_or_response_size_corruption(self):
        for mutate in (
            lambda d: d.update(RetCodes={}),
            lambda d: d.update(RetCodes={'200': 0}),
            lambda d: d.update(RetCodes={'200': True}),
            lambda d: d.update(RetCodes={'200': -1}),
            lambda d: d.update(RetCodes={'200': float('nan')}),
            lambda d: d.update(RequestedQPS='1600'),
            lambda d: d.update(NumThreads=4),
            lambda d: d.update(DisableFastClient=True),
            lambda d: d.update(HTTPReqTimeOut=120_000_000_000),
            lambda d: d.update(ActualDuration=float('inf')),
            lambda d: d.update(ActualDuration=100_000_000),
            lambda d: d.update(ActualQPS=99),
            lambda d: d.update(StartTime='2026-11-17T12:00:00'),
            lambda d: d.update(Payload='not valid base64'),
            lambda d: d.update(Payload=base64.b64encode(b'wrong length').decode()),
            lambda d: d['DurationHistogram'].update(Count=9),
            lambda d: d['Sizes'].update(Count=9),
            lambda d: d['HeaderSizes'].update(Sum=float('nan')),
            lambda d: d['Sizes'].update(Sum=10 * 1024),
            lambda d: d['Sizes'].update(Sum=d['Sizes']['Sum'] + 1),
        ):
            data = self.load_record()
            mutate(data)
            with self.subTest(data=data), self.assertRaises((ValueError, KeyError)):
                profile.validate_load(json.dumps(data), 1024, 80, 1)
        with self.assertRaises(ValueError):
            profile.validate_load('not json', 1024, 80, 1)

    def test_bulk_standard_client_preserves_body_only_sizes_and_real_failure_sentinels(self):
        for header_count in (0, 10):
            data = self.load_record(payload_bytes=1_048_576, duration=2)
            data['HeaderSizes']['Count'] = header_count
            result = profile.validate_load(json.dumps(data), 1_048_576, 80, 2)
            self.assertIs(result['DisableFastClient'], True)
            self.assertEqual(result['Sizes']['Sum'], 10 * 1_048_576)
            self.assertEqual(result['HeaderSizes'], {'Count': header_count, 'Min': 0, 'Max': 0, 'Sum': 0})
            self.assertEqual(result['ActualQPS'], 5)
        data = self.load_record(payload_bytes=1_048_576, codes={'-1': 10})
        data['Sizes'].update(Min=-1, Max=-1, Sum=-10)
        self.assertEqual(profile.validate_load(json.dumps(data), 1_048_576, 80, 1)['Sizes']['Sum'], -10)
        for mutate in (
            lambda d: d.update(DisableFastClient=False),
            lambda d: d['HeaderSizes'].update(Count=9),
            lambda d: d['HeaderSizes'].update(Sum=120),
            lambda d: d['Sizes'].update(Sum=d['Sizes']['Sum'] + 1200),
        ):
            data = self.load_record(payload_bytes=1_048_576)
            mutate(data)
            with self.assertRaises(ValueError):
                profile.validate_load(json.dumps(data), 1_048_576, 80, 1)

    def test_predeclared_workloads_choose_protocol_client_before_capture(self):
        for payload_bytes, standard_client in ((1024, False), (1_048_576, True)):
            command = profile.fortio_args('http://test/echo', payload_bytes, 4, 20)
            self.assertEqual('-stdclient' in command, standard_client)
            self.assertEqual(command[command.index('-qps') + 1], '0')

    def test_shared_load_uses_actual_target_and_preserves_measured_rate_duration_and_body_proof(self):
        command = profile.fortio_args('http://test/echo', 1024, 64, 20, requested_qps=10000)
        command = command[command.index('--') + 1:]
        self.assertEqual(command[command.index('-qps') + 1], '10000')
        self.assertEqual(command[command.index('-c') + 1], '64')
        self.assertEqual(command[command.index('-t') + 1], '20s')
        self.assertNotIn('-stdclient', command)
        for achieved in (9700, 10000, 10300):
            data = self.load_record(connections=64, duration=2, requested_qps=10000,
                                    codes={'200': achieved * 2})
            result = profile.validate_load(json.dumps(data), 1024, 64, 2, requested_qps=10000)
            self.assertEqual(result['ActualQPS'], achieved)
            self.assertEqual(result['ActualDuration'], 2_000_000_000)
            self.assertEqual(result['RetCodes'], {'200': achieved * 2})
            self.assertEqual(result['PayloadBytes'], 1024)
            self.assertEqual(result['PayloadSHA256'], hashlib.sha256(b'x' * 1024).hexdigest())
            self.assertEqual(result['Sizes'], data['Sizes'])
            self.assertNotIn('Payload', result)

    def test_shared_load_rejects_wrong_target_drift_errors_and_payload_corruption(self):
        for codes in ({'200': 10000}, {'200': 9699}, {'200': 10301}, {'200': 9999, '503': 1}, {'-1': 10000}):
            data = self.load_record(connections=64, requested_qps=10000, codes=codes)
            if codes == {'200': 10000}:
                data['RequestedQPS'] = 'max'
            with self.subTest(codes=codes), self.assertRaises(ValueError):
                profile.validate_load(json.dumps(data), 1024, 64, 1, requested_qps=10000)
        for mutate in (
            lambda d: d.update(RequestedQPS='9999'),
            lambda d: d.update(NumThreads=80),
            lambda d: d.update(Payload=base64.b64encode(b'wrong body').decode()),
            lambda d: d['Sizes'].update(Sum=d['Sizes']['Sum'] - 1),
        ):
            data = self.load_record(connections=64, requested_qps=10000)
            mutate(data)
            with self.subTest(data=data), self.assertRaises(ValueError):
                profile.validate_load(json.dumps(data), 1024, 64, 1, requested_qps=10000)
        for invalid in (-1, True, 10000.0, '10000'):
            with self.subTest(invalid=invalid):
                with self.assertRaises(ValueError):
                    profile.fortio_args('http://test/echo', 1024, 64, 20, requested_qps=invalid)
                with self.assertRaises(ValueError):
                    profile.validate_load('{}', 1024, 64, 20, requested_qps=invalid)

    def test_host_delta_does_not_fabricate_zero_or_double_count_guest(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = pathlib.Path(temporary) / 'stat'
            source.write_text('cpu 10 20 30 40 50 60 70 80 90 100\n')
            self.assertEqual(profile.host_ticks(source), (360, 90))
        self.assertEqual(profile.host_util((100, 20), (200, 60)), 60)
        for current in ((100, 20), (90, 20), (200, 10), (200, 121)):
            with self.assertRaises(ValueError):
                profile.host_util((100, 20), current)

    def metric(self, uid='uid', cpu=2_000_000):
        stamp = dt.datetime.fromtimestamp(100, dt.timezone.utc).isoformat()
        # CRI can supply a working set but omit RSS. It is not an RSS substitute.
        return {'pods': [{'podRef': {'uid': uid, 'namespace': 'bench'}, 'containers': [
            {'name': 'proxy', 'cpu': {'time': stamp, 'usageNanoCores': cpu},
             'memory': {'time': stamp, 'workingSetBytes': 999_999}}]}]}

    def rss(self, uid='uid', container_id='a' * 64, value=4096, stamp=100_000, systemd=True):
        if systemd:
            path = (f'/kubepods.slice/kubepods-burstable.slice/'
                    f'kubepods-burstable-pod{uid.replace("-", "_")}.slice/'
                    f'cri-containerd-{container_id}.scope')
        else:
            path = f'/kubepods/burstable/pod{uid}/{container_id}'
        # Raw cgroups need not have Kubernetes/runtime metadata labels.
        return f'container_memory_rss{{container="",id={json.dumps(path)},namespace=""}} {value} {stamp}\n'

    def test_actual_rss_without_summary_rss_requires_every_expected_container(self):
        data = self.metric()
        expected = {'uid': ('worker', 'a' * 64)}

        def run(args):
            if args[-1].endswith('/stats/summary'):
                return json.dumps(data)
            self.assertEqual(args[-1], '/api/v1/nodes/worker/proxy/metrics/cadvisor')
            return self.rss()

        self.assertEqual(profile.proxy_metrics('bench', 'proxy', expected, run, now=101), (2, 4))
        # An optional summary RSS field must not change the authoritative value.
        data['pods'][0]['containers'][0]['memory']['rssBytes'] = 888_888
        self.assertEqual(profile.proxy_metrics('bench', 'proxy', expected, run, now=101), (2, 4))
        for mutate in (
            lambda d: d['pods'].clear(),
            lambda d: d['pods'][0]['containers'].clear(),
            lambda d: d['pods'][0]['containers'].append(d['pods'][0]['containers'][0]),
            lambda d: d['pods'].append(d['pods'][0]),
            lambda d: d['pods'][0]['podRef'].update(namespace='other'),
            lambda d: d['pods'][0]['containers'][0]['cpu'].pop('usageNanoCores'),
            lambda d: d['pods'][0]['containers'][0]['cpu'].update(usageNanoCores=float('nan')),
        ):
            data = self.metric()
            mutate(data)
            with self.assertRaises((ValueError, KeyError)):
                profile.proxy_metrics('bench', 'proxy', expected, run, now=101)
        data = self.metric()
        with self.assertRaisesRegex(ValueError, 'stale'):
            profile.proxy_metrics('bench', 'proxy', expected, run, now=131)
        with self.assertRaises(subprocess.CalledProcessError):
            profile.proxy_metrics('bench', 'proxy', expected,
                                  lambda args: (_ for _ in ()).throw(subprocess.CalledProcessError(1, args)), now=101)
        with self.assertRaises(subprocess.CalledProcessError):
            profile.proxy_metrics('bench', 'proxy', expected,
                                  lambda args: json.dumps(data) if args[-1].endswith('/stats/summary') else
                                  (_ for _ in ()).throw(subprocess.CalledProcessError(1, args)), now=101)

    def test_rss_sums_all_required_instances_across_nodes_and_cgroup_drivers(self):
        expected = {'uid-a': ('client', 'a' * 64), 'uid-b': ('client', 'b' * 64),
                    'uid-c': ('server', 'c' * 64)}
        summaries = {'client': {'pods': self.metric('uid-a')['pods'] + self.metric('uid-b', 4_000_000)['pods']},
                     'server': self.metric('uid-c', 8_000_000)}
        metrics = {'client': self.rss('uid-a') + self.rss('uid-b', 'b' * 64, 8192, systemd=False),
                   'server': self.rss('uid-c', 'c' * 64, 16384)}
        # Do not count another instance, the pod aggregate, or an unrelated cgroup.
        metrics['client'] += self.rss('uid-a', 'd' * 64, 999_999)
        metrics['client'] += ('container_memory_rss{id="/kubepods/burstable/poduid-a"} 999999 100000\n'
                              'container_memory_rss{id="/unrelated",container="proxy",namespace="bench"} 999999 100000\n')

        def run(args):
            node = args[-1].split('/')[4]
            return json.dumps(summaries[node]) if args[-1].endswith('/stats/summary') else metrics[node]

        self.assertEqual(profile.proxy_metrics('bench', 'proxy', expected, run, now=101), (14, 28))
        metrics['server'] = ''
        with self.assertRaisesRegex(ValueError, 'missing proxy RSS'):
            profile.proxy_metrics('bench', 'proxy', expected, run, now=101)
        metrics['server'] = self.rss('uid-c', 'c' * 64, 16384)
        summaries['client']['pods'].append(summaries['server']['pods'][0])
        with self.assertRaisesRegex(ValueError, 'moved'):
            profile.proxy_metrics('bench', 'proxy', expected, run, now=101)

    def test_rss_rejects_missing_duplicate_wrong_instance_stale_and_invalid_samples(self):
        expected = {'uid': ('worker', 'a' * 64)}
        self.assertEqual(profile.cadvisor_rss(self.rss(), expected, 'worker', 101), 4)
        for text in (
            '', self.rss() * 2, self.rss(container_id='b' * 64), self.rss(uid='other'),
            self.rss(stamp=70_000), self.rss(stamp=102_000), self.rss(stamp='NaN'),
            self.rss(stamp='Inf'), self.rss(value='NaN'), self.rss(value='Inf'),
            self.rss(value=0), self.rss(value=-1),
            self.rss().rsplit(' ', 1)[0],  # No source timestamp: scrape time is not freshness.
            self.rss().replace('container=""', 'container="",container=""'),
            self.rss().replace('container=""', 'container=invalid'),
            # A substring match would incorrectly accept the wrong pod UID.
            self.rss(uid='prefix-uid'), self.rss(uid='uid-suffix'),
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                profile.cadvisor_rss(text, expected, 'worker', 101)
        with self.assertRaisesRegex(ValueError, 'moved'):
            profile.cadvisor_rss(self.rss(), expected, 'other-worker', 101)

    def pod(self, uid, node, container_id='a' * 64):
        return {'metadata': {'uid': uid}, 'spec': {'nodeName': node, 'containers': [{'name': 'proxy'}]},
                'status': {'phase': 'Running', 'containerStatuses': [
                    {'name': 'proxy', 'ready': True, 'containerID': f'containerd://{container_id}'}]}}

    def test_both_workers_require_ready_proxy_container_ids(self):
        pods = {'items': [self.pod('a', 'client'), self.pod('b', 'server', 'b' * 64)]}
        run = lambda args: json.dumps(pods)
        self.assertEqual(profile.expected_containers('bench', 'proxy', ['client', 'server'], run),
                         {'a': ('client', 'a' * 64), 'b': ('server', 'b' * 64)})
        for container_id in ('', 'containerd://', 'containerd://short', 'wrong://' + 'a' * 64):
            pods['items'][1]['status']['containerStatuses'][0]['containerID'] = container_id
            with self.assertRaisesRegex(ValueError, 'container ID'):
                profile.expected_containers('bench', 'proxy', ['client', 'server'], run)
        pods['items'][1]['status']['containerStatuses'][0]['ready'] = False
        with self.assertRaisesRegex(ValueError, 'not ready'):
            profile.expected_containers('bench', 'proxy', ['client', 'server'], run)
        pods['items'].pop()
        with self.assertRaisesRegex(ValueError, 'missing proxy'):
            profile.expected_containers('bench', 'proxy', ['client', 'server'], run)

    def collect(self, directory, failure=None, real_metrics=False, payload_bytes=1024,
                requested_qps=0, connections=80):
        args = types.SimpleNamespace(target='http://test/echo', mesh='interlink', duration=1,
                                     namespace='bench', container='proxy', client_node='client',
                                     server_node='server', results=pathlib.Path(directory))
        events = []
        launched = threading.Event()
        times = {}
        ticks_count = [0]
        samples = []
        processes = []
        pods = {'items': [self.pod('a', 'client'), self.pod('b', 'server', 'b' * 64)]}
        measured_count = requested_qps or 10
        if failure == 'measured-rate':
            measured_count = int(measured_count * 0.96)
        measured_codes = ({'200': measured_count - 2, '503': 2}
                          if failure in ('measured-errors', 'measured-exit-errors') else {'200': measured_count})
        if failure == 'all-errors':
            measured_codes = {'-1': measured_count}
        measured_record = self.load_record(payload_bytes=payload_bytes, connections=connections,
                                          codes=measured_codes, requested_qps=requested_qps)
        if failure == 'body':
            measured_record['Sizes']['Sum'] -= 1
        measured_text = json.dumps(measured_record)
        if failure == 'malformed':
            measured_text = 'not json'

        def run(command, timeout=30):
            if command[:3] == ['kubectl', 'get', 'pods']:
                events.append('expected')
                if failure == 'restart' and launched.is_set():
                    pods['items'][1]['status']['containerStatuses'][0]['containerID'] = 'containerd://' + 'c' * 64
                return json.dumps(pods)
            if command[:3] == ['kubectl', 'get', '--raw']:
                node = command[-1].split('/')[4]
                uid, cpu, rss = ('a', 2_000_000, 4096) if node == 'client' else ('b', 4_000_000, 8192)
                if command[-1].endswith('/stats/summary'):
                    data = self.metric(uid, cpu)
                    data['pods'][0]['containers'][0]['cpu']['time'] = dt.datetime.now(dt.timezone.utc).isoformat()
                    return json.dumps(data)
                self.assertTrue(command[-1].endswith('/metrics/cadvisor'))
                samples.append(time.time())
                events.append('sample')
                return self.rss(uid, uid * 64, rss, int(time.time() * 1000))
            self.assertIn('10s', command)
            self.assertEqual(command[command.index('-qps') + 1], str(requested_qps))
            self.assertFalse(samples, 'sampler must not run during warmup')
            events.append('warmup')
            if failure == 'warmup':
                raise subprocess.CalledProcessError(1, command)
            warmup_count = requested_qps * 10 if requested_qps else 10
            if failure == 'warmup-rate':
                warmup_count = int(warmup_count * 0.96)
            codes = ({'200': warmup_count - 3, '503': 3}
                     if failure in ('warmup-errors', 'warmup-exit-errors') else {'200': warmup_count})
            text = json.dumps(self.load_record(payload_bytes=payload_bytes, connections=connections,
                                              duration=10, codes=codes, requested_qps=requested_qps))
            if failure in ('warmup-exit-errors', 'warmup-exit-success', 'warmup-malformed'):
                raise subprocess.CalledProcessError(1, command, output='not json' if failure == 'warmup-malformed' else text)
            return text

        def sample():
            samples.append(time.time())
            events.append('sample')
            if failure == 'initial-metrics':
                raise ValueError('missing metrics')
            if failure == 'metrics' and len(samples) > 1:
                if not launched.wait(1):
                    raise TimeoutError('measured launch gate never opened')
                raise ValueError('missing metrics')
            return 2, 4

        def ticks():
            ticks_count[0] += 1
            return ticks_count[0] * 100, ticks_count[0] * 20

        class Process:
            def __init__(self, command, stdout):
                self.args = command
                self_outer.assertEqual(command[command.index('-qps') + 1], str(requested_qps))
                self.returncode = None
                self.polls = 0
                self.terminated = False
                times['start'] = time.time()
                stdout.write(measured_text)
                stdout.flush()
                launched.set()
                processes.append(self)

            def poll(self):
                self.polls += 1
                if failure in ('metrics', 'restart') and not self.terminated:
                    return None
                if self.polls > 1:
                    times.setdefault('end', time.time())
                    self.returncode = 1 if failure in ('load', 'measured-exit-errors') else 0
                return self.returncode

            def terminate(self):
                self.terminated = True
                self.returncode = -15

            def wait(self, timeout):
                return self.returncode

        workload = {'name': 'shared' if requested_qps else ('rps' if payload_bytes == 1024 else 'bulk'),
                    'payload_bytes': payload_bytes}
        sampler = None if real_metrics else sample
        self_outer = self
        with Deadline():
            if (failure in ('warmup', 'initial-metrics', 'metrics', 'load', 'malformed', 'restart',
                            'warmup-exit-success', 'warmup-malformed', 'body', 'warmup-rate', 'measured-rate')
                    or requested_qps and failure in ('warmup-errors', 'warmup-exit-errors', 'measured-errors',
                                                    'measured-exit-errors', 'all-errors')):
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    profile.collect_point(args, workload, connections, run, sampler, ticks, Process,
                                          interval=0.01, requested_qps=requested_qps)
            else:
                profile.collect_point(args, workload, connections, run, sampler, ticks, Process,
                                      interval=0.01, requested_qps=requested_qps)
        return events, samples, times, processes

    def test_sampler_starts_after_warmup_and_brackets_full_measured_window(self):
        with tempfile.TemporaryDirectory() as temporary:
            events, samples, times, _ = self.collect(temporary)
            self.assertEqual(events[:3], ['warmup', 'expected', 'sample'])
            self.assertLessEqual(samples[0], times['start'])
            self.assertGreaterEqual(samples[-1], times['end'])
            for path in pathlib.Path(temporary).glob('*.csv'):
                with path.open() as source:
                    rows = list(csv.DictReader(source))
                self.assertGreaterEqual(len(rows), 2)
                self.assertLessEqual(float(rows[0]['timestamp']), times['start'])
                self.assertGreaterEqual(float(rows[-1]['timestamp']), times['end'])

    def test_default_collector_publishes_cadvisor_rss_not_working_set_for_full_window(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, samples, times, _ = self.collect(temporary, real_metrics=True)
            self.assertLessEqual(samples[0], times['start'])
            self.assertGreaterEqual(samples[-1], times['end'])
            with (pathlib.Path(temporary) / 'metrics-interlink-rps-c80.csv').open() as source:
                rows = list(csv.DictReader(source))
            self.assertGreaterEqual(len(rows), 2)
            self.assertLessEqual(float(rows[0]['timestamp']), times['start'])
            self.assertGreaterEqual(float(rows[-1]['timestamp']), times['end'])
            self.assertEqual({float(row['cpu_millicores']) for row in rows}, {6})
            self.assertEqual({float(row['memory_rss_kb']) for row in rows}, {12})

    def test_warmup_and_initial_metrics_fail_before_measured_launch(self):
        for failure in ('warmup', 'initial-metrics', 'warmup-exit-success', 'warmup-malformed'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                _, _, _, processes = self.collect(temporary, failure)
                self.assertEqual(processes, [])

    def test_sampler_failure_terminates_measured_load_and_propagates(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, samples, _, processes = self.collect(temporary, 'metrics')
            self.assertGreaterEqual(len(samples), 2)
            self.assertTrue(processes[0].terminated)

    def test_measured_command_failure_still_takes_final_sample(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, samples, times, _ = self.collect(temporary, 'load')
            self.assertGreaterEqual(samples[-1], times['end'])

    def test_request_error_boundaries_keep_separate_warmup_codes_and_normalized_bulk_proof(self):
        for failure in ('warmup-errors', 'warmup-exit-errors', 'measured-errors', 'measured-exit-errors', 'all-errors'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                _, _, _, processes = self.collect(temporary, failure, payload_bytes=1_048_576)
                record = json.loads((pathlib.Path(temporary) / 'fortio-interlink-bulk-c80.json').read_text())
                self.assertNotIn('Payload', record)
                self.assertEqual(record['PayloadBytes'], 1_048_576)
                self.assertEqual(record['PayloadSHA256'], hashlib.sha256(b'x' * 1_048_576).hexdigest())
                self.assertEqual(record['WarmupRetCodes'], {'200': 7, '503': 3} if failure.startswith('warmup') else {'200': 10})
                expected_codes = {'-1': 10} if failure == 'all-errors' else (
                    {'200': 8, '503': 2} if failure.startswith('measured') else {'200': 10})
                self.assertEqual(record['RetCodes'], expected_codes)
                self.assertEqual(processes[0].args[processes[0].args.index('-qps') + 1], '0')
                self.assertEqual(processes[0].args[processes[0].args.index('-payload-size') + 1], '1048576')
                self.assertIn('-stdclient', processes[0].args)
                self.assertIs(record['DisableFastClient'], True)
                self.assertEqual(record['HeaderSizes'], {'Count': 0, 'Min': 0, 'Max': 0, 'Sum': 0})
                self.assertEqual(record['Sizes']['Sum'], 10 * 1_048_576)
                self.assertTrue((pathlib.Path(temporary) / 'metrics-interlink-bulk-c80.csv').is_file())

    def test_shared_collector_brackets_load_and_writes_only_the_contract_file_triple(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, samples, times, processes = self.collect(temporary, real_metrics=True,
                                                       requested_qps=10000, connections=64)
            self.assertEqual(len(processes), 1)
            self.assertLessEqual(samples[0], times['start'])
            self.assertGreaterEqual(samples[-1], times['end'])
            root = pathlib.Path(temporary)
            self.assertEqual({path.name for path in root.iterdir()}, {
                'fortio-interlink-shared-c64.json', 'metrics-interlink-shared-c64.csv',
                'metrics-interlink-shared-c64.host.csv'})
            result = json.loads((root / 'fortio-interlink-shared-c64.json').read_text())
            self.assertEqual(result['RequestedQPS'], '10000')
            self.assertEqual(result['ActualQPS'], 10000)
            self.assertEqual(result['WarmupRetCodes'], {'200': 100000})
            self.assertEqual(result['RetCodes'], {'200': 10000})
            self.assertEqual(result['PayloadBytes'], 1024)
            self.assertNotIn('Payload', result)

    def test_shared_collector_aborts_warmup_errors_and_drift_before_measured_launch(self):
        for failure in ('warmup-errors', 'warmup-exit-errors', 'warmup-rate'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                _, _, _, processes = self.collect(temporary, failure, requested_qps=10000, connections=64)
                self.assertEqual(processes, [])

    def test_shared_collector_rejects_measured_errors_drift_and_body_corruption_with_raw_evidence(self):
        for failure in ('measured-errors', 'measured-exit-errors', 'all-errors', 'measured-rate', 'body'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                _, samples, times, processes = self.collect(temporary, failure,
                                                           requested_qps=10000, connections=64)
                self.assertEqual(len(processes), 1)
                self.assertGreaterEqual(samples[-1], times['end'])
                result = json.loads((pathlib.Path(temporary) / 'fortio-interlink-shared-c64.json').read_text())
                self.assertIn('Payload', result)
                if 'errors' in failure:
                    self.assertGreater(profile.request_errors(result['RetCodes']), 0)
                elif failure == 'measured-rate':
                    self.assertEqual(result['ActualQPS'], 9600)
                else:
                    self.assertEqual(result['Sizes']['Sum'] - result['HeaderSizes']['Sum'], 10000 * 1024 - 1)

    def test_restarted_required_proxy_aborts_and_terminates_measured_load(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, _, _, processes = self.collect(temporary, 'restart', real_metrics=True)
            self.assertTrue(processes[0].terminated)

    def test_malformed_measured_command_output_is_not_an_error_boundary(self):
        with tempfile.TemporaryDirectory() as temporary:
            self.collect(temporary, 'malformed')

    def collector_cli(self, extra=()):
        return [
            'capture_profile.py', '--mesh', 'interlink', '--namespace', 'bench', '--container', 'proxy',
            '--target', 'http://test/echo', '--client-node', 'client', '--server-node', 'server',
            '--results', '/unused', '--duration', '20', '--rps-concurrency', '1,4,16', '--bulk-concurrency', '1,4',
            *extra,
        ]

    def test_cli_collects_default_shared_control_before_the_entire_capacity_grid(self):
        with mock.patch.object(sys, 'argv', self.collector_cli()), \
             mock.patch.object(profile, 'collect_point') as collect:
            profile.main()
        self.assertEqual([(call.args[1]['name'], call.args[1]['payload_bytes'], call.args[2],
                           call.kwargs.get('requested_qps', 0)) for call in collect.call_args_list],
                         [('shared', 1024, 64, 10000), ('rps', 1024, 1, 0), ('rps', 1024, 4, 0),
                          ('rps', 1024, 16, 0), ('bulk', 1_048_576, 1, 0), ('bulk', 1_048_576, 4, 0)])

    def test_cli_shared_only_collects_exactly_one_control_with_custom_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            collect_point = profile.collect_point
            collected = []

            def collect(args, workload, connections, *, requested_qps=0):
                collected.append((workload, connections, requested_qps))
                # Exercise the collector's actual command/validation/file consumer,
                # not just its CLI dispatch, without starting external workloads.
                with mock.patch.object(profile, 'collect_point', collect_point):
                    self.collect(temporary, requested_qps=requested_qps, connections=connections)

            with mock.patch.object(sys, 'argv', self.collector_cli([
                '--shared-only', '--shared-rps', '8000', '--shared-concurrency', '32',
            ])), mock.patch.object(profile, 'collect_point', side_effect=collect):
                profile.main()
            self.assertEqual(collected, [({'name': 'shared', 'payload_bytes': 1024}, 32, 8000)])
            root = pathlib.Path(temporary)
            self.assertEqual({path.name for path in root.iterdir()}, {
                'fortio-interlink-shared-c32.json', 'metrics-interlink-shared-c32.csv',
                'metrics-interlink-shared-c32.host.csv'})
            result = json.loads((root / 'fortio-interlink-shared-c32.json').read_text())
            self.assertEqual(result['RequestedQPS'], '8000')
            self.assertEqual(result['ActualQPS'], 8000)

    def test_cli_rejects_invalid_controls_or_grids_before_any_collection(self):
        for option, value in (
            ('--shared-rps', '0'), ('--shared-rps', '-1'), ('--shared-rps', 'nan'),
            ('--shared-rps', '1.5'), ('--shared-concurrency', '0'), ('--shared-concurrency', '-1'),
            ('--shared-concurrency', '1.5'), ('--duration', '0'), ('--rps-concurrency', '1,1'),
            ('--bulk-concurrency', '0'),
        ):
            with self.subTest(option=option, value=value), \
                 mock.patch.object(sys, 'argv', self.collector_cli([option, value])), \
                 mock.patch.object(profile, 'collect_point') as collect, \
                 mock.patch('sys.stderr'), self.assertRaises(SystemExit) as error:
                profile.main()
            self.assertEqual(error.exception.code, 2)
            collect.assert_not_called()

    def test_publication_replaces_whole_cohort_and_keeps_private_files_out(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            stage = root / 'stage'
            stage.mkdir()
            old = root / 'results'
            old.mkdir()
            (old / 'old-profile.json').write_text('old')
            data = stage / 'data'
            data.mkdir()
            (data / 'comparison.md').write_text('new valid cohort')
            (stage / 'private.key').write_text('secret')
            readme, pending_readme = root / 'README.md', stage / 'README.md'
            readme.write_text('old headline')
            pending_readme.write_text('new headline')
            calls = []
            capture.publish_results(data, old, 'a' * 40, guard=calls.append, readme=(pending_readme, readme))
            self.assertEqual(calls, ['a' * 40])
            self.assertEqual([p.name for p in old.iterdir()], ['comparison.md'])
            self.assertEqual((old / 'comparison.md').read_text(), 'new valid cohort')
            self.assertEqual(readme.read_text(), 'new headline')
            self.assertTrue((stage / 'private.key').exists())
            self.assertFalse((stage / 'previous-results').exists())

    def test_stale_source_never_replaces_existing_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            old, data = root / 'results', root / 'data'
            old.mkdir()
            data.mkdir()
            (old / 'comparison.md').write_text('valid prior capture')
            readme, pending_readme = root / 'README.md', root / 'staged-README.md'
            readme.write_text('valid prior headline')
            pending_readme.write_text('unpublished new headline')
            with self.assertRaisesRegex(RuntimeError, 'stale'):
                capture.publish_results(data, old, 'a' * 40,
                                        guard=mock.Mock(side_effect=RuntimeError('stale')),
                                        readme=(pending_readme, readme))
            self.assertEqual((old / 'comparison.md').read_text(), 'valid prior capture')
            self.assertTrue(data.exists())
            self.assertEqual(readme.read_text(), 'valid prior headline')
            self.assertEqual(pending_readme.read_text(), 'unpublished new headline')

    def test_rename_failure_rolls_back_prior_dataset(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            old, data = root / 'results', root / 'data'
            old.mkdir()
            data.mkdir()
            (old / 'comparison.md').write_text('valid')
            rename = pathlib.Path.rename

            def fail_staged(path, destination):
                if path == data:
                    raise OSError('injected rename failure')
                return rename(path, destination)

            with mock.patch.object(pathlib.Path, 'rename', fail_staged), self.assertRaises(OSError):
                capture.publish_results(data, old, 'a' * 40, guard=lambda revision: None)
            self.assertEqual((old / 'comparison.md').read_text(), 'valid')

    def test_readme_publication_failure_rolls_back_both_headline_and_canonical_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            stage = root / 'stage'
            stage.mkdir()
            old, data = root / 'results', stage / 'data'
            old.mkdir()
            data.mkdir()
            (old / 'comparison.md').write_text('valid prior comparison')
            (data / 'comparison.md').write_text('new comparison')
            readme, pending = root / 'README.md', stage / 'README.md'
            readme.write_text('valid prior headline')
            pending.write_text('new headline')
            rename = pathlib.Path.rename

            def fail_readme(path, destination):
                if path == pending:
                    raise OSError('injected README publication failure')
                return rename(path, destination)

            with mock.patch.object(pathlib.Path, 'rename', fail_readme), self.assertRaises(OSError):
                capture.publish_results(data, old, 'a' * 40, guard=lambda revision: None,
                                        readme=(pending, readme))
            self.assertEqual((old / 'comparison.md').read_text(), 'valid prior comparison')
            self.assertEqual(readme.read_text(), 'valid prior headline')
            self.assertEqual((data / 'comparison.md').read_text(), 'new comparison')
            self.assertEqual(pending.read_text(), 'new headline')

    def test_archive_requires_full_explicit_provenance_and_rejects_source_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / 'source.rs'
            source.write_text('source')
            with mock.patch.dict(os.environ, {}, clear=True), self.assertRaisesRegex(RuntimeError, 'archive requires'):
                capture.source_identity(root)
            with mock.patch.dict(os.environ, BENCH_REVISION='short'), self.assertRaises(ValueError):
                capture.source_identity(root)
            with mock.patch.dict(os.environ, BENCH_REVISION='a' * 40):
                revision, guard = capture.source_identity(root)
                self.assertEqual(revision, 'a' * 40)
                (root / 'tmp').mkdir()
                (root / 'tmp' / 'private').write_text('operational state')
                guard(revision)
                source.write_text('different source')
                with self.assertRaisesRegex(RuntimeError, 'source changed'):
                    guard(revision)

    def test_git_guard_rejects_changed_head_dirty_source_and_false_explicit_revision(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            (root / '.git').mkdir()
            with mock.patch.dict(os.environ, {}, clear=True):
                with mock.patch.object(capture, 'git', side_effect=['a' * 40, 'a' * 40, '']):
                    revision, guard = capture.source_identity(root)
                with mock.patch.object(capture, 'git', return_value='b' * 40):
                    with self.assertRaisesRegex(RuntimeError, 'HEAD changed'):
                        guard(revision)
                with mock.patch.object(capture, 'git', side_effect=['a' * 40, ' M source.rs']):
                    with self.assertRaisesRegex(RuntimeError, 'clean committed source'):
                        guard(revision)
            with mock.patch.dict(os.environ, BENCH_REVISION='b' * 40), mock.patch.object(capture, 'git', return_value='a' * 40):
                with self.assertRaisesRegex(RuntimeError, 'does not match'):
                    capture.source_identity(root)

    def orchestration(self, failure=None):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            bench = root / 'bench'
            bench.mkdir()
            # This fake repository still supplies genuine capture inputs; the
            # orchestrator's real provenance validation must remain enabled.
            for relative in capture.HARNESS_ROOT_FILES:
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(capture.ROOT / relative, destination)
            for relative in capture.HARNESS_INPUT_DIRS:
                shutil.copytree(capture.ROOT / relative, root / relative, dirs_exist_ok=True)
            previous = bench / 'results'
            previous.mkdir()
            (previous / 'comparison.md').write_text('previous valid capture')
            (root / 'README.md').write_text('previous valid headline')
            scripts = []
            reporter_calls = []
            guard = mock.Mock()

            def script(name, env, log, timeout):
                scripts.append(name)
                if name == failure:
                    raise subprocess.CalledProcessError(1, name)
                private = pathlib.Path(env['BENCH_PRIVATE_DIR'])
                (private / 'credential.key').write_text('secret')
                if name.startswith('scripts/'):
                    mesh = {'scripts/run-interlink.sh': 'interlink',
                            'scripts/run-linkerd.sh': 'linkerd',
                            'scripts/run-istio.sh': 'istio-ambient'}[name]
                    data = pathlib.Path(env['BENCH_CAPTURE_DIR']) / mesh
                    data.mkdir()
                    (data / 'evidence.json').write_text('{}')

            def command(args, **kwargs):
                if '--results' in args:
                    reporter_calls.append(args)
                    data = pathlib.Path(args[args.index('--results') + 1])
                    metadata = json.loads((data / 'capture.json').read_text())
                    self.assertEqual(metadata['meshes'], list(capture.MESHES))
                    self.assertEqual(metadata['revision'], 'a' * 40)
                    self.assertEqual(metadata['schema_version'], 2)
                    self.assertEqual(metadata['benchmark'], 'echo-capacity')
                    self.assertEqual(metadata['server_delay_ms'], 0)
                    self.assertEqual(metadata['duration_seconds'], 20)
                    self.assertEqual(metadata['request_timeout_seconds'], 5)
                    self.assertEqual(metadata['workloads'],
                                     profile.workload_grids('1,4,16,64,256,1024,4096', '1,4,16,64,256,1024'))
                    self.assertNotIn('profiles', metadata)
                    self.assertNotIn('payload_bytes', metadata)
                    self.assertEqual(set(metadata['versions']),
                                     {'kind', 'kubernetes', 'linkerd', 'istio', 'fortio'})
                    self.assertEqual({p.name for p in data.iterdir()},
                                     set(capture.MESHES) | {'capture.json'})
                    if failure == 'reporter':
                        raise subprocess.CalledProcessError(1, args)
                    pathlib.Path(args[args.index('--output') + 1]).write_text('new validated capture')
                    pathlib.Path(args[args.index('--readme') + 1]).write_text('new validated headline')
                return types.SimpleNamespace(returncode=0, stdout='')

            pins = dict(KIND_VERSION='v0.24.0', KUBECTL_VERSION='v1.30.0',
                        LINKERD_VERSION='stable-2.14.10', ISTIO_VERSION='1.24.0',
                        FORTIO_VERSION='1.66.0', PATH=os.defpath)
            previous_umask = os.umask(0o077)
            try:
                with mock.patch.object(capture, 'ROOT', root), mock.patch.object(capture, 'BENCH', bench), \
                     mock.patch.object(capture, 'source_identity', return_value=('a' * 40, guard)), \
                     mock.patch.object(capture, 'run_script', side_effect=script), \
                     mock.patch.object(capture, 'load_results', side_effect=ValueError('invalid cohort')
                                       if failure == 'validation' else None, return_value=({}, [])), \
                     mock.patch.object(capture, 'peak_metadata', return_value={}), \
                     mock.patch.object(capture.subprocess, 'run', side_effect=command), \
                     mock.patch.dict(os.environ, pins, clear=True):
                    if failure:
                        with self.assertRaises((subprocess.CalledProcessError, ValueError)):
                            capture.capture()
                        self.assertEqual((previous / 'comparison.md').read_text(), 'previous valid capture')
                        self.assertEqual((root / 'README.md').read_text(), 'previous valid headline')
                        guard.assert_not_called()
                    else:
                        capture.capture()
                        self.assertEqual((previous / 'comparison.md').read_text(), 'new validated capture')
                        self.assertEqual((root / 'README.md').read_text(), 'new validated headline')
                        guard.assert_called_once_with('a' * 40)
                        self.assertFalse(list(previous.rglob('*.key')))
                        self.assertEqual(len(reporter_calls), 1)
            finally:
                os.umask(previous_umask)
            self.assertIn('teardown.sh', scripts)
            rejected = list((root / 'tmp').glob('rejected-capture-*'))
            if failure and failure != 'setup.sh':
                self.assertEqual(len(rejected), 1)
                meshes = ['interlink'] if failure == 'scripts/run-linkerd.sh' else list(capture.MESHES)
                for mesh in meshes:
                    self.assertEqual((rejected[0] / mesh / 'evidence.json').read_text(), '{}')
                self.assertEqual(rejected[0].stat().st_mode & 0o077, 0)
            else:
                self.assertEqual(rejected, [])
            self.assertFalse(list((root / 'tmp').rglob('*.key')))
            self.assertEqual({p.name for p in (root / 'tmp').iterdir()},
                             {'mesh-capture.lock', *(p.name for p in rejected)})
            if failure != 'reporter' and failure is not None:
                self.assertEqual(reporter_calls, [])

    def test_orchestrator_publishes_only_complete_reported_cohort_and_removes_private_state(self):
        self.orchestration()

    def test_setup_mesh_validation_and_reporter_errors_cleanup_without_publication(self):
        for failure in ('setup.sh', 'scripts/run-linkerd.sh', 'validation', 'reporter'):
            with self.subTest(failure=failure):
                self.orchestration(failure)

    def test_cleanup_attempts_every_resource_and_fails_closed_on_errors(self):
        calls = []

        def command(args, **kwargs):
            calls.append(args)
            if args[2] == 'rm' and 'echo-server' in args[-1]:
                raise subprocess.CalledProcessError(1, args)
            return types.SimpleNamespace(stdout='image-id')

        with mock.patch.object(capture, 'run_script', side_effect=RuntimeError('cluster deletion failed')), \
             mock.patch.object(capture.subprocess, 'run', side_effect=command):
            with self.assertRaisesRegex(RuntimeError, 'cleanup failed'):
                capture.cleanup({'BENCH_IMAGE_TAG': 'unique'}, None)
        self.assertEqual(len(calls), 4)
        self.assertIn('interlinkd', calls[-1][-1])


    def test_metrics_use_clock_after_fetch(self):
        clock = [100.0]
        stamp = '1970-01-01T00:01:41Z'
        summary = {'pods': [{
            'podRef': {'uid': 'proxy-uid', 'namespace': 'bench'},
            'containers': [{
                'name': 'interlinkd',
                'cpu': {'time': stamp, 'usageNanoCores': 12_000_000},
                'memory': {'time': stamp, 'workingSetBytes': 9_999_999},
            }],
        }]}

        def fetch(args):
            clock[0] += 2.0
            if args[-1].endswith('/stats/summary'):
                return json.dumps(summary)
            return self.rss('proxy-uid', value=1_048_576, stamp=103_000)

        with mock.patch.object(profile.time, 'time', side_effect=lambda: clock[0]):
            measured = profile.proxy_metrics(
                'bench', 'interlinkd', {'proxy-uid': ('worker', 'a' * 64)}, run=fetch)
        self.assertEqual(measured, (12.0, 1024.0))


if __name__ == '__main__':
    unittest.main()
