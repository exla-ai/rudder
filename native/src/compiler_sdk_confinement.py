"""Mandatory A2 proc-less compiler view and inherited syscall fence.
The observer alone holds the actual fresh-PID proc mount FD. No Landlock,
Root grant, reusable-cache permission, or unsupported-mechanism fallback.
"""
import ctypes as _sc_ctypes
import errno as _sc_errno
import fcntl as _sc_fcntl
import hashlib as _sc_hashlib
import json as _sc_json
import os as _sc_os
import platform as _sc_platform
import stat as _sc_stat
import sys as _sc_sys

SDK_COMPILER_INPUT_POLICY = {'format_version': 2,
 'kind': 'a2-procless-seccomp-read-v2',
 'semantics_version': 2,
 'compiler_proc': 'empty-readonly',
 'observer_proc': 'original-postfork-held-fd-only',
 'workspace_parent': 'readonly',
 'source_read_tree': '/workspace/src',
 'read_file': '/workspace/rules.json',
 'sdk_read_execute_file': '/workspace/buck2',
 'private_read_write_trees': ['/workspace/out', '/tmp'],
 'absent_payload_paths': ['/proc/sys/kernel/random/uuid', '/workspace/job.json'],
 'compiler_stdin': 'empty-pipe',
 'compiler_descriptors': 'stdio-only-at-exec',
 'observer_proc_fd': 'close-before-compiler-exec',
 'namespace_clone_mask': 2114060416,
 'deny_errno': 1,
 'clone3_errno': 38,
 'architectures': {'x86_64': {'audit_arch': 3221225534,
                              'clone': 56,
                              'clone3': 435,
                              'close_range': 436,
                              'denied_syscalls': {'mount': 165,
                                                  'umount2': 166,
                                                  'pivot_root': 155,
                                                  'chroot': 161,
                                                  'unshare': 272,
                                                  'setns': 308,
                                                  'ptrace': 101,
                                                  'process_vm_readv': 310,
                                                  'process_vm_writev': 311,
                                                  'name_to_handle_at': 303,
                                                  'open_by_handle_at': 304,
                                                  'open_tree': 428,
                                                  'move_mount': 429,
                                                  'fsopen': 430,
                                                  'fsconfig': 431,
                                                  'fsmount': 432,
                                                  'fspick': 433,
                                                  'pidfd_getfd': 438,
                                                  'mount_setattr': 442}},
                   'aarch64': {'audit_arch': 3221225655,
                               'clone': 220,
                               'clone3': 435,
                               'close_range': 436,
                               'denied_syscalls': {'mount': 40,
                                                   'umount2': 39,
                                                   'pivot_root': 41,
                                                   'chroot': 51,
                                                   'unshare': 97,
                                                   'setns': 268,
                                                   'ptrace': 117,
                                                   'process_vm_readv': 270,
                                                   'process_vm_writev': 271,
                                                   'name_to_handle_at': 264,
                                                   'open_by_handle_at': 265,
                                                   'open_tree': 428,
                                                   'move_mount': 429,
                                                   'fsopen': 430,
                                                   'fsconfig': 431,
                                                   'fsmount': 432,
                                                   'fspick': 433,
                                                   'pidfd_getfd': 438,
                                                   'mount_setattr': 442}}},
 'unavailable': 'refuse',
 'deterministic_execution': False,
 'cache_reuse': False,
 'sdk_root_mount': 'ro-nosuid-nodev',
 'workspace_mount': 'ro-nosuid-nodev-noexec-recursive-children',
 'source_mount': 'ro-nosuid-nodev-noexec',
 'private_mounts': 'rw-nosuid-nodev-noexec',
 'foreign_audit_arch': 'kill-process',
 'x32_syscall_bit': 1073741824,
 'x32_action': 'kill-process'}

# Kept inline because the authenticated helper embeds this file, not imports.
# The Rust observer validator includes the identical canonical policy JSON.
SDK_COMPILER_ISOLATION = {'cache_reuse': False,
 'compiler_stdio': 'empty-read-pipe-and-two-write-pipes',
 'format_version': 3,
 'interpreters_and_anonymous_jit': 'not-a-semantic-code-execution-boundary',
 'kind': 'compiler-procless-boundary-v3',
 'mounts': ['/', '/tmp', '/workspace', '/workspace/buck2', '/workspace/out', '/workspace/src'],
 'native_writable_exec': 'noexec-mounts-no-memfd-no-execveat',
 'source_declarations': 'job-union-exact-regular-files-no-symlinks-or-hardlinks',
 'syscall_additions': {'aarch64': {'add_key': 217,
                                   'execveat': 281,
                                   'io_uring_enter': 426,
                                   'io_uring_register': 427,
                                   'io_uring_setup': 425,
                                   'keyctl': 219,
                                   'memfd_create': 279,
                                   'msgctl': 187,
                                   'msgget': 186,
                                   'msgrcv': 188,
                                   'msgsnd': 189,
                                   'request_key': 218,
                                   'semctl': 191,
                                   'semget': 190,
                                   'semop': 193,
                                   'semtimedop': 192,
                                   'shmat': 196,
                                   'shmctl': 195,
                                   'shmdt': 197,
                                   'shmget': 194},
                       'x86_64': {'add_key': 248,
                                  'execveat': 322,
                                  'io_uring_enter': 426,
                                  'io_uring_register': 427,
                                  'io_uring_setup': 425,
                                  'keyctl': 250,
                                  'memfd_create': 319,
                                  'msgctl': 71,
                                  'msgget': 68,
                                  'msgrcv': 70,
                                  'msgsnd': 69,
                                  'request_key': 249,
                                  'semctl': 66,
                                  'semget': 64,
                                  'semop': 65,
                                  'semtimedop': 220,
                                  'shmat': 30,
                                  'shmctl': 31,
                                  'shmdt': 67,
                                  'shmget': 29}}}

def consume_sdk_observer_job():
    """Consume only sealed bounded DATA, then permanently replace fd0 by EOF."""
    meta = _sc_os.fstat(0)
    flags = _sc_fcntl.fcntl(0, _sc_fcntl.F_GETFL)
    seals = _sc_fcntl.fcntl(0, _sc_fcntl.F_GET_SEALS)
    if (not _sc_stat.S_ISREG(meta.st_mode) or meta.st_nlink != 0 or
        not 0 < meta.st_size <= 16 * 1024 or flags & _sc_os.O_ACCMODE != _sc_os.O_RDONLY or
        seals & 15 != 15):
        raise RuntimeError('SDK_OBSERVER_SEALED_REQUEST_REQUIRED')
    request = _sc_sys.stdin.buffer.read(16 * 1024 + 1)
    if len(request) != meta.st_size:
        raise RuntimeError('SDK_OBSERVER_REQUEST_BOUND')
    job = _sc_json.loads(request)
    del request
    _sc_sys.stdin.close()
    read_fd, write_fd = _sc_os.pipe2(_sc_os.O_CLOEXEC)
    _sc_os.close(write_fd)
    if read_fd != 0:
        _sc_os.dup2(read_fd, 0, inheritable=False)
        _sc_os.close(read_fd)
    else:
        _sc_os.set_inheritable(0, False)
    return job


class SdkObserverProc:
    """An actual descriptor, never a proc capability reconstructed from DATA."""
    _allowed = {'self', 'self/exe', 'self/status', 'self/task',
                'self/ns/user', 'self/ns/mnt', 'self/ns/pid', 'self/ns/net', 'self/mountinfo'}

    def __init__(self, fd):
        if not isinstance(fd, int) or fd < 3:
            raise RuntimeError('SDK_OBSERVER_PROC_DESCRIPTOR')
        self.fd = fd
        meta = _sc_os.fstat(fd)
        if not _sc_stat.S_ISDIR(meta.st_mode):
            raise RuntimeError('SDK_OBSERVER_PROC_NOT_DIRECTORY')
        self.identity = (meta.st_dev, meta.st_ino, meta.st_mode)
        libc = _sc_ctypes.CDLL(None, use_errno=True)
        data = _sc_ctypes.create_string_buffer(256)
        if libc.fstatfs(fd, _sc_ctypes.byref(data)) != 0:
            raise OSError(_sc_ctypes.get_errno(), 'SDK_OBSERVER_PROC_FSTATFS')
        if _sc_ctypes.c_long.from_buffer(data).value != 0x9fa0:
            raise RuntimeError('SDK_OBSERVER_DESCRIPTOR_IS_NOT_PROC')
        _sc_os.set_inheritable(fd, False)
        if self.readlink('self') != str(_sc_os.getpid()) or _sc_os.getpid() != 1:
            raise RuntimeError('SDK_OBSERVER_PROC_NOT_ORIGINAL_FRESH_PID_MOUNT')
        self.pid_namespace = self.readlink('self/ns/pid')

    def current(self):
        meta = _sc_os.fstat(self.fd)
        if ((meta.st_dev, meta.st_ino, meta.st_mode) != self.identity or
            _sc_os.get_inheritable(self.fd)):
            raise RuntimeError('SDK_OBSERVER_PROC_DESCRIPTOR_CHANGED')

    def stat(self, path):
        self.current()
        if path not in self._allowed:
            raise RuntimeError('SDK_OBSERVER_PROC_PATH')
        return _sc_os.stat(path, dir_fd=self.fd)

    def readlink(self, path):
        self.current()
        if path not in self._allowed:
            raise RuntimeError('SDK_OBSERVER_PROC_PATH')
        return _sc_os.readlink(path, dir_fd=self.fd)

    def read(self, path, maximum):
        self.current()
        if path not in ('self/exe', 'self/status', 'self/mountinfo') or not 0 < maximum <= 64 * 1024 * 1024:
            raise RuntimeError('SDK_OBSERVER_PROC_READ_BOUND')
        fd = _sc_os.open(path, _sc_os.O_RDONLY | _sc_os.O_CLOEXEC, dir_fd=self.fd)
        try:
            chunks = []
            total = 0
            while total <= maximum:
                block = _sc_os.read(fd, min(65536, maximum + 1 - total))
                if not block:
                    return b''.join(chunks)
                total += len(block)
                chunks.append(block)
            raise RuntimeError('SDK_OBSERVER_PROC_PAYLOAD_BOUND')
        finally:
            _sc_os.close(fd)

    def single_threaded(self):
        self.current()
        fd = _sc_os.open('self/task', _sc_os.O_RDONLY | _sc_os.O_DIRECTORY | _sc_os.O_CLOEXEC,
                         dir_fd=self.fd)
        try:
            return len(_sc_os.listdir(fd)) == 1
        finally:
            _sc_os.close(fd)


class _SockFilter(_sc_ctypes.Structure):
    _fields_ = [('code', _sc_ctypes.c_ushort), ('jt', _sc_ctypes.c_ubyte),
                ('jf', _sc_ctypes.c_ubyte), ('k', _sc_ctypes.c_uint32)]


class _SockProgram(_sc_ctypes.Structure):
    _fields_ = [('length', _sc_ctypes.c_ushort),
                ('filter', _sc_ctypes.POINTER(_SockFilter))]


class _CompilerSeccomp:
    """Actual inherited Linux filter; it is not a filesystem/SDK owner."""
    def __init__(self, policy):
        if policy != SDK_COMPILER_INPUT_POLICY:
            raise RuntimeError('SDK_FIXED_SYSCALL_POLICY_DIFFERS')
        if (_sc_platform.system() != 'Linux' or _sc_sys.byteorder != 'little' or
            _sc_ctypes.sizeof(_sc_ctypes.c_void_p) != 8 or _sc_ctypes.sizeof(_sc_ctypes.c_long) != 8):
            raise RuntimeError('SDK_SECCOMP_PLATFORM_UNSUPPORTED')
        self.architecture = _sc_platform.machine()
        if self.architecture not in policy['architectures']:
            raise RuntimeError('SDK_SECCOMP_ARCHITECTURE_UNSUPPORTED')
        self.numbers = policy['architectures'][self.architecture]
        self.libc = _sc_ctypes.CDLL(None, use_errno=True)
        self.libc.syscall.restype = _sc_ctypes.c_long
        deny = 0x50000 | policy['deny_errno']
        unavailable = 0x50000 | policy['clone3_errno']
        allow = 0x7fff0000
        # seccomp_data: nr@0, architecture@4, arguments[0]@16 (low/high).
        instructions = [(0x20, 0, 0, 4),
            (0x15, 1, 0, self.numbers['audit_arch']), (0x06, 0, 0, 0x80000000),
            (0x20, 0, 0, 0), (0x45, 0, 1, policy['x32_syscall_bit']), (0x06, 0, 0, 0x80000000),
            (0x15, 0, 1, self.numbers['clone3']), (0x06, 0, 0, unavailable),
            (0x15, 0, 7, self.numbers['clone']),
            (0x20, 0, 0, 20), (0x15, 1, 0, 0), (0x06, 0, 0, deny),
            (0x20, 0, 0, 16), (0x45, 0, 1, policy['namespace_clone_mask']),
            (0x06, 0, 0, deny), (0x06, 0, 0, allow)]
        for number in sorted(set(self.denied_syscalls().values())):
            instructions.extend([(0x15, 0, 1, number), (0x06, 0, 0, deny)])
        instructions.append((0x06, 0, 0, allow))
        if len(instructions) > 256 or _sc_ctypes.sizeof(_SockFilter) != 8:
            raise RuntimeError('SDK_SECCOMP_PROGRAM_BOUND_OR_ABI')
        self.instructions = (_SockFilter * len(instructions))(
            *(_SockFilter(*item) for item in instructions))
        self.program = _SockProgram(len(instructions), self.instructions)

    def denied_syscalls(self):
        return {**self.numbers['denied_syscalls'],
                **SDK_COMPILER_ISOLATION['syscall_additions'][self.architecture]}

    def install(self):
        if self.libc.prctl(39, 0, 0, 0, 0) != 1:
            raise RuntimeError('SDK_SECCOMP_ORIGINAL_NNP_ABSENT')
        if self.libc.prctl(22, 2, _sc_ctypes.byref(self.program), 0, 0) != 0:
            raise OSError(_sc_ctypes.get_errno(), 'SDK_SECCOMP_REQUIRED')
        if self.libc.prctl(21, 0, 0, 0, 0) != 2:
            raise RuntimeError('SDK_SECCOMP_FILTER_NOT_ACTIVE')

    def probe(self):
        for name, expected in (('unshare', _sc_errno.EPERM), ('clone3', _sc_errno.ENOSYS),
                               ('memfd_create', _sc_errno.EPERM),
                               ('execveat', _sc_errno.EPERM),
                               ('io_uring_setup', _sc_errno.EPERM),
                               ('shmget', _sc_errno.EPERM), ('keyctl', _sc_errno.EPERM)):
            number = (self.numbers['clone3'] if name == 'clone3'
                      else self.denied_syscalls()[name])
            _sc_ctypes.set_errno(0)
            value = self.libc.syscall(number, 0, 0)
            if value != -1 or _sc_ctypes.get_errno() != expected:
                raise RuntimeError('SDK_SECCOMP_ACTUAL_DENIAL_PROBE_' + name)


class SdkCompilerReadPolicy:
    def __init__(self, policy, observer_proc, declared_sources):
        if policy != SDK_COMPILER_INPUT_POLICY:
            raise RuntimeError('SDK_FIXED_VIEW_POLICY_DIFFERS')
        observer_proc.current()
        if not observer_proc.single_threaded():
            raise RuntimeError('SDK_COMPILER_REQUIRES_SINGLE_THREAD_OBSERVER')
        if observer_proc.readlink('self/ns/pid') != observer_proc.pid_namespace:
            raise RuntimeError('SDK_OBSERVER_ORIGINAL_PID_NAMESPACE_CHANGED')
        for path in ('/', '/proc', '/workspace', '/workspace/src', '/workspace/rules.json', '/workspace/buck2'):
            if not _sc_os.statvfs(path).f_flag & _sc_os.ST_RDONLY:
                raise RuntimeError('SDK_COMPILER_VIEW_NOT_READONLY_' + path)
        required = _sc_os.ST_NOSUID | _sc_os.ST_NODEV | _sc_os.ST_NOEXEC
        for path in ('/workspace', '/workspace/src', '/workspace/out', '/tmp'):
            if _sc_os.statvfs(path).f_flag & required != required:
                raise RuntimeError('SDK_COMPILER_VIEW_MOUNT_FLAGS_' + path)
        if _sc_os.statvfs('/').f_flag & (_sc_os.ST_NOSUID | _sc_os.ST_NODEV) != (_sc_os.ST_NOSUID | _sc_os.ST_NODEV):
            raise RuntimeError('SDK_COMPILER_SDK_MOUNT_FLAGS')
        with _sc_os.scandir('/proc') as entries:
            if next(entries, None) is not None:
                raise RuntimeError('SDK_COMPILER_PROC_VIEW_NOT_EMPTY')
        if _sc_os.path.lexists('/workspace/job.json'):
            raise RuntimeError('SDK_JOB_FILE_MUST_NOT_EXIST')
        for path in policy['private_read_write_trees']:
            if _sc_os.statvfs(path).f_flag & _sc_os.ST_RDONLY:
                raise RuntimeError('SDK_COMPILER_PRIVATE_DIRECTORY_READONLY')
            with _sc_os.scandir(path) as entries:
                if next(entries, None) is not None:
                    raise RuntimeError('SDK_COMPILER_PRIVATE_DIRECTORY_NOT_EMPTY')
        self.mount_count = self.validate_mounts(observer_proc)
        self.validate_sources(declared_sources)
        status = dict(line.split(':', 1) for line in observer_proc.read('self/status', 65536).decode().splitlines() if ':' in line)
        if any(int(status[key].strip(), 16) for key in ('CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd')) or status['NoNewPrivs'].strip() != '1':
            raise RuntimeError('SDK_COMPILER_ORIGINAL_PRIVILEGE_FLOOR_REQUIRED')
        self.proc = observer_proc
        self.filter = _CompilerSeccomp(policy)
        self.policy_sha256 = _sc_hashlib.sha256(_sc_json.dumps(
            policy, sort_keys=True, separators=(',', ':')).encode()).hexdigest()

    @staticmethod
    def validate_mounts(observer_proc):
        # statvfs on a handful of paths misses recursive child mounts and aliases.
        # The fresh observer proc reports the actual chroot-relative mount table.
        expected = set(SDK_COMPILER_ISOLATION['mounts'])
        seen = set()
        for line in observer_proc.read('self/mountinfo', 65536).decode('ascii').splitlines():
            fields = line.split()
            if len(fields) < 10 or '-' not in fields:
                raise RuntimeError('SDK_COMPILER_MOUNT_TABLE_INVALID')
            path = fields[4]
            options = set(fields[5].split(','))
            kind = fields[fields.index('-') + 1]
            if path not in expected or path in seen or kind in ('proc', 'sysfs', 'devtmpfs', 'devpts', 'mqueue', 'cgroup', 'cgroup2'):
                raise RuntimeError('SDK_COMPILER_UNDECLARED_MOUNT_' + path)
            required = {'nosuid', 'nodev'}
            required.add('rw' if path in ('/workspace/out', '/tmp') else 'ro')
            if path not in ('/', '/workspace/buck2'):
                required.add('noexec')
            if not required <= options:
                raise RuntimeError('SDK_COMPILER_MOUNT_TABLE_FLAGS_' + path)
            seen.add(path)
        if seen != expected:
            raise RuntimeError('SDK_COMPILER_MOUNT_TABLE_INCOMPLETE')
        return len(seen)

    @staticmethod
    def validate_sources(declared_sources):
        if not isinstance(declared_sources, set) or not declared_sources or len(declared_sources) > 100000:
            raise RuntimeError('SDK_COMPILER_SOURCE_DECLARATION_REQUIRED')
        actual = set()
        pending = [('/workspace/src', '')]
        count = 0
        while pending:
            base, relative = pending.pop()
            with _sc_os.scandir(base) as entries:
                for entry in entries:
                    count += 1
                    if count > 100000:
                        raise RuntimeError('SDK_COMPILER_SOURCE_ENTRY_BOUND')
                    name = relative + entry.name
                    meta = entry.stat(follow_symlinks=False)
                    if _sc_stat.S_ISDIR(meta.st_mode):
                        if name.count('/') >= 256:
                            raise RuntimeError('SDK_COMPILER_SOURCE_DEPTH_BOUND')
                        pending.append((entry.path, name + '/'))
                    elif _sc_stat.S_ISREG(meta.st_mode) and meta.st_nlink == 1:
                        actual.add(name)
                    else:
                        raise RuntimeError('SDK_COMPILER_SOURCE_LINK_OR_SPECIAL_FILE')
        if actual != declared_sources:
            raise RuntimeError('SDK_COMPILER_UNDECLARED_SOURCE_FILES')

    def before_exec(self):
        # Only the real compiler fork enters this path. Its observer keeps the
        # original proc FD. No protected descriptor is allowed through exec.
        if self.filter.libc.prctl(3, 0, 0, 0, 0) != 0:
            raise RuntimeError('SDK_COMPILER_ORIGINAL_ND0_ABSENT')
        # Popen reuses fd0 without dup2 when stdin=0. Undo CLOEXEC only here,
        # after checking it is the consumed request's empty pipe.
        for fd, mode in ((0, _sc_os.O_RDONLY), (1, _sc_os.O_WRONLY), (2, _sc_os.O_WRONLY)):
            if not _sc_stat.S_ISFIFO(_sc_os.fstat(fd).st_mode) or _sc_fcntl.fcntl(fd, _sc_fcntl.F_GETFL) & _sc_os.O_ACCMODE != mode:
                raise RuntimeError('SDK_COMPILER_STDIO_MUST_BE_PRIVATE_PIPES')
            _sc_os.set_inheritable(fd, True)
        _sc_os.close(self.proc.fd)
        try:
            _sc_fcntl.fcntl(self.proc.fd, _sc_fcntl.F_GETFD)
        except OSError as error:
            if error.errno != _sc_errno.EBADF:
                raise
        else:
            raise RuntimeError('SDK_COMPILER_PROC_DESCRIPTOR_STILL_OPEN')
        # Keep the Popen exec-error pipe functioning until exec; close all
        # nonstdio handles atomically on actual exec, including any new ones.
        if self.filter.libc.syscall(self.filter.numbers['close_range'], 3,
                                   _sc_ctypes.c_uint(0xffffffff), 4) != 0:
            raise OSError(_sc_ctypes.get_errno(), 'SDK_COMPILER_CLOSE_ON_EXEC_REQUIRED')
        self.filter.install()
        self.filter.probe()
        for path in SDK_COMPILER_INPUT_POLICY['absent_payload_paths']:
            try:
                fd = _sc_os.open(path, _sc_os.O_RDONLY | _sc_os.O_CLOEXEC)
            except OSError as error:
                if error.errno != _sc_errno.ENOENT:
                    raise RuntimeError('SDK_COMPILER_ABSENT_INPUT_PROBE') from error
            else:
                _sc_os.close(fd)
                raise RuntimeError('SDK_COMPILER_PRIVATE_INPUT_VISIBLE')
        if _sc_os.read(0, 1) != b'':
            raise RuntimeError('SDK_COMPILER_STDIN_NOT_EMPTY')

    def completed_spawn_data(self):
        # Called only after genuine exec, successful Child exit and both EOFs.
        return {'format_version': 3, 'kind': 'actual-a2-procless-seccomp-child-v3',
                'isolation_sha256': _sc_hashlib.sha256(_sc_json.dumps(SDK_COMPILER_ISOLATION, sort_keys=True, separators=(',', ':')).encode()).hexdigest(),
                'mount_count': self.mount_count, 'source_declarations': 'job-union-exact',
                'memfd_errno': _sc_errno.EPERM, 'execveat_errno': _sc_errno.EPERM,
                'io_uring_errno': _sc_errno.EPERM, 'sysv_ipc_errno': _sc_errno.EPERM,
                'keyring_errno': _sc_errno.EPERM,
                'policy_sha256': self.policy_sha256,
                'architecture': self.filter.architecture, 'seccomp_mode': 2,
                'unshare_errno': _sc_errno.EPERM, 'clone3_errno': _sc_errno.ENOSYS,
                'proc_uuid_errno': _sc_errno.ENOENT, 'job_file_errno': _sc_errno.ENOENT,
                'observer_proc_fd_closed': True, 'proc_empty_readonly': True,
                'workspace_parent_readonly': True, 'stdin_eof': True,
                'descriptors': 'stdio-only-at-exec', 'cache_reuse': False}
