#!/usr/bin/python3
"""Report image packaging errors before the native immutable-tree import.

Build-only diagnostics. The native importer remains the authority for the tree,
content, hardlinks, resource bounds and retained pins.
"""
import os
import json
from pathlib import Path
import stat

ROOTS = (Path('/usr'), Path('/opt/rudder'),
         Path('/opt/rudder-managed-runtime/v1'), Path('/etc/ssl/certs'),
         Path('/etc/alternatives'))
PROVIDER_KERNEL = Path('/usr/lib/modules')
PROVIDER_KERNEL_BOUNDARY_ID = 'provider-kernel-empty-v1'
PROVIDER_TOOL = Path('/usr/sbin/swapon')
FILES = (Path('/etc/ld.so.cache'), Path('/etc/localtime'),
         Path('/etc/ssl/openssl.cnf'), Path('/etc/python3.11/sitecustomize.py'))


def main():
    if os.getuid() != 0 or os.geteuid() != 0:
        raise SystemExit('image layout diagnostics require image Root')
    provider = PROVIDER_KERNEL.lstat()
    if (not stat.S_ISDIR(provider.st_mode) or stat.S_ISLNK(provider.st_mode) or
            provider.st_uid != 0 or provider.st_gid != 0 or provider.st_mode & 0o022 or
            PROVIDER_KERNEL.resolve(strict=True) != PROVIDER_KERNEL):
        raise SystemExit('provider kernel boundary is not an actual protected directory')
    tool = PROVIDER_TOOL.lstat()
    if (not stat.S_ISREG(tool.st_mode) or tool.st_uid != 0 or tool.st_gid != 0 or
            tool.st_mode & 0o022 or PROVIDER_TOOL.resolve(strict=True) != PROVIDER_TOOL):
        raise SystemExit('provider tool boundary is not an actual protected file')
    errors = []
    entries = 0
    regular_bytes = 0
    inodes = {}

    def check(path):
        nonlocal entries, regular_bytes
        entries += 1
        if entries > 65536:
            raise SystemExit('image layout exceeds 65536 entries')
        metadata = path.lstat()
        reasons = []
        if path == PROVIDER_TOOL:
            return
        if path == PROVIDER_KERNEL and not stat.S_ISDIR(metadata.st_mode):
            reasons.append('provider-kernel-boundary-type')
        if metadata.st_uid != 0 or metadata.st_gid != 0:
            reasons.append('owner')
        if stat.S_ISLNK(metadata.st_mode):
            try:
                target = path.resolve(strict=True)
                if target.is_relative_to(PROVIDER_KERNEL) or target == PROVIDER_TOOL:
                    reasons.append('provider-kernel-target:' + str(target))
                if not (target == Path('/dev/null') or target in FILES or
                        any(target.is_relative_to(root) for root in ROOTS)):
                    reasons.append('undeclared-target:' + str(target))
            except (OSError, RuntimeError) as error:
                reasons.append('unavailable-target:' + type(error).__name__)
        else:
            if metadata.st_mode & 0o022:
                reasons.append('mutable')
            if stat.S_ISREG(metadata.st_mode):
                regular_bytes += metadata.st_size
                identity = (metadata.st_dev, metadata.st_ino)
                group = inodes.setdefault(identity, {'bytes': metadata.st_size, 'paths': []})
                group['paths'].append(str(path))
                if not metadata.st_mode & 0o004:
                    reasons.append('unreadable')
            elif not stat.S_ISDIR(metadata.st_mode):
                reasons.append('special-file')
        if reasons:
            errors.append((str(path), reasons))

    for root in ROOTS:
        check(root)
        for directory, directories, files in os.walk(root, followlinks=False):
            for name in sorted(directories + files):
                check(Path(directory) / name)
            # Count/check the boundary itself, never provider payload entries.
            directories[:] = [name for name in directories
                              if Path(directory) / name != PROVIDER_KERNEL]
    for path in FILES:
        check(path)
    for path, reasons in errors[:100]:
        print('RUDDER_IMAGE_LAYOUT_ERROR', path, ','.join(reasons), flush=True)
    if errors:
        raise SystemExit(f'image layout has {len(errors)} invalid entries')
    print('RUDDER_IMAGE_LAYOUT_CHECKED', entries, flush=True)
    print('RUDDER_PROVIDER_KERNEL_BOUNDARY', json.dumps({
        'policy_id': PROVIDER_KERNEL_BOUNDARY_ID,
        'host_directory': str(PROVIDER_KERNEL),
        'expected_guest_view': 'empty-readonly-0555',
        'provider_payload_captured': False,
        'kernel_policy_qualified': False,
        'provider_tool_file': str(PROVIDER_TOOL),
        'provider_tool_guest_view': 'empty-readonly-noexec-0444',
        'authority': False,
    }, separators=(',', ':')), flush=True)
    linked = sorted((group for group in inodes.values() if len(group['paths']) > 1),
                    key=lambda group: group['bytes'] * len(group['paths']), reverse=True)
    print('RUDDER_IMAGE_INPUT_BYTES', json.dumps({
        'regular_path_bytes': regular_bytes,
        'unique_inode_bytes': sum(group['bytes'] for group in inodes.values()),
        'hardlink_groups': len(linked),
        'largest_hardlink_groups': [{'bytes': group['bytes'], 'aliases': len(group['paths']),
                                     'first_path': group['paths'][0]} for group in linked[:10]],
    }, separators=(',', ':')), flush=True)


if __name__ == '__main__':
    main()
