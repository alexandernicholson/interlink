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

from capture_profile import profiles

BENCH = pathlib.Path(__file__).resolve().parents[1]
ROOT = BENCH.parent
MESHES = ('interlink', 'linkerd', 'istio-ambient')


def git(*args, root=ROOT):
    return subprocess.check_output(['git', '-C', str(root), *args], text=True, timeout=30).strip()


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


def assert_remote_revision(revision, remote='origin', branch='main', root=ROOT):
    line = git('ls-remote', '--exit-code', remote, f'refs/heads/{branch}', root=root)
    if line.split()[0] != revision:
        raise RuntimeError('remote source advanced; refusing stale benchmark publication')


def publish_results(staged, destination, revision, guard=assert_source):
    # No old profile can survive: move the entire validated cohort, never merge directories.
    guard(revision)
    backup = staged.parent / 'previous-results'
    had_previous = destination.exists()
    try:
        if had_previous:
            destination.rename(backup)
        staged.rename(destination)
    except BaseException:
        if backup.exists() and not destination.exists():
            backup.rename(destination)
        raise
    if had_previous:
        shutil.rmtree(backup)


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


def capture():
    os.umask(0o077)
    revision, guard = source_identity()
    selected = profiles(os.environ.get('BENCH_PROFILES', '320:80,800:200,1600:400'))
    duration = int(os.environ.get('BENCH_DURATION', '60'))
    if duration <= 0:
        raise ValueError('BENCH_DURATION must be a positive number of seconds')
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
            private.mkdir(mode=0o700)
            run_id = uuid.uuid4().hex
            env = os.environ.copy()
            env.update(BENCH_CAPTURE_DIR=str(data), BENCH_PRIVATE_DIR=str(private),
                       BENCH_IMAGE_TAG=f'{revision[:12]}-{run_id}',
                       BENCH_CLUSTER_PREFIX=f'bench-{run_id[:12]}',
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
                            run_script('scripts/' + script, env, log,
                                       1200 + len(selected) * (duration + 300))
                            subprocess.run(['kind', 'delete', 'cluster', '--name',
                                            env['BENCH_CLUSTER_PREFIX'] + '-' + suffix],
                                           env=env, stdout=log, stderr=subprocess.STDOUT,
                                           check=True, timeout=180)
                        completed = timestamp()
                    finally:
                        # Also removes partially-created clusters after any setup/load failure.
                        cleanup(env, log)
                    versions = {name: os.environ[key] for name, key in (
                        ('kind', 'KIND_VERSION'), ('kubernetes', 'KUBECTL_VERSION'),
                        ('linkerd', 'LINKERD_VERSION'), ('istio', 'ISTIO_VERSION'),
                        ('fortio', 'FORTIO_VERSION'))}
                    metadata = dict(schema_version=1, revision=revision, run_id=run_id,
                                    started_at=started, completed_at=completed,
                                    duration_seconds=duration, warmup_seconds=10,
                                    payload_bytes=1024, server_delay_ms=200,
                                    profiles=[dict(qps=q, connections=c) for q, c in selected],
                                    meshes=list(MESHES), versions=versions,
                                    host=dict(system=os.uname().sysname,
                                              architecture=os.uname().machine,
                                              kernel=os.uname().release,
                                              logical_cpus=os.cpu_count(),
                                              memory_bytes=os.sysconf('SC_PAGE_SIZE') *
                                              os.sysconf('SC_PHYS_PAGES')))
                    (data / 'capture.json').write_text(json.dumps(metadata, indent=2) + '\n')
                    subprocess.run([sys.executable, str(BENCH / 'scripts/aggregate.py'),
                                    '--results', str(data), '--output', str(data / 'comparison.md')],
                                   check=True, timeout=60)
                    publish_results(data, BENCH / 'results', revision, guard=guard)
                except BaseException:
                    # Operational output stays private and is deleted, never copied into evidence.
                    print('Capture failed; no results published. Private diagnostic tail:', file=sys.stderr)
                    log.flush()
                    log.seek(0)
                    print(''.join(log.readlines()[-60:]), file=sys.stderr)
                    raise
    print('Published bench/results/comparison.md', flush=True)


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'capture interrupted by signal {signum}')


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    if len(sys.argv) == 3 and sys.argv[1] == '--check-remote':
        assert_remote_revision(sys.argv[2])
    elif len(sys.argv) == 1:
        capture()
    else:
        raise SystemExit('usage: run_capture.py [--check-remote REVISION]')
