#!/usr/bin/env python3
"""Collect actual kubelet RSS/CPU and Linux host utilization around measured Fortio.

Kubelet summary memory.rssBytes records resident memory, not working set.
The sampler starts only after successful discarded warmup, takes a ready sample
before measured load starts, and takes its final sample after load exits. Missing
containers, stale/non-finite metrics and any command failure abort the capture.
"""
import argparse
import csv
import datetime as dt
import json
import math
import pathlib
import subprocess
import threading
import time


def command(args, timeout=30):
    return subprocess.run(args, check=True, capture_output=True, text=True, timeout=timeout).stdout


def profiles(spec):
    result = []
    for item in spec.split(','):
        parts = item.split(':')
        if len(parts) != 2 or any(not p.isdigit() or int(p) <= 0 for p in parts):
            raise ValueError('profiles must be comma-separated positive qps:connections pairs')
        pair = tuple(map(int, parts))
        if pair in result:
            raise ValueError('duplicate profile')
        result.append(pair)
    return result


def number(value, positive=False):
    if isinstance(value, bool) or not isinstance(value, (float, int)) or not math.isfinite(value):
        raise ValueError('missing or non-finite metric')
    if value < 0 or (positive and value == 0):
        raise ValueError('invalid metric value')
    return value


def host_ticks(path='/proc/stat'):
    fields = pathlib.Path(path).read_text().splitlines()[0].split()
    if fields[0] != 'cpu' or len(fields) < 9:
        raise ValueError('Linux aggregate CPU counters unavailable')
    # guest/guest_nice are already included in user/nice; do not double-count them.
    ticks = [int(v) for v in fields[1:9]]
    return sum(ticks), ticks[3] + ticks[4]


def host_util(previous, current):
    total = current[0] - previous[0]
    idle = current[1] - previous[1]
    if total <= 0 or idle < 0 or idle > total:
        raise ValueError('invalid host CPU delta')
    return 100 * (total - idle) / total


def expected_containers(namespace, container, nodes, run=command):
    pods = json.loads(run(['kubectl', 'get', 'pods', '-n', namespace, '-o', 'json']))['items']
    expected = {}
    for pod in pods:
        if not any(c['name'] == container for c in pod['spec']['containers']):
            continue
        if pod['status'].get('phase') != 'Running' or pod['metadata'].get('deletionTimestamp'):
            raise ValueError('proxy pod is not running')
        status = next((c for c in pod['status'].get('containerStatuses', []) if c['name'] == container), None)
        if status is None or not status.get('ready'):
            raise ValueError('proxy container is not ready')
        expected[pod['metadata']['uid']] = pod['spec']['nodeName']
    if not expected or not set(nodes).issubset(set(expected.values())):
        raise ValueError('missing proxy on a workload worker')
    return expected


def proxy_metrics(namespace, container, expected, run=command, now=None):
    cpu = rss = 0
    seen = set()
    for node in sorted(set(expected.values())):
        summary = json.loads(run(['kubectl', 'get', '--raw', f'/api/v1/nodes/{node}/proxy/stats/summary']))
        observed_at = time.time() if now is None else now
        for pod in summary['pods']:
            uid = pod['podRef']['uid']
            if uid not in expected or pod['podRef']['namespace'] != namespace:
                continue
            if expected[uid] != node:
                raise ValueError('proxy moved during measurement')
            matches = [c for c in pod['containers'] if c['name'] == container]
            if len(matches) != 1 or uid in seen:
                raise ValueError('duplicate or missing proxy container')
            metric = matches[0]
            for kind in ('cpu', 'memory'):
                stamp = dt.datetime.fromisoformat(metric[kind]['time'].replace('Z', '+00:00')).timestamp()
                if not 0 <= observed_at - stamp <= 30:
                    raise ValueError('stale kubelet metric')
            cpu += number(metric['cpu']['usageNanoCores']) / 1_000_000
            rss += number(metric['memory']['rssBytes'], positive=True) / 1024
            seen.add(uid)
    if seen != set(expected):
        raise ValueError('missing proxy metrics')
    return cpu, rss


def validate_load(text):
    data = json.loads(text)
    codes = data['RetCodes']
    if not isinstance(codes, dict) or not codes:
        raise ValueError('Fortio returned no request counters')
    if any(isinstance(v, bool) or not isinstance(v, int) or v < 0 for v in codes.values()):
        raise ValueError('Fortio returned invalid request counters')
    if codes.get('200', 0) <= 0 or any(str(k) != '200' and v != 0 for k, v in codes.items()):
        raise ValueError('Fortio returned request errors or no requests')
    return data


def fortio_args(target, qps, connections, duration):
    return ['kubectl', 'exec', '-n', 'bench', 'deploy/fortio-client', '-c', 'fortio', '--',
            'fortio', 'load', '-quiet', '-qps', str(qps), '-c', str(connections),
            '-t', f'{duration}s', '-payload-size', '1024', '-timeout', '120s',
            '-json', '/dev/stdout', target]


def collect_profile(args, qps, connections, run=command, sample=None, ticks=host_ticks,
                    launch=subprocess.Popen, interval=2):
    # No sampler exists during this discarded 10-second warmup.
    validate_load(run(fortio_args(args.target, qps, connections, 10), timeout=140))
    expected = expected_containers(args.namespace, args.container,
                                   [args.client_node, args.server_node], run)
    if sample is None:
        sample = lambda: proxy_metrics(args.namespace, args.container, expected, run)
    label = f'{args.mesh}-q{qps}-c{connections}'
    args.results.mkdir(parents=True, exist_ok=True)
    proxy_path = args.results / f'metrics-{label}.csv'
    host_path = args.results / f'metrics-{label}.host.csv'
    ready = threading.Event()
    stop = threading.Event()
    errors = []

    def sampler():
        try:
            previous = ticks()
            # Establish a genuine host delta before opening the measured window.
            stop.wait(0.1)
            with proxy_path.open('w') as pf, host_path.open('w') as hf:
                proxy = csv.writer(pf)
                host = csv.writer(hf)
                proxy.writerow(['timestamp', 'cpu_millicores', 'memory_rss_kb'])
                host.writerow(['timestamp', 'host_util_percent'])
                while True:
                    cpu, rss = sample()
                    current = ticks()
                    timestamp = time.time()
                    proxy.writerow([timestamp, number(cpu), number(rss, positive=True)])
                    host.writerow([timestamp, host_util(previous, current)])
                    pf.flush()
                    hf.flush()
                    previous = current
                    ready.set()
                    if stop.is_set():
                        return
                    stop.wait(interval)
                    # On stop, iterate once more: the final sample brackets load completion.
        except BaseException as error:
            errors.append(error)
            ready.set()

    thread = threading.Thread(target=sampler, daemon=True)
    process = None
    thread.start()
    try:
        if not ready.wait(70):
            raise TimeoutError('sampler readiness timed out')
        if errors:
            raise errors[0]
        # Redirect to files, not PIPEs: Fortio output cannot deadlock a full pipe.
        with (args.results / f'fortio-{label}.json').open('w') as out:
            process = launch(fortio_args(args.target, qps, connections, args.duration), stdout=out)
            deadline = time.monotonic() + args.duration + 140
            while process.poll() is None:
                if errors:
                    raise errors[0]
                if time.monotonic() >= deadline:
                    raise TimeoutError('measured Fortio timed out')
                stop.wait(0.1)
            if process.returncode:
                raise subprocess.CalledProcessError(process.returncode, process.args)
        validate_load((args.results / f'fortio-{label}.json').read_text())
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        stop.set()
        thread.join(timeout=70)
    if thread.is_alive():
        raise TimeoutError('sampler shutdown timed out')
    if errors:
        raise errors[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for field in ('mesh', 'namespace', 'container', 'target', 'client-node', 'server-node'):
        parser.add_argument('--' + field, required=True)
    parser.add_argument('--results', type=pathlib.Path, required=True)
    parser.add_argument('--duration', type=int, required=True)
    parser.add_argument('--profiles', required=True)
    args = parser.parse_args()
    if args.duration <= 0:
        parser.error('duration must be positive')
    for qps, connections in profiles(args.profiles):
        collect_profile(args, qps, connections)


if __name__ == '__main__':
    main()
