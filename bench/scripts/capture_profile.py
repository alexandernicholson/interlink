#!/usr/bin/env python3
"""Collect actual kubelet RSS/CPU and Linux host utilization around measured Fortio.

CPU comes from the summary API; RSS comes from timestamped cAdvisor cgroup
metrics. Summary rssBytes is optional (the CRI backend can omit it). Match the
API's pod UID and exact container ID, not optional cAdvisor runtime labels.
The sampler starts only after successful discarded warmup, takes a ready sample
before measured load starts, and takes its final sample after load exits. Missing
containers, stale/non-finite metrics and commands without valid results abort.
"""
import argparse
import base64
import csv
import datetime as dt
import hashlib
import json
import math
import pathlib
import re
import subprocess
import threading
import time

REQUEST_TIMEOUT_SECONDS = 5


def command(args, timeout=30):
    return subprocess.run(args, check=True, capture_output=True, text=True, timeout=timeout).stdout


def concurrency_grid(spec):
    values = spec.split(',')
    if not values or any(not value.isdigit() or int(value) <= 0 for value in values):
        raise ValueError('concurrency must be comma-separated positive integers')
    result = [int(value) for value in values]
    if len(set(result)) != len(result):
        raise ValueError('duplicate concurrency')
    return result


def workload_grids(rps, bulk):
    return [{'name': 'rps', 'payload_bytes': 1024, 'concurrency': concurrency_grid(rps)},
            {'name': 'bulk', 'payload_bytes': 1_048_576, 'concurrency': concurrency_grid(bulk)}]


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
        # This benchmark pins containerd kind nodes. Keep the running instance's
        # ID so a restarted container cannot silently replace its RSS series.
        match = re.fullmatch(r'containerd://([0-9a-f]{64})', status.get('containerID', ''))
        if match is None:
            raise ValueError('missing or invalid containerd container ID')
        expected[pod['metadata']['uid']] = (pod['spec']['nodeName'], match[1])
    if not expected or not set(nodes).issubset({node for node, _ in expected.values()}):
        raise ValueError('missing proxy on a workload worker')
    return expected


def cadvisor_rss(text, expected, node, observed_at):
    """Read actual cgroup RSS, including raw cgroups without runtime labels.

    Kubernetes v1.30 server.go exposes cAdvisor Memory.RSS with its sample
    timestamp at /metrics/cadvisor. cAdvisor libcontainer/handler.go populates
    it from memory.stat anon (v2) or total_rss (v1), not working set:
    https://github.com/google/cadvisor/blob/v0.49.0/container/libcontainer/handler.go
    """
    seen = set()
    rss = 0
    for line in text.splitlines():
        if not line.startswith('container_memory_rss{'):
            continue
        sample = re.fullmatch(r'container_memory_rss\{(.*)\}\s+(\S+)\s+(\S+)', line)
        if sample is None:
            raise ValueError('missing or invalid cAdvisor RSS timestamp')
        labels = {}
        remaining = sample[1]
        while remaining:
            label = re.match(r'([a-zA-Z_][a-zA-Z0-9_]*)=("(?:[^"\\]|\\["\\n])*")(,|$)', remaining)
            if label is None or label[1] in labels:
                raise ValueError('invalid cAdvisor labels')
            labels[label[1]] = json.loads(label[2])
            remaining = remaining[label.end():]
        path = labels.get('id', '').split('/')
        for uid, (expected_node, container_id) in expected.items():
            if not path or path[-1] not in (container_id, f'cri-containerd-{container_id}.scope'):
                continue
            # systemd escapes UID hyphens as underscores; cgroupfs does not.
            pod = re.compile(r'(?:^|-)pod' + re.escape(uid.replace('-', '_')) + r'\.slice$')
            if not any(part == f'pod{uid}' or pod.search(part) for part in path[:-1]):
                raise ValueError('cAdvisor proxy UID mismatch')
            if node != expected_node:
                raise ValueError('proxy moved during measurement')
            if uid in seen:
                raise ValueError('duplicate proxy RSS metric')
            # Prometheus text timestamps are milliseconds since the Unix epoch.
            stamp = number(float(sample[3])) / 1000
            if not 0 <= observed_at - stamp <= 30:
                raise ValueError('stale cAdvisor RSS metric')
            rss += number(float(sample[2]), positive=True) / 1024
            seen.add(uid)
    if seen != {uid for uid, (expected_node, _) in expected.items() if expected_node == node}:
        raise ValueError('missing proxy RSS metrics')
    return rss


def proxy_metrics(namespace, container, expected, run=command, now=None):
    cpu = rss = 0
    seen = set()
    for node in sorted({node for node, _ in expected.values()}):
        summary = json.loads(run(['kubectl', 'get', '--raw', f'/api/v1/nodes/{node}/proxy/stats/summary']))
        observed_at = time.time() if now is None else now
        for pod in summary['pods']:
            uid = pod['podRef']['uid']
            if uid not in expected or pod['podRef']['namespace'] != namespace:
                continue
            if expected[uid][0] != node:
                raise ValueError('proxy moved during measurement')
            matches = [c for c in pod['containers'] if c['name'] == container]
            if len(matches) != 1 or uid in seen:
                raise ValueError('duplicate or missing proxy container')
            metric = matches[0]
            stamp = dt.datetime.fromisoformat(metric['cpu']['time'].replace('Z', '+00:00')).timestamp()
            if not 0 <= observed_at - stamp <= 30:
                raise ValueError('stale kubelet metric')
            cpu += number(metric['cpu']['usageNanoCores']) / 1_000_000
            seen.add(uid)
        text = run(['kubectl', 'get', '--raw', f'/api/v1/nodes/{node}/proxy/metrics/cadvisor'])
        observed_at = time.time() if now is None else now
        rss += cadvisor_rss(text, expected, node, observed_at)
    if seen != set(expected):
        raise ValueError('missing proxy metrics')
    return cpu, rss


def validate_load(text, payload_bytes, connections, duration, requested_qps=0):
    """Keep real capacity error boundaries; require error-free matched-load controls."""
    if type(requested_qps) is not int or requested_qps < 0:
        raise ValueError('requested QPS must be a nonnegative integer')
    data = json.loads(text)
    codes = data['RetCodes']
    if not isinstance(codes, dict) or not codes:
        raise ValueError('Fortio returned no request counters')
    if any(not re.fullmatch(r'-?\d+', code) or type(count) is not int or count < 0
           for code, count in codes.items()):
        raise ValueError('Fortio returned invalid request counters')
    count = sum(codes.values())
    if count <= 0 or data['DurationHistogram']['Count'] != count:
        raise ValueError('Fortio returned no requests or inconsistent latency counts')
    expected_qps = str(requested_qps) if requested_qps else 'max'
    if data['RequestedQPS'] != expected_qps or data['NumThreads'] != connections:
        raise ValueError('Fortio load differs from the requested rate or concurrency')
    standard_client = payload_bytes == 1_048_576
    if data['DisableFastClient'] is not standard_client:
        raise ValueError('Fortio client differs from the workload protocol')
    if data['HTTPReqTimeOut'] != REQUEST_TIMEOUT_SECONDS * 1_000_000_000:
        raise ValueError('Fortio request timeout differs from the declared experiment')
    seconds = number(data['ActualDuration'], positive=True) / 1e9
    # Fortio includes the final in-flight request drain in elapsed time.
    if seconds < duration * 0.95:
        raise ValueError('Fortio measured duration is shorter than the requested duration')
    achieved = number(data['ActualQPS'], positive=True)
    if not math.isclose(achieved, count / seconds, rel_tol=0.001):
        raise ValueError('Fortio request rate and counts disagree')
    if requested_qps and abs(achieved - requested_qps) > requested_qps * 0.03:
        raise ValueError(f'Fortio achieved {achieved} QPS, outside 3% of target {requested_qps}')
    stamp = dt.datetime.fromisoformat(data['StartTime'].replace('Z', '+00:00'))
    if stamp.tzinfo is None:
        raise ValueError('Fortio start time needs a timezone')
    payload = base64.b64decode(data['Payload'], validate=True)
    if len(payload) != payload_bytes:
        raise ValueError('Fortio request payload differs from the workload')
    error_count = request_errors(codes)
    for field in ('Sizes', 'HeaderSizes'):
        histogram = data[field]
        allowed_counts = (0, count) if standard_client and field == 'HeaderSizes' else (count,)
        if type(histogram['Count']) is not int or histogram['Count'] not in allowed_counts:
            raise ValueError('Fortio response size counts differ from requests')
        # The standard client explicitly returns size=-1 on a failed request.
        # Retain that real error-boundary evidence, never publish it as throughput.
        minimum = -1 if standard_client and field == 'Sizes' and error_count else 0
        for key in ('Min', 'Max', 'Sum'):
            value = histogram[key]
            floor = -error_count if key == 'Sum' and minimum < 0 else minimum
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < floor:
                raise ValueError('invalid Fortio response size')
        if histogram['Max'] < histogram['Min']:
            raise ValueError('Fortio response size range is invalid')
        if standard_client and field == 'HeaderSizes' and any(histogram[key] != 0 for key in ('Min', 'Max', 'Sum')):
            raise ValueError('standard-client response sizes must exclude headers')
    if not standard_client and data['Sizes']['Sum'] < data['HeaderSizes']['Sum']:
        raise ValueError('Fortio response size is smaller than its headers')
    if not error_count:
        body_bytes = data['Sizes']['Sum'] - data['HeaderSizes']['Sum']
        if not math.isclose(body_bytes, count * payload_bytes, rel_tol=1e-12, abs_tol=0.01):
            raise ValueError('Fortio echoed response payload differs from the request')
    if requested_qps and error_count:
        raise ValueError(f'Fortio matched-load control returned request errors: {codes}')
    data['PayloadBytes'] = len(payload)
    data['PayloadSHA256'] = hashlib.sha256(payload).hexdigest()
    del data['Payload']
    return data


def request_errors(codes):
    return sum(count for code, count in codes.items() if code != '200')


def fortio_args(target, payload_bytes, connections, duration, requested_qps=0):
    if type(requested_qps) is not int or requested_qps < 0:
        raise ValueError('requested QPS must be a nonnegative integer')
    args = ['kubectl', 'exec', '-n', 'bench', 'deploy/fortio-client', '-c', 'fortio', '--',
            'fortio', 'load', '-quiet', '-qps', str(requested_qps), '-c', str(connections),
            '-t', f'{duration}s', '-payload-size', str(payload_bytes),
            '-timeout', f'{REQUEST_TIMEOUT_SECONDS}s',
            '-allow-initial-errors', '-json', '/dev/stdout']
    if payload_bytes == 1_048_576:
        # Streaming HTTP/1 echo requires a client that reads responses while its
        # request writer can still be active. Fortio's fast client writes first.
        # Fortio otherwise silently caps generated payloads at 256 KiB.
        args.extend(['-stdclient', '-maxpayloadsizekb', '1024'])
    return args + [target]


def collect_point(args, workload, connections, run=command, sample=None, ticks=host_ticks,
                  launch=subprocess.Popen, interval=2, *, requested_qps=0):
    payload_bytes = workload['payload_bytes']
    # No sampler exists during discarded warmup. Capacity errors remain boundary
    # evidence; a matched-load control must be error-free and achieve its target.
    try:
        warmup_text = run(fortio_args(args.target, payload_bytes, connections, 10, requested_qps), timeout=140)
    except subprocess.CalledProcessError as error:
        if not error.stdout:
            raise
        try:
            warmup = validate_load(error.stdout, payload_bytes, connections, 10, requested_qps)
        except (ValueError, KeyError, TypeError):
            raise error
        if not request_errors(warmup['RetCodes']):
            raise
    else:
        warmup = validate_load(warmup_text, payload_bytes, connections, 10, requested_qps)
    expected = expected_containers(args.namespace, args.container,
                                   [args.client_node, args.server_node], run)
    if sample is None:
        def sample():
            current = expected_containers(args.namespace, args.container,
                                          [args.client_node, args.server_node], run)
            if current != expected:
                raise ValueError('proxy moved or restarted during measurement')
            return proxy_metrics(args.namespace, args.container, expected, run)
    label = f'{args.mesh}-{workload["name"]}-c{connections}'
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
            process = launch(fortio_args(args.target, payload_bytes, connections, args.duration, requested_qps), stdout=out)
            deadline = time.monotonic() + args.duration + 140
            while process.poll() is None:
                if errors:
                    raise errors[0]
                if time.monotonic() >= deadline:
                    raise TimeoutError('measured Fortio timed out')
                stop.wait(0.1)
        result_path = args.results / f'fortio-{label}.json'
        measured = validate_load(result_path.read_text(), payload_bytes, connections, args.duration, requested_qps)
        if process.returncode and not request_errors(measured['RetCodes']):
            raise subprocess.CalledProcessError(process.returncode, process.args)
        measured['WarmupRetCodes'] = warmup['RetCodes']
        result_path.write_text(json.dumps(measured, indent=2) + '\n')
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
    parser.add_argument('--rps-concurrency', required=True)
    parser.add_argument('--bulk-concurrency', required=True)
    parser.add_argument('--shared-rps', type=int, default=10000)
    parser.add_argument('--shared-concurrency', type=int, default=64)
    parser.add_argument('--shared-only', action='store_true')
    args = parser.parse_args()
    if args.duration <= 0 or args.shared_rps <= 0 or args.shared_concurrency <= 0:
        parser.error('duration, shared RPS and shared concurrency must be positive integers')
    try:
        workloads = workload_grids(args.rps_concurrency, args.bulk_concurrency)
    except ValueError as error:
        parser.error(str(error))
    # Per-mesh runners create fresh containers before invoking this collector.
    # Collect the shared control before capacity can raise their RSS high-water marks.
    collect_point(args, {'name': 'shared', 'payload_bytes': 1024}, args.shared_concurrency,
                  requested_qps=args.shared_rps)
    if not args.shared_only:
        for workload in workloads:
            for connections in workload['concurrency']:
                collect_point(args, workload, connections)


if __name__ == '__main__':
    main()
