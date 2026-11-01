# Fixed credential-free physical probe. No model/provider requests or compiler.
import ctypes
import errno
import json
import os
import socket
import stat
import sys

expected = json.loads(sys.argv[1])
assert expected['profile'] == 'spec4-local-complete-offline-provider-kernel-empty-v1'
assert os.getuid() == 1500 and os.getgid() == 1500 and os.getgroups() == []
status = {}
with open('/proc/self/status', encoding='ascii') as stream:
    for line in stream:
        key, _, value = line.partition(':')
        status[key] = value.strip()
assert all(int(status[key], 16) == 0 for key in ['CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd'])
assert status['NoNewPrivs'] == '1'
# Seccomp mode is observation DATA, not evidence this implementation installed a filter.
seccomp = int(status['Seccomp'])
assert seccomp in (0, 2)
assert 'HOST_ENV_MUST_NOT_LEAK' not in os.environ
namespaces = {}
for name, host in expected['host_namespaces'].items():
    value = os.stat('/proc/self/ns/' + name)
    namespaces[name] = {'device': str(value.st_dev), 'inode': str(value.st_ino)}
    if name in ('user', 'mnt', 'pid', 'net'):
        assert namespaces[name] != host
assert sorted(name for _, name in socket.if_nameindex()) == ['lo']

for path in ('/usr/lib/modules', '/lib/modules'):
    value = os.stat(path)
    assert stat.S_ISDIR(value.st_mode) and stat.S_IMODE(value.st_mode) == 0o555
    assert os.listdir(path) == []
    flags = os.statvfs(path).f_flag
    assert flags & (os.ST_RDONLY | os.ST_NOSUID | os.ST_NODEV | os.ST_NOEXEC) == (os.ST_RDONLY | os.ST_NOSUID | os.ST_NODEV | os.ST_NOEXEC)
    try:
        fd = os.open(path + '/forbidden', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError as error:
        assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM)
    else:
        os.close(fd)
        raise AssertionError('masked kernel directory writable')
assert os.readlink('/lib') == 'usr/lib'
for path in ('/usr/sbin/swapon', '/sbin/swapon'):
    value = os.stat(path)
    assert stat.S_ISREG(value.st_mode) and stat.S_IMODE(value.st_mode) == 0o444 and value.st_size == 0
    flags = os.statvfs(path).f_flag
    assert flags & (os.ST_RDONLY | os.ST_NOSUID | os.ST_NODEV | os.ST_NOEXEC) == (os.ST_RDONLY | os.ST_NOSUID | os.ST_NODEV | os.ST_NOEXEC)
    with open(path, 'rb') as stream:
        assert stream.read(1) == b''
    assert not os.access(path, os.X_OK)
    try:
        fd = os.open(path, os.O_WRONLY)
    except OSError as error:
        assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM)
    else:
        os.close(fd)
        raise AssertionError('provider tool mask writable')
assert os.readlink('/sbin') == 'usr/sbin'
for path in expected['forbidden_paths'] + ['/dev/fuse', '/etc/shadow', '/.old-root/usr/lib/modules', '/.old-root/usr/sbin/swapon', '/.fly/swapon']:
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    except OSError as error:
        assert error.errno in (errno.ENOENT, errno.EACCES, errno.EPERM, errno.ENOTDIR)
    else:
        os.close(fd)
        raise AssertionError('private/old-root file reachable')
try:
    os.listdir('/proc/1/fd')
except OSError as error:
    assert error.errno in (errno.EACCES, errno.EPERM)
else:
    raise AssertionError('namespace init descriptor table readable')
try:
    os.fstat(200)
except OSError as error:
    assert error.errno == errno.EBADF
else:
    raise AssertionError('deliberately inherited private descriptor reached guest')
with os.scandir('/proc/self/fd') as entries:
    descriptors = [entry.name for entry in entries]
for descriptor in descriptors:
    try:
        target = os.readlink('/proc/self/fd/' + descriptor)
    except FileNotFoundError:
        continue
    assert not target.startswith(expected['private_root'])
    assert int(descriptor) <= 2

libc = ctypes.CDLL(None, use_errno=True)
def deny(name, operation):
    ctypes.set_errno(0)
    result = operation()
    error = ctypes.get_errno()
    assert result == -1 and error in (errno.EPERM, errno.EACCES), (name, result, error)
    return error
ns_fd = os.open('/proc/self/ns/mnt', os.O_RDONLY | os.O_CLOEXEC)
try:
    denials = {
        'mount': deny('mount', lambda: libc.mount(b'none', b'/tmp', b'tmpfs', 0, None)),
        'unmount_mask': deny('unmount_mask', lambda: libc.umount2(b'/usr/lib/modules', 0)),
        'setns': deny('setns', lambda: libc.setns(ns_fd, 0x00020000)),
        'ptrace_init': deny('ptrace_init', lambda: libc.ptrace(16, 1, None, None)),
    }
finally:
    os.close(ns_fd)
# No new seccomp requirement: nested USER/NS creation may legitimately succeed.
# Its inherited locked mounts must still prevent uncovering the provider tree.
r, w = os.pipe2(os.O_CLOEXEC)
child = os.fork()
if child == 0:
    os.close(r)
    try:
        ctypes.set_errno(0)
        result = libc.unshare(0x10000000 | 0x00020000)
        unshare_errno = ctypes.get_errno()
        unmount_errno = None
        if result == 0:
            ctypes.set_errno(0)
            unmount = libc.umount2(b'/usr/lib/modules', 0)
            unmount_errno = ctypes.get_errno()
            assert unmount == -1 and unmount_errno in (errno.EPERM, errno.EACCES, errno.EINVAL)
            assert os.listdir('/usr/lib/modules') == [] and os.listdir('/lib/modules') == []
            ctypes.set_errno(0)
            assert libc.umount2(b'/usr/sbin/swapon', 0) == -1
            assert ctypes.get_errno() in (errno.EPERM, errno.EACCES, errno.EINVAL)
            assert os.stat('/usr/sbin/swapon').st_size == 0 and os.stat('/sbin/swapon').st_size == 0
        else:
            assert unshare_errno in (errno.EPERM, errno.EACCES, errno.EINVAL, errno.ENOSYS)
        body = json.dumps({'unshare_result': result, 'unshare_errno': unshare_errno,
                           'unmount_errno': unmount_errno, 'payload_visible': False}, separators=(',', ':')).encode()
        assert os.write(w, body) == len(body)
        os.close(w)
        os._exit(0)
    except BaseException:
        os._exit(1)
os.close(w)
raw = b''
while True:
    block = os.read(r, 4096)
    if not block:
        break
    raw += block
    assert len(raw) <= 4096
os.close(r)
actual_child, child_status = os.waitpid(child, 0)
assert actual_child == child and os.WIFEXITED(child_status) and os.WEXITSTATUS(child_status) == 0
nested = json.loads(raw)
assert nested['payload_visible'] is False
report = {'format_version': 1, 'nonce': expected['nonce'], 'profile': expected['profile'],
          'uid': os.getuid(), 'gid': os.getgid(), 'all_capabilities_zero': True,
          'no_new_privileges': True, 'mask_empty_readonly': True, 'merged_alias_masked': True, 'provider_tools_masked': True,
          'private_and_old_root_denied': True, 'inherited_fd_closed': True,
          'namespaces': namespaces, 'interfaces': ['lo'], 'seccomp_mode': seccomp,
          'seccomp_filter_installed_by_fixture': False, 'denial_errno': denials,
          'nested_namespace': nested, 'nested_child_reaped': True, 'nested_pipe_eof': True}
print('RUDDER_PROVIDER_KERNEL_GUEST ' + json.dumps(report, sort_keys=True, separators=(',', ':')), flush=True)
assert sys.stdin.buffer.read(1) == b'Q'
print('RUDDER_PROVIDER_KERNEL_DONE', flush=True)
