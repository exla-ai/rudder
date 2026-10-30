"""Generated fixtures only. Never reads an installed auth/profile/session store."""
import base64
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time

NATIVE = Path('/opt/rudder/dist/native/rudder-native')
EXPECTED_NATIVE = 'c08e44c5feb0fe9b2a4907e725cedd07cab5d2daf887622245e62f40fdfd2c85'
BASE = Path(sys.argv[1]).resolve()
PROBE = Path(__file__).with_name('probe-ts.mjs')
REPORT = {'kind': 'alpha9-compatibility-refusal', 'uid': os.geteuid(), 'cases': [],
          'limitations': ['manually generated schema5 fixture, not implemented activation migration',
                          'protocol refusal only; no physical revocation or provider qualification',
                          'no model, authenticated provider, active home or actual legacy task exercised']}


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(argv, cwd, env, terminal=False, timeout=8):
    started = time.monotonic()
    master = slave = None
    if terminal:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 120, 0, 0))
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=slave, stdout=slave,
                                 stderr=slave, start_new_session=True)
        os.close(slave)
        output = bytearray()
        try:
            while True:
                if time.monotonic() - started > timeout:
                    raise subprocess.TimeoutExpired(argv, timeout)
                ready, _, _ = select.select([master], [], [], .05)
                if ready:
                    try:
                        part = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not part:
                        break
                    output.extend(part)
                    assert len(output) <= 1024 * 1024
                elif child.poll() is not None:
                    break
            code = child.wait(timeout=1)
        except BaseException:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            child.wait()
            raise
        finally:
            os.close(master)
        text = output.decode('utf8', errors='replace')
    else:
        completed = subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   timeout=timeout, start_new_session=True)
        code, text = completed.returncode, completed.stdout.decode('utf8', errors='replace')
    return {'argv': [str(arg) for arg in argv], 'terminal': terminal, 'exit': code,
            'ms': round((time.monotonic() - started) * 1000, 3), 'output': text}


def snapshot(repo):
    return {str(path.relative_to(repo)): sha(path) for path in sorted(repo.rglob('*'))
            if path.is_file() and not path.is_symlink()
            and '.rudder/engine/owner.lock' not in str(path)}


assert BASE.parent == Path('/workspace') and BASE.name.startswith('alpha9-compat-')
assert sha(NATIVE) == EXPECTED_NATIVE, 'actual alpha9 native binary differs'
assert json.loads(Path('/opt/rudder/package.json').read_text())['version'] == '4.0.0-alpha.9'
REPORT.update(native={'path': str(NATIVE), 'sha256': sha(NATIVE)},
              packageVersion='4.0.0-alpha.9', kernel=os.uname().release,
              python=sys.version, sqlite=sqlite3.sqlite_version,
              moduleHashes={name: sha('/opt/rudder/dist/' + name + '.js')
                            for name in ['engine-authority', 'run-manager', 'scheduler', 'state']})
BASE.mkdir(mode=0o700, exist_ok=True)
try:
    for mode in ['fabric', 'malformed', 'unsupported', 'missing']:
        case = BASE / mode
        repo, home, os_home = case / 'repo', case / 'rudder-home', case / 'os-home'
        for directory in [repo, home / 'fabric', os_home, case / 'bin']:
            directory.mkdir(parents=True, mode=0o700)
        env = {'HOME': str(os_home), 'RUDDER_HOME': str(home), 'TERM': 'xterm-256color',
               'PATH': str(case / 'bin') + ':/usr/local/bin:/usr/bin:/bin',
               'LANG': 'C.UTF-8', 'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null',
               'NO_COLOR': '1', 'CI': '1'}
        subprocess.run(['/usr/local/bin/git', 'init', '-q', str(repo)], env=env, check=True)
        # Only inert fixture launchers appear on PATH; the production Codex
        # override points at one too. No actual model CLI is permitted to start.
        trap = case / 'backend-attempted'
        for program in ['claude', 'codex', 'opencode', 'rudder']:
            target = case / 'bin' / program
            target.write_text('#!/bin/sh\nprintf attempted >' + str(trap) + '\nexit 97\n')
            target.chmod(0o700)
        env['RUDDER_CODEX_BIN'] = str(case / 'bin/codex')
        run_dir = repo / '.rudder/runs/managed-fixture'
        run_dir.mkdir(parents=True, mode=0o700)
        marker = {'phase': 'reserved', 'fence': {'owner': {'project_id': 'fixture', 'owner_id': 'old', 'epoch': 1},
                  'run_id': 'managed-fixture', 'run_epoch': 1, 'attempt_id': '00000000-0000-0000-0000-000000000001',
                  'request_id': '00000000-0000-0000-0000-000000000002'},
                  'runnable_root': {'kind': 'runnable_root', 'algorithm': 'sha256', 'digest': '0' * 64, 'size_bytes': 1}}
        record = {'id': 'managed-fixture', 'task': 'inert managed source fixture', 'mode': 'execute',
                  'status': 'orphaned', 'backend': 'codex', 'model': 'fixture', 'repoRoot': str(repo),
                  'workspace': {'enabled': False, 'path': str(repo)}, 'vcs': 'fabric', 'managedLocal': marker}
        (run_dir / 'run.json').write_text(json.dumps(record, sort_keys=True) + '\n')
        (run_dir / 'managed-local.json').write_text(json.dumps(marker, sort_keys=True, separators=(',', ':')))
        owner = repo / '.rudder/engine/owner.json'
        owner.parent.mkdir(mode=0o700)
        if mode == 'fabric':
            owner.write_text(json.dumps({'format_version': 1, 'authority': 'fabric', 'state': 'stopped',
                                        'repo': str(repo), 'instance_id': 'old-stopped-fixture'}) + '\n')
        elif mode == 'malformed':
            owner.write_text('{malformed fixture\n')
        elif mode == 'unsupported':
            owner.write_text('{"format_version":2,"authority":"fabric"}\n')
        database = home / 'fabric/journal.sqlite'
        with sqlite3.connect(database) as db:
            db.execute('PRAGMA journal_mode=WAL')
            db.execute('PRAGMA synchronous=FULL')
            db.execute('CREATE TABLE fixture_sentinel(note TEXT NOT NULL)')
            db.execute("INSERT INTO fixture_sentinel VALUES('synthetic protocol5, no activation')")
            db.execute('PRAGMA user_version=5')
        database.chmod(0o600)
        before = snapshot(repo)
        native_results = []
        for argv, terminal in [([str(NATIVE), 'engine', 'serve', '--repo', str(repo), '--json'], False),
                               ([str(NATIVE)], True),
                               ([str(NATIVE), 'engine', 'serve', '--repo', str(repo)], False)]:
            result = run(argv, str(repo), env, terminal=terminal)
            assert result['exit'] != 0 and 'journal schema is newer than this engine' in result['output'], result
            native_results.append(result)
            assert snapshot(repo) == before, 'owner projection/run/history changed before schema refusal'
            assert not trap.exists(), 'a backend launcher was attempted'
        node = run(['/usr/local/bin/node', str(PROBE), str(repo), mode], str(repo), env)
        assert node['exit'] == 0, node
        ts_result = json.loads(node['output'].strip())
        assert snapshot(repo) == before, 'old TS guard mutated fixture metadata/history'
        assert not trap.exists()
        with sqlite3.connect(database) as db:
            assert db.execute('PRAGMA user_version').fetchone()[0] == 5
            assert db.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
            assert db.execute('SELECT * FROM fixture_sentinel').fetchall() == [('synthetic protocol5, no activation',)]
        REPORT['cases'].append({'mode': mode, 'native': native_results, 'ts': ts_result,
                                'ownerRunHistoryUnchanged': True, 'backendTrapAbsent': True,
                                'schemaVersion': 5, 'integrity': 'ok'})
    REPORT['ok'] = True
finally:
    shutil.rmtree(BASE)
    REPORT['fixtureAbsent'] = not BASE.exists()
    print(json.dumps(REPORT, sort_keys=True))
