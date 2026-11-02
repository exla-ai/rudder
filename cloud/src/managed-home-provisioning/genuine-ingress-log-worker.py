"""Disabled-bridge worker; ONLY its Node original holder can start this process.

Fixed protocol carries fresh live/refused samples, never a timeout/deadline.
A refused sample is absorbing. Only original stream drain/wait may continue.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import stat
import subprocess
import sys
import threading

LIMIT = 8 * 1024 * 1024
FLY = '/workspace/home/.fly/bin/flyctl'
MAX_REQUESTS = 65536
OPERATIONS = frozenset(('directory_lstat', 'directory_open', 'directory_fstat', 'stdout_open', 'stderr_open',
                        'selector_allocate', 'logs_spawn', 'pidfd_open', 'stdout_nonblocking', 'stderr_nonblocking',
                        'stdout_register', 'stderr_register', 'select',
                        'stdout_read', 'stderr_read', 'stdout_write', 'stderr_write', 'child_wait',
                        'stdout_fsync', 'stderr_fsync', 'directory_fsync'))
HELD = []

class Refusal(ValueError):
    pass

def need(value, label):
    if not value:
        raise Refusal(label)

def exact(value, keys):
    need(type(value) is dict and set(value) == set(keys), 'PROTOCOL_SCHEMA')

def pairs(rows):
    result = {}
    for key, value in rows:
        need(key not in result, 'PROTOCOL_DUPLICATE')
        result[key] = value
    return result

def emit(value):
    sys.stdout.write(json.dumps(value, separators=(',', ':'), ensure_ascii=True) + '\n')
    sys.stdout.flush()

def receive(maximum):
    raw = sys.stdin.buffer.readline(maximum + 1)
    need(0 < len(raw) <= maximum and raw.endswith(b'\n'), 'PROTOCOL_BOUND')
    try:
        return json.loads(raw.decode('utf-8', 'strict'), object_pairs_hook=pairs)
    except (UnicodeError, ValueError):
        raise Refusal('PROTOCOL_ENCODING') from None

class OriginalLogs:
    def __init__(self, nonce):
        self.child = subprocess.Popen.__new__(subprocess.Popen)
        self.unknown = threading.Event()
        self.nonce = nonce
        self.requests = 0
        self.expired = False
        self.spawn_entered = False
        self.constructor_completed = False
        self.actual_child_wait = False
        self.exit_code = None
        self.pidfd = None
        self.selector = None
        self.directory_fd = None
        self.pipes = {}
        self.raw_fds = {}
        self.hashes = {n: hashlib.sha256() for n in ('stdout', 'stderr')}
        self.bytes = dict(stdout=0, stderr=0)
        self.stored = dict(stdout=0, stderr=0)
        self.parts = dict(stdout=[], stderr=[])
        self.eof = dict(stdout=False, stderr=False)
        self.fsynced = dict(stdout=False, stderr=False)
        self.directory_fsynced = False
        self.errors = []
        self.overflow = False

    def latch(self, label):
        if label not in self.errors:
            self.errors.append(label)

    def hold_unknown(self, label):
        self.latch(label)
        # Same original Popen/event/selector/pipes/pidfd/files remain rooted.
        while True:
            try:
                self.unknown.wait()
            except BaseException:
                self.latch('unknown_hold_interrupted')

    def sample(self, operation, settlement=False):
        need(operation in OPERATIONS and self.requests < MAX_REQUESTS, 'PROTOCOL_REQUEST_BOUND')
        self.requests += 1
        emit({'schema': 1, 'type': 'budget', 'experiment_nonce': self.nonce,
              'sequence': self.requests, 'operation': operation})
        reply = receive(512)
        exact(reply, ('schema', 'type', 'experiment_nonce', 'sequence', 'live'))
        need(type(reply['schema']) is int and reply['schema'] == 1 and reply['type'] == 'budget_reply'
             and reply['experiment_nonce'] == self.nonce and type(reply['sequence']) is int
             and reply['sequence'] == self.requests and type(reply['live']) is bool, 'PROTOCOL_IDENTITY')
        if not reply['live']:
            self.expired = True
        # A later positive reply cannot undo refusal or authorize a new effect.
        if self.expired and not settlement:
            raise Refusal('ORIGINAL_BUDGET_REFUSED')
        return not self.expired

    def observation(self):
        return {'schema': 2, 'constructor_completed': self.constructor_completed,
                'actual_child_wait': self.actual_child_wait, 'exit_code': self.exit_code,
                'pidfd_held': self.pidfd is not None, 'stdout_eof': self.eof['stdout'],
                'stderr_eof': self.eof['stderr'], 'errors': list(self.errors), 'expired': self.expired,
                'overflow': self.overflow, 'original_budget_requests': self.requests,
                'original_budget_refused': self.expired,
                'raw_files_fsynced': all(self.fsynced.values()), 'raw_directory_fsynced': self.directory_fsynced,
                'stdout_bytes': self.bytes['stdout'], 'stderr_bytes': self.bytes['stderr'],
                'stdout_sha256': self.hashes['stdout'].hexdigest(), 'stderr_sha256': self.hashes['stderr'].hexdigest(),
                'stdout_stored_bytes': self.stored['stdout'], 'stderr_stored_bytes': self.stored['stderr']}

    def capture(self, argv, output):
        # The Node original holder sampled before worker spawn; every explicit
        # logs application effect/read below consumes its own new live sample.
        self.sample('directory_lstat');meta = Path(output).lstat()
        need(stat.S_ISDIR(meta.st_mode) and meta.st_uid == os.geteuid()
             and stat.S_IMODE(meta.st_mode) == 0o700, 'PRIVATE_OUTPUT_DIRECTORY')
        self.sample('directory_open')
        self.directory_fd = os.open(output, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        self.sample('directory_fstat');actual = os.fstat(self.directory_fd)
        need((meta.st_dev, meta.st_ino) == (actual.st_dev, actual.st_ino), 'PRIVATE_DIRECTORY_CHANGED')
        self.sample('selector_allocate');self.selector = selectors.DefaultSelector()  # before logs constructor effect
        for name in ('stdout', 'stderr'):
            self.sample(name + '_open')
            self.raw_fds[name] = os.open(name + '.private', os.O_WRONLY | os.O_CREAT | os.O_EXCL
                                         | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=self.directory_fd)
        self.sample('logs_spawn');self.spawn_entered = True
        try:
            subprocess.Popen.__init__(self.child, argv, stdin=subprocess.DEVNULL,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, close_fds=True)
            self.constructor_completed = True
        except BaseException:
            self.latch('constructor')
            if type(getattr(self.child, 'pid', None)) is not int:
                self.hold_unknown('constructor_without_public_pid')
        if type(getattr(self.child, 'pid', None)) is not int:
            self.hold_unknown('original_pid_unavailable')
        try:
            self.sample('pidfd_open');self.pidfd = os.pidfd_open(self.child.pid, 0)
        except BaseException:
            self.latch('pidfd')  # settle same process/streams, never accept without actual pidfd
        for name in ('stdout', 'stderr'):
            stream = getattr(self.child, name, None);self.pipes[name] = stream
            if stream is None:
                self.hold_unknown('original_pipe_unavailable')
            try:
                self.sample(name + '_nonblocking', settlement=True)
                os.set_blocking(stream.fileno(), False)
                self.sample(name + '_register', settlement=True)
                self.selector.register(stream, selectors.EVENT_READ, name)
            except BaseException:
                self.hold_unknown('original_pipe_observer')
        while not all(self.eof.values()):
            self.sample('select', settlement=True)
            try:
                rows = self.selector.select(0.05)  # pacing only; no new authority clock
            except BaseException:
                self.hold_unknown('original_selector')
            for key, _ in rows:
                name = key.data
                self.sample(name + '_read', settlement=True)
                try:
                    chunk = os.read(key.fd, 65536)
                except BlockingIOError:
                    continue
                except BaseException:
                    self.hold_unknown('original_pipe_read')
                if not chunk:
                    self.eof[name] = True
                    try:
                        self.selector.unregister(key.fileobj)
                    except BaseException:
                        self.latch('unregister')
                    continue
                self.hashes[name].update(chunk);self.bytes[name] += len(chunk)
                if self.expired:
                    continue  # settlement-only drain; no raw write/new effect after refusal
                available = LIMIT - sum(self.stored.values());keep = chunk[:max(0, available)]
                if len(keep) < len(chunk):
                    self.overflow = True
                if 'raw_write_' + name in self.errors:
                    continue
                try:
                    at = 0
                    while at < len(keep):
                        self.sample(name + '_write')
                        count = os.write(self.raw_fds[name], keep[at:])
                        if count <= 0:
                            raise OSError()
                        self.parts[name].append(keep[at:at+count]);at += count;self.stored[name] += count
                except BaseException:
                    self.latch('raw_write_' + name)
        while not self.actual_child_wait:
            self.sample('child_wait', settlement=True)
            try:
                result = self.child.wait()
                if type(result) is not int:
                    self.latch('wait_type');continue
                self.actual_child_wait = True;self.exit_code = result
            except BaseException:
                self.latch('wait_interrupted')
        if not self.expired:
            for name in ('stdout', 'stderr'):
                try:
                    self.sample(name + '_fsync');os.fsync(self.raw_fds[name]);self.fsynced[name] = True
                except BaseException:
                    self.latch('fsync_' + name)
            try:
                self.sample('directory_fsync');os.fsync(self.directory_fd);self.directory_fsynced = True
            except BaseException:
                self.latch('directory_fsync')
        return self.observation(), b''.join(self.parts['stdout']), b''.join(self.parts['stderr'])

def main():
    initial = receive(65536)
    exact(initial, ('schema', 'type', 'experiment_nonce', 'target', 'output'))
    need(type(initial['schema']) is int and initial['schema'] == 1 and initial['type'] == 'start', 'PROTOCOL_SCHEMA')
    import re
    nonce = initial['experiment_nonce']
    need(type(nonce) is str and re.fullmatch(r'[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}', nonce), 'PROTOCOL_NONCE')
    target = initial['target'];exact(target, ('app', 'machine'))
    for value in target.values():
        need(type(value) is str and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,79}', value), 'TARGET_ID')
    need(type(initial['output']) is str and 0 < len(initial['output']) <= 4096 and '\x00' not in initial['output'], 'OUTPUT_PATH')
    owner = OriginalLogs(nonce);HELD.append(owner)
    try:
        capture, stdout, stderr = owner.capture([FLY, 'logs', '--app', target['app'], '--machine', target['machine'], '--no-tail'], initial['output'])
        emit({'schema': 1, 'type': 'captured', 'experiment_nonce': nonce, 'capture': capture, 'qualified': False})
        finalize = receive(65536)
        exact(finalize, ('schema', 'type', 'experiment_nonce', 'expected'))
        need(type(finalize['schema']) is int and finalize['schema'] == 1 and finalize['type'] == 'finalize'
             and finalize['experiment_nonce'] == nonce, 'PROTOCOL_FINALIZE')
        projection = None
        try:
            need(finalize['expected']['experiment_nonce'] == nonce and finalize['expected']['app'] == target['app']
                 and finalize['expected']['machine'] == target['machine'], 'EXPECTED_TARGET')
            projection = project.project(finalize['expected'], capture, stdout, stderr)
        except (Refusal, project.Refusal, KeyError, TypeError):
            pass
        emit({'schema': 1, 'type': 'result', 'experiment_nonce': nonce,
              'capture': capture, 'projection': projection, 'qualified': False})
        return 0 if projection is not None else 2
    except BaseException:
        if owner.spawn_entered and not (owner.actual_child_wait and all(owner.eof.values())):
            owner.hold_unknown('worker_after_original_spawn')
        emit({'schema': 1, 'type': 'refused', 'experiment_nonce': nonce, 'requests': owner.requests, 'qualified': False})
        return 2

if __name__ == '__main__':
    # Node supplies SAME pinned readonly source descriptor 4; no pathname
    # replacement between its hash check and our pure-module import.
    import importlib.machinery
    need(len(sys.argv) == 2 and sys.argv[1] == '/proc/self/fd/4', 'PINNED_PROJECT_DESCRIPTOR')
    loader = importlib.machinery.SourceFileLoader('genuine_project', sys.argv[1])
    spec = importlib.util.spec_from_loader(loader.name, loader)
    project = importlib.util.module_from_spec(spec);loader.exec_module(project)
    raise SystemExit(main())
