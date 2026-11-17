#!/usr/bin/env python3
"""Build once, collect one matched cohort, validate, then replace the sole dataset."""
import datetime as dt
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import uuid

from capture_profile import REQUEST_TIMEOUT_SECONDS, workload_grids
from aggregate import HARNESS_INPUT_DIRS, HARNESS_ROOT_FILES, load_results, peak_metadata

BENCH = pathlib.Path(__file__).resolve().parents[1]
ROOT = BENCH.parent
MESHES = ('interlink', 'linkerd', 'istio-ambient')


def git(*args, root=ROOT):
    return subprocess.check_output(['git', '-C', str(root), *args], text=True, timeout=30).strip()


def runtime_matches(before, after, root=ROOT):
    if before == after:
        return True
    paths = ('src', 'Cargo.toml', 'Cargo.lock', 'Dockerfile')
    try:
        old = git('rev-parse', *(f'{before}:{path}' for path in paths), root=root)
        new = git('rev-parse', *(f'{after}:{path}' for path in paths), root=root)
    except subprocess.CalledProcessError as error:
        raise ValueError('shared-only capture requires the recorded runtime in local Git history') from error
    return old == new


def assert_source(revision, root=ROOT):
    if git('rev-parse', 'HEAD', root=root) != revision:
        raise RuntimeError('source HEAD changed during capture; refusing publication')
    # Generated results are not source. All other tracked/untracked changes are rejected.
    if git('status', '--porcelain', '--untracked-files=all', '--', '.', ':!bench/results', root=root):
        raise RuntimeError('capture requires clean committed source outside bench/results')


def source_fingerprint(root):
    """Archive captures must prove that the measured source tree did not change."""
    digest = hashlib.sha256()
    excluded = {'.git', 'target', 'tmp', 'bench/bin', 'bench/results'}
    for directory, dirs, files in os.walk(root):
        base = pathlib.Path(directory).relative_to(root)
        dirs[:] = sorted(d for d in dirs if (base / d).as_posix() not in excluded)
        for name in sorted(files):
            path = pathlib.Path(directory) / name
            relative = path.relative_to(root).as_posix()
            digest.update(relative.encode() + b'\0')
            digest.update(str(path.lstat().st_mode).encode() + b'\0')
            if path.is_symlink():
                digest.update(os.readlink(path).encode())
            else:
                with path.open('rb') as source:
                    for block in iter(lambda: source.read(1024 * 1024), b''):
                        digest.update(block)
            digest.update(b'\0')
    return digest.digest()


def harness_hashes(root):
    """Pin actual measurement inputs independently of the runtime source revision."""
    inputs = [root / relative for relative in HARNESS_ROOT_FILES]
    for relative in HARNESS_INPUT_DIRS:
        directory = root / relative
        if not directory.is_dir():
            raise ValueError(f'missing capture input directory: {relative}')
        inputs.extend(path for path in directory.rglob('*') if path.is_file()
                      and (relative != 'bench/scripts' or path.suffix in ('.py', '.sh')))
    hashes = {}
    for path in sorted(inputs):
        if not path.is_file() or path.is_symlink():
            raise ValueError(f'capture input is not a regular file: {path.relative_to(root)}')
        hashes[path.relative_to(root).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    return hashes


def source_identity(root=ROOT):
    explicit = os.environ.get('BENCH_REVISION')
    if explicit is not None and not re.fullmatch('[0-9a-f]{40}', explicit):
        raise ValueError('BENCH_REVISION must be the full lowercase source git SHA')
    if (root / '.git').exists():
        revision = git('rev-parse', 'HEAD', root=root)
        if explicit is not None and explicit != revision:
            raise RuntimeError('BENCH_REVISION does not match source HEAD')
        guard = lambda value: assert_source(value, root=root)
    else:
        if explicit is None:
            raise RuntimeError('source archive requires explicit full BENCH_REVISION provenance')
        revision = explicit
        fingerprint = source_fingerprint(root)

        def guard(value):
            if value != revision or os.environ.get('BENCH_REVISION') != revision:
                raise RuntimeError('archive source revision changed during capture')
            if source_fingerprint(root) != fingerprint:
                raise RuntimeError('archive source changed during capture; refusing publication')
    guard(revision)
    return revision, guard


def publish_results(staged, destination, revision, guard=assert_source, readme=None):
    # Validate source before touching either consumer. The generated README remains
    # staged until this point, so it never looks like a source edit to either guard.
    guard(revision)
    outputs = [(staged, destination, staged.parent / 'previous-results')]
    if readme is not None:
        pending, published = readme
        outputs.append((pending, published, staged.parent / 'previous-README.md'))
    moved = []
    try:
        for pending, published, backup in outputs:
            had_previous = published.exists()
            if had_previous:
                published.rename(backup)
            try:
                pending.rename(published)
            except BaseException:
                if had_previous:
                    backup.rename(published)
                raise
            moved.append((pending, published, backup, had_previous))
    except BaseException:
        for pending, published, backup, had_previous in reversed(moved):
            published.rename(pending)
            if had_previous:
                backup.rename(published)
        raise
    for _, _, backup, had_previous in moved:
        if had_previous:
            if backup.is_dir():
                shutil.rmtree(backup)
            else:
                backup.unlink()


def timestamp():
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec='seconds').replace('+00:00', 'Z')


def run_script(script, env, log, timeout):
    process = subprocess.Popen(['bash', str(BENCH / script)], env=env, stdout=log,
                               stderr=subprocess.STDOUT, start_new_session=True)
    try:
        status = process.wait(timeout=timeout)
        if status:
            raise subprocess.CalledProcessError(status, process.args)
    except BaseException:
        # Shell setup pipelines must not outlive a timed-out or interrupted capture.
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
        raise


def cleanup(env, log):
    errors = []
    try:
        run_script('teardown.sh', env, log, 540)
    except Exception as error:
        errors.append(error)
    # Attempt every resource even if another deletion failed. An absent image is
    # normal when setup failed before its build, but a Docker query failure is not.
    for image in ('echo-server', 'interlinkd'):
        name = f'interlink-bench/{image}:{env["BENCH_IMAGE_TAG"]}'
        try:
            existing = subprocess.run(['docker', 'image', 'ls', '--quiet', name],
                                      env=env, capture_output=True, text=True,
                                      check=True, timeout=30)
            if existing.stdout.strip():
                subprocess.run(['docker', 'image', 'rm', '-f', name], env=env,
                               stdout=log, stderr=subprocess.STDOUT, check=True, timeout=60)
        except Exception as error:
            errors.append(error)
    if errors:
        raise RuntimeError('capture resource cleanup failed; refusing publication') from errors[0]


def capture(shared_only=False):
    os.umask(0o077)
    revision, guard = source_identity()
    inputs = harness_hashes(ROOT)
    selected = workload_grids(os.environ.get('BENCH_RPS_CONCURRENCY', '1,4,16,64,256,1024,4096'),
                              os.environ.get('BENCH_BULK_CONCURRENCY', '1,4,16,64,256,1024'))
    duration = int(os.environ.get('BENCH_DURATION', '20'))
    if duration <= 0:
        raise ValueError('BENCH_DURATION must be a positive number of seconds')
    target = int(os.environ.get('BENCH_SHARED_RPS', '10000'))
    clients = int(os.environ.get('BENCH_SHARED_CONCURRENCY', '64'))
    if target <= 0 or clients <= 0:
        raise ValueError('shared load and concurrency must be positive')
    versions = {name: os.environ[key] for name, key in (
        ('kind', 'KIND_VERSION'), ('kubernetes', 'KUBECTL_VERSION'),
        ('linkerd', 'LINKERD_VERSION'), ('istio', 'ISTIO_VERSION'),
        ('fortio', 'FORTIO_VERSION'))}
    machine = dict(system=os.uname().sysname, architecture=os.uname().machine,
                   kernel=os.uname().release, logical_cpus=os.cpu_count(),
                   memory_bytes=os.sysconf('SC_PAGE_SIZE') * os.sysconf('SC_PHYS_PAGES'))
    baseline = None
    if shared_only:
        baseline, _ = load_results(BENCH / 'results')
        if (not runtime_matches(baseline['revision'], revision, root=ROOT)
                or baseline['host'] != machine or baseline['versions'] != versions):
            raise ValueError('shared-only capture needs the same runtime, machine and tool versions')
        stable_inputs = lambda values: {name: value for name, value in values.items()
                                       if name in ('Dockerfile', 'Cargo.toml', 'Cargo.lock')
                                       or name.startswith(('bench/workloads/', 'bench/manifests/'))}
        if stable_inputs(baseline['harness_sha256']) != stable_inputs(inputs):
            raise ValueError('shared-only capture needs unchanged workloads and deployment settings')
    scratch = ROOT / 'tmp'
    scratch.mkdir(exist_ok=True)
    # One producer owns shared tool paths and canonical publication at a time.
    with (scratch / 'mesh-capture.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with tempfile.TemporaryDirectory(prefix='mesh-capture-', dir=scratch) as temporary:
            stage = pathlib.Path(temporary)
            data = stage / 'data'
            private = stage / 'private'
            data.mkdir()
            if baseline is not None:
                shutil.copytree(BENCH / 'results', data, dirs_exist_ok=True)
                previous = baseline.pop('shared_load', None)
                if previous is not None:
                    for mesh in MESHES:
                        label = f'{mesh}-shared-c{previous["concurrency"]}'
                        for name in (f'fortio-{label}.json', f'metrics-{label}.csv', f'metrics-{label}.host.csv'):
                            (data / mesh / name).unlink()
            private.mkdir(mode=0o700)
            run_id = uuid.uuid4().hex
            env = os.environ.copy()
            env.update(BENCH_CAPTURE_DIR=str(data), BENCH_PRIVATE_DIR=str(private),
                       BENCH_IMAGE_TAG=f'{revision[:12]}-{run_id}',
                       BENCH_CLUSTER_PREFIX=f'bench-{run_id[:12]}',
                       BENCH_SHARED_RPS=str(target), BENCH_SHARED_CONCURRENCY=str(clients),
                       BENCH_SHARED_ONLY='1' if shared_only else '0',
                       KUBECONFIG=str(private / 'kubeconfig'), PYTHONDONTWRITEBYTECODE='1')
            # kubectl never waits indefinitely for a broken apiserver.
            env['PATH'] = str(BENCH / 'bin') + os.pathsep + env['PATH']
            with (private / 'capture.log').open('w+') as log:
                try:
                    try:
                        run_script('setup.sh', env, log, 3600)
                        started = timestamp()
                        for mesh, script, suffix in (
                            ('interlink', 'run-interlink.sh', 'interlink'),
                            ('linkerd', 'run-linkerd.sh', 'linkerd'),
                            ('istio-ambient', 'run-istio.sh', 'istio'),
                        ):
                            print(f'Capturing {mesh}', flush=True)
                            count = 1 if shared_only else 1 + sum(len(w['concurrency']) for w in selected)
                            run_script('scripts/' + script, env, log, 1200 + count * (duration + 300))
                            subprocess.run(['kind', 'delete', 'cluster', '--name',
                                            env['BENCH_CLUSTER_PREFIX'] + '-' + suffix],
                                           env=env, stdout=log, stderr=subprocess.STDOUT,
                                           check=True, timeout=180)
                        completed = timestamp()
                    finally:
                        # Also removes partially-created clusters after any setup/load failure.
                        cleanup(env, log)
                    metadata = baseline if baseline is not None else dict(
                        schema_version=2, benchmark='echo-capacity', revision=revision,
                        run_id=run_id, harness_sha256=inputs,
                        started_at=started, completed_at=completed,
                        duration_seconds=duration, warmup_seconds=10,
                        request_timeout_seconds=REQUEST_TIMEOUT_SECONDS,
                        server_delay_ms=0, workloads=selected, meshes=list(MESHES),
                        versions=versions, host=machine)
                    metadata['shared_load'] = dict(
                        target_rps=target, concurrency=clients, run_id=run_id,
                        started_at=started, completed_at=completed,
                        duration_seconds=duration, warmup_seconds=10,
                        request_timeout_seconds=REQUEST_TIMEOUT_SECONDS, server_delay_ms=0,
                        revision=metadata['revision'], host=machine, versions=versions, harness_sha256=inputs)
                    (data / 'capture.json').write_text(json.dumps(metadata, indent=2) + '\n')
                    _, points = load_results(data)
                    metadata['peaks'] = peak_metadata(points)
                    (data / 'capture.json').write_text(json.dumps(metadata, indent=2) + '\n')
                    staged_readme = stage / 'README.md'
                    shutil.copy2(ROOT / 'README.md', staged_readme)
                    subprocess.run([sys.executable, str(BENCH / 'scripts/aggregate.py'),
                                    '--results', str(data), '--output', str(data / 'comparison.md'),
                                    '--readme', str(staged_readme)],
                                   check=True, timeout=60)
                    publish_results(data, BENCH / 'results', revision, guard=guard,
                                    readme=(staged_readme, ROOT / 'README.md'))
                except BaseException:
                    if data.exists() and any(data.iterdir()):
                        rejected = scratch / f'rejected-capture-{run_id}'
                        data.rename(rejected)
                        print(f'Unpublished raw evidence retained privately: {rejected}', file=sys.stderr)
                    # Operational output stays private and is deleted, never copied into evidence.
                    print('Capture failed; no results published. Private diagnostic tail:', file=sys.stderr)
                    log.flush()
                    log.seek(0)
                    print(''.join(log.readlines()[-60:]), file=sys.stderr)
                    raise
    print('Published bench/results/comparison.md and README headline', flush=True)


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'capture interrupted by signal {signum}')


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    if len(sys.argv) == 1:
        capture()
    elif sys.argv[1:] == ['--shared-only']:
        capture(shared_only=True)
    else:
        raise SystemExit('usage: run_capture.py [--shared-only]')
