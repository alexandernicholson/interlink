"""Deterministic producer tests; no Docker, Kubernetes, downloads or workloads.

Run: python3 -B -m unittest discover -s bench/tests -p 'test_run.py'
"""
import csv
import datetime as dt
import json
import os
import pathlib
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
    def test_profiles_preserve_defaults_and_reject_invalid_or_duplicate_load(self):
        self.assertEqual(profile.profiles('320:80,800:200,1600:400'),
                         [(320, 80), (800, 200), (1600, 400)])
        for value in ('0:80', '320:-1', '320', '', '320:80,320:80', 'nan:1'):
            with self.subTest(value=value), self.assertRaises(ValueError):
                profile.profiles(value)

    def test_load_requires_successful_real_requests(self):
        self.assertEqual(profile.validate_load('{"RetCodes":{"200":4}}')['RetCodes']['200'], 4)
        for text in ('{}', '{"RetCodes":{}}', '{"RetCodes":{"200":0}}',
                     '{"RetCodes":{"200":1,"503":1}}', '{"RetCodes":{"200":NaN}}',
                     '{"RetCodes":{"200":Infinity}}', '{"RetCodes":{"200":true}}',
                     '{"RetCodes":{"200":-1}}', 'not json'):
            with self.subTest(text=text), self.assertRaises((ValueError, KeyError)):
                profile.validate_load(text)

    def test_host_delta_does_not_fabricate_zero_or_double_count_guest(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = pathlib.Path(temporary) / 'stat'
            source.write_text('cpu 10 20 30 40 50 60 70 80 90 100\n')
            self.assertEqual(profile.host_ticks(source), (360, 90))
        self.assertEqual(profile.host_util((100, 20), (200, 60)), 60)
        for current in ((100, 20), (90, 20), (200, 10), (200, 121)):
            with self.assertRaises(ValueError):
                profile.host_util((100, 20), current)

    def metric(self):
        stamp = dt.datetime.fromtimestamp(100, dt.timezone.utc).isoformat()
        return {'pods': [{'podRef': {'uid': 'uid', 'namespace': 'bench'}, 'containers': [
            {'name': 'proxy', 'cpu': {'time': stamp, 'usageNanoCores': 2_000_000},
             'memory': {'time': stamp, 'rssBytes': 4096}}]}]}

    def test_actual_rss_and_cpu_require_every_expected_container(self):
        data = self.metric()
        run = lambda args: json.dumps(data)
        self.assertEqual(profile.proxy_metrics('bench', 'proxy', {'uid': 'worker'}, run, now=101), (2, 4))
        for mutate in (
            lambda d: d['pods'].clear(),
            lambda d: d['pods'][0]['containers'].clear(),
            lambda d: d['pods'][0]['containers'][0]['memory'].pop('rssBytes'),
            lambda d: d['pods'][0]['containers'][0]['cpu'].update(usageNanoCores=float('nan')),
            lambda d: d['pods'][0]['containers'][0]['memory'].update(rssBytes=0),
        ):
            data = self.metric()
            mutate(data)
            with self.assertRaises((ValueError, KeyError)):
                profile.proxy_metrics('bench', 'proxy', {'uid': 'worker'}, run, now=101)
        data = self.metric()
        with self.assertRaisesRegex(ValueError, 'stale'):
            profile.proxy_metrics('bench', 'proxy', {'uid': 'worker'}, run, now=131)
        with self.assertRaises(subprocess.CalledProcessError):
            profile.proxy_metrics('bench', 'proxy', {'uid': 'worker'},
                                  lambda args: (_ for _ in ()).throw(subprocess.CalledProcessError(1, args)), now=101)

    def pod(self, uid, node):
        return {'metadata': {'uid': uid}, 'spec': {'nodeName': node, 'containers': [{'name': 'proxy'}]},
                'status': {'phase': 'Running', 'containerStatuses': [{'name': 'proxy', 'ready': True}]}}

    def test_both_workers_require_ready_proxy_containers(self):
        pods = {'items': [self.pod('a', 'client'), self.pod('b', 'server')]}
        run = lambda args: json.dumps(pods)
        self.assertEqual(profile.expected_containers('bench', 'proxy', ['client', 'server'], run),
                         {'a': 'client', 'b': 'server'})
        pods['items'][1]['status']['containerStatuses'][0]['ready'] = False
        with self.assertRaisesRegex(ValueError, 'not ready'):
            profile.expected_containers('bench', 'proxy', ['client', 'server'], run)
        pods['items'].pop()
        with self.assertRaisesRegex(ValueError, 'missing proxy'):
            profile.expected_containers('bench', 'proxy', ['client', 'server'], run)

    def collect(self, directory, failure=None):
        args = types.SimpleNamespace(target='http://test/echo', mesh='interlink', duration=1,
                                     namespace='bench', container='proxy', client_node='client',
                                     server_node='server', results=pathlib.Path(directory))
        events = []
        launched = threading.Event()
        times = {}
        ticks_count = [0]
        samples = []
        processes = []
        pods = {'items': [self.pod('a', 'client'), self.pod('b', 'server')]}

        def run(command, timeout=30):
            if command[:3] == ['kubectl', 'get', 'pods']:
                events.append('expected')
                return json.dumps(pods)
            self.assertIn('10s', command)
            self.assertFalse(samples, 'sampler must not run during warmup')
            events.append('warmup')
            if failure == 'warmup':
                raise subprocess.CalledProcessError(1, command)
            return '{"RetCodes":{"200":10}}'

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
                self.returncode = None
                self.polls = 0
                self.terminated = False
                times['start'] = time.time()
                stdout.write('{"RetCodes":{"200":10}}')
                stdout.flush()
                launched.set()
                processes.append(self)

            def poll(self):
                self.polls += 1
                if failure == 'metrics' and not self.terminated:
                    return None
                if self.polls > 1:
                    times.setdefault('end', time.time())
                    self.returncode = 1 if failure == 'load' else 0
                return self.returncode

            def terminate(self):
                self.terminated = True
                self.returncode = -15

            def wait(self, timeout):
                return self.returncode

        with Deadline():
            if failure:
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    profile.collect_profile(args, 320, 80, run, sample, ticks, Process, interval=0.01)
            else:
                profile.collect_profile(args, 320, 80, run, sample, ticks, Process, interval=0.01)
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

    def test_warmup_and_initial_metrics_fail_before_measured_launch(self):
        for failure in ('warmup', 'initial-metrics'):
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
            calls = []
            capture.publish_results(data, old, 'a' * 40, guard=calls.append)
            self.assertEqual(calls, ['a' * 40])
            self.assertEqual([p.name for p in old.iterdir()], ['comparison.md'])
            self.assertEqual((old / 'comparison.md').read_text(), 'new valid cohort')
            self.assertTrue((stage / 'private.key').exists())
            self.assertFalse((stage / 'previous-results').exists())

    def test_stale_source_never_replaces_existing_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            old, data = root / 'results', root / 'data'
            old.mkdir()
            data.mkdir()
            (old / 'comparison.md').write_text('valid prior capture')
            with self.assertRaisesRegex(RuntimeError, 'stale'):
                capture.publish_results(data, old, 'a' * 40,
                                        guard=mock.Mock(side_effect=RuntimeError('stale')))
            self.assertEqual((old / 'comparison.md').read_text(), 'valid prior capture')
            self.assertTrue(data.exists())

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

    def test_remote_source_guard_accepts_only_exact_current_revision(self):
        with mock.patch.object(capture, 'git', return_value='a' * 40 + '\trefs/heads/main'):
            capture.assert_remote_revision('a' * 40)
            with self.assertRaisesRegex(RuntimeError, 'advanced'):
                capture.assert_remote_revision('b' * 40)
        with mock.patch.object(capture, 'git', side_effect=subprocess.CalledProcessError(1, ['git'])):
            with self.assertRaises(subprocess.CalledProcessError):
                capture.assert_remote_revision('a' * 40)

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
            previous = bench / 'results'
            previous.mkdir()
            (previous / 'comparison.md').write_text('previous valid capture')
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
                    self.assertEqual(metadata['profiles'], [
                        {'qps': 320, 'connections': 80},
                        {'qps': 800, 'connections': 200},
                        {'qps': 1600, 'connections': 400}])
                    self.assertEqual(set(metadata['versions']),
                                     {'kind', 'kubernetes', 'linkerd', 'istio', 'fortio'})
                    self.assertEqual({p.name for p in data.iterdir()},
                                     set(capture.MESHES) | {'capture.json'})
                    if failure == 'reporter':
                        raise subprocess.CalledProcessError(1, args)
                    pathlib.Path(args[args.index('--output') + 1]).write_text('new validated capture')
                return types.SimpleNamespace(returncode=0, stdout='')

            pins = dict(KIND_VERSION='v0.24.0', KUBECTL_VERSION='v1.30.0',
                        LINKERD_VERSION='stable-2.14.10', ISTIO_VERSION='1.24.0',
                        FORTIO_VERSION='1.66.0', PATH=os.defpath)
            previous_umask = os.umask(0o077)
            try:
                with mock.patch.object(capture, 'ROOT', root), mock.patch.object(capture, 'BENCH', bench), \
                     mock.patch.object(capture, 'source_identity', return_value=('a' * 40, guard)), \
                     mock.patch.object(capture, 'run_script', side_effect=script), \
                     mock.patch.object(capture.subprocess, 'run', side_effect=command), \
                     mock.patch.dict(os.environ, pins, clear=True):
                    if failure:
                        with self.assertRaises(subprocess.CalledProcessError):
                            capture.capture()
                        self.assertEqual((previous / 'comparison.md').read_text(), 'previous valid capture')
                        guard.assert_not_called()
                    else:
                        capture.capture()
                        self.assertEqual((previous / 'comparison.md').read_text(), 'new validated capture')
                        guard.assert_called_once_with('a' * 40)
                        self.assertFalse(list(previous.rglob('*.key')))
                        self.assertEqual(len(reporter_calls), 1)
            finally:
                os.umask(previous_umask)
            self.assertIn('teardown.sh', scripts)
            self.assertEqual([p.name for p in (root / 'tmp').iterdir()], ['mesh-capture.lock'])
            if failure != 'reporter' and failure is not None:
                self.assertEqual(reporter_calls, [])

    def test_orchestrator_publishes_only_complete_reported_cohort_and_removes_private_state(self):
        self.orchestration()

    def test_setup_mesh_and_reporter_errors_cleanup_without_publication(self):
        for failure in ('setup.sh', 'scripts/run-linkerd.sh', 'reporter'):
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
                'memory': {'time': stamp, 'rssBytes': 1_048_576},
            }],
        }]}

        def fetch(args):
            clock[0] = 102.0
            return json.dumps(summary)

        with mock.patch.object(profile.time, 'time', side_effect=lambda: clock[0]):
            measured = profile.proxy_metrics(
                'bench', 'interlinkd', {'proxy-uid': 'worker'}, run=fetch)
        self.assertEqual(measured, (12.0, 1024.0))


if __name__ == '__main__':
    unittest.main()
