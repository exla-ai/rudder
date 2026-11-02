"""Pure, non-authoritative historical DATA projection; never dispatches or reads files."""
import hashlib
import json
import re

ROOT = 'RUDDER_GENUINE_ROOT_COMMAND_DATA_V1='
NATIVE = 'RUDDER_GENUINE_INGRESS_BOUNDARY_DATA_V1='
LIMIT = 8 * 1024 * 1024
ROOT_RECEIPT_MANIFEST_SHA256 = '08ec75f53153899d855c53621a86d2781e89ce7bb12c77f97a55cd386dd0f264'
FIXTURE_MANIFEST_SHA256 = '7a5a41afc55a7569a709d1d2224c3a5b1368ac16731f52f6b0f3411c6d7ecacf'
LEAVES = {
    'CloseWins': ('managed-ingress-fixture-close-wins', False, False, True, False),
    'AdmissionWins': ('managed-ingress-fixture-admission-wins', True, True, False, False),
    'CarrierContention': ('managed-ingress-fixture-carrier-contention', True, True, False, True),
}
SHA = re.compile(r'[0-9a-f]{64}\Z')
UUID = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\Z')
KEY = re.compile(r'[A-Za-z0-9_.:-]{1,128}\Z')
EXPECTED_KEYS = set('schema case feature experiment_nonce app machine instance_id image_digest integrated_revision source_manifest_sha256 root_receipt_manifest_sha256 fixture_manifest_sha256 native_elf_sha256 engine_elf_sha256 rpc_wire rpc_wire_sha256 session_generation command_key dispatch_disposition dispatch_attempts rpc_send_observations original_session_retained'.split())
ROOT_KEYS = set('schema case canonical_command_sha256 raw_packet_sha256 raw_packet_bytes command_key_sha256 rpc_id_sha256 operation_sha256 session_generation_sha256 constraint original_forward_outcome original_send_attempt_armed original_pre_forward_sample_live root_known_forwarded reporting_worker_pending source_accepted runtime_ready retirement_complete'.split())
NATIVE_KEYS = set('schema case fixture_boundary_observed current original_job_retained root_present exact_original_root_captured original_root_refusal_retained candidate_pre_effect no_descendants actual_worker_carrier_would_block coordinator_joined_including_tls original_ingress_worker_pending original_runtime_worker_pending reporting_worker_pending covered_roots_closed command_sha256 session_generation_sha256 source_accepted runtime_ready retirement_complete'.split())
CAPTURE_KEYS = set('schema constructor_completed actual_child_wait exit_code pidfd_held stdout_eof stderr_eof errors expired overflow original_budget_requests original_budget_refused raw_files_fsynced raw_directory_fsynced stdout_bytes stderr_bytes stdout_sha256 stderr_sha256 stdout_stored_bytes stderr_stored_bytes'.split())

class Refusal(ValueError):
    """Only fixed labels leave this parser; never private payload/error text."""

def require(condition, code):
    if not condition:
        raise Refusal(code)

def exact(value, keys, code):
    require(type(value) is dict and set(value) == set(keys), code)

def pairs(rows):
    result = {}
    for key, value in rows:
        require(key not in result, 'DUPLICATE_JSON_KEY')
        result[key] = value
    return result

def loads(raw, maximum=32768):
    require(type(raw) in (bytes, str), 'JSON_TYPE')
    try:
        encoded = raw.encode('utf-8', 'strict') if type(raw) is str else raw
        require(0 < len(encoded) <= maximum, 'JSON_BOUND')
        value = json.loads(encoded.decode('utf-8', 'strict'), object_pairs_hook=pairs,
                           parse_constant=lambda _: (_ for _ in ()).throw(Refusal('JSON_NUMBER')))
        # Reject lone UTF-16 surrogates, unsafe integers, floats and excessive nesting.
        canonical(value, maximum)
        return value
    except Refusal:
        raise
    except (ValueError, UnicodeError, RecursionError, TypeError):
        raise Refusal('JSON_ENCODING') from None

def canonical(value, maximum=32768):
    nodes = 0
    def visit(item, depth):
        nonlocal nodes
        nodes += 1
        require(depth <= 24 and nodes <= 16384, 'JSON_COMPLEXITY')
        if item is None or type(item) is bool:
            return
        if type(item) is int:
            require(abs(item) <= 9007199254740991, 'JSON_INTEGER')
        elif type(item) is str:
            item.encode('utf-8', 'strict')
        elif type(item) is dict:
            for key, child in item.items():
                require(type(key) is str and key.isascii(), 'JSON_KEY')
                visit(child, depth + 1)
        elif type(item) is list:
            for child in item:
                visit(child, depth + 1)
        else:
            raise Refusal('JSON_VALUE')
    try:
        visit(value, 0)
        out = json.dumps(value, ensure_ascii=False, separators=(',', ':'), sort_keys=True).encode('utf-8')
        require(len(out) <= maximum, 'JSON_BOUND')
        return out
    except (UnicodeError, RecursionError):
        raise Refusal('JSON_ENCODING') from None

def digest(raw):
    return hashlib.sha256(raw).hexdigest()

def sha(value):
    require(type(value) is str and SHA.fullmatch(value) is not None and value != '0' * 64, 'SHA256')

def text(value, maximum=128):
    require(type(value) is str and 0 < len(value.encode('utf-8')) <= maximum
            and not any(ord(c) < 32 or ord(c) == 127 for c in value), 'TEXT')

def capture_valid(c, stdout, stderr):
    exact(c, CAPTURE_KEYS, 'CAPTURE_SCHEMA')
    require(type(stdout) is bytes and type(stderr) is bytes and len(stdout) + len(stderr) <= LIMIT, 'LOG_BOUND')
    require(type(c['schema']) is int and c['schema'] == 2, 'CAPTURE_SCHEMA')
    for key in ('constructor_completed', 'actual_child_wait', 'pidfd_held', 'stdout_eof', 'stderr_eof'):
        require(c[key] is True, 'ORIGINAL_CAPTURE_PARTIAL')
    require(c['errors'] == [] and type(c['errors']) is list and c['expired'] is False and c['overflow'] is False, 'ORIGINAL_CAPTURE_FAILED')
    require(type(c['exit_code']) is int and c['exit_code'] == 0, 'ORIGINAL_CHILD_EXIT')
    require(type(c['original_budget_requests']) is int and 0 < c['original_budget_requests'] <= 65536
            and c['original_budget_refused'] is False and c['raw_files_fsynced'] is True
            and c['raw_directory_fsynced'] is True, 'ORIGINAL_LIVE_BUDGET_OR_PERSISTENCE')
    for name, raw in [('stdout', stdout), ('stderr', stderr)]:
        require(type(c[name + '_bytes']) is int and type(c[name + '_stored_bytes']) is int
                and c[name + '_bytes'] == c[name + '_stored_bytes'] == len(raw), 'LOG_PARTIAL')
        require(c[name + '_sha256'] == digest(raw), 'LOG_HASH')
    require(stderr == b'', 'CLI_STDERR')

def frames(raw):
    require(type(raw) is bytes and len(raw) <= LIMIT, 'LOG_BOUND')
    try:
        decoded = raw.decode('utf-8', 'strict')
    except UnicodeError:
        raise Refusal('LOG_ENCODING') from None
    found = {}
    for line in decoded.splitlines():
        require(len(line.encode('utf-8')) <= 65536, 'LOG_LINE_BOUND')
        if 'RUDDER_GENUINE_' not in line:
            continue
        matches = [(label, line.find(label)) for label in (ROOT, NATIVE) if label in line]
        require(len(matches) == 1, 'FOREIGN_OR_MULTIPLE_FRAME')
        label, pos = matches[0]
        require(line.count('RUDDER_GENUINE_') == 1 and label not in found, 'DUPLICATE_FRAME')
        # The future actual Fly output must qualify this finite plain-line envelope.
        # Quotes/braces/equals before the marker refuse source echoes/JSON wrappers.
        prefix = line[:pos]
        require(len(prefix) <= 512 and re.fullmatch(r'[A-Za-z0-9_ .:/+\[\]-]*', prefix) is not None, 'LOG_ENVELOPE')
        payload = line[pos + len(label):]
        found[label] = loads(payload, 4096)
    require(set(found) == {ROOT, NATIVE}, 'MISSING_FRAME')
    return found[ROOT], found[NATIVE]

def _project(expected, capture, stdout, stderr):
    exact(expected, EXPECTED_KEYS, 'EXPECTED_SCHEMA')
    require(type(expected['schema']) is int and expected['schema'] == 1, 'EXPECTED_SCHEMA')
    require(expected['case'] in LEAVES, 'CASE')
    feature, root_present, root_exact, refused, contention = LEAVES[expected['case']]
    require(expected['feature'] == feature, 'FEATURE')
    require(type(expected['experiment_nonce']) is str and UUID.fullmatch(expected['experiment_nonce']), 'EXPERIMENT_NONCE')
    require(expected['command_key'] == 'genuine-ingress:' + expected['experiment_nonce'], 'COMMAND_KEY')
    for key in ('app', 'machine', 'instance_id'):
        require(type(expected[key]) is str and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,79}', expected[key]), 'LIFECYCLE_BINDING')
    for key in ('source_manifest_sha256', 'root_receipt_manifest_sha256', 'fixture_manifest_sha256', 'native_elf_sha256', 'engine_elf_sha256', 'rpc_wire_sha256'):
        sha(expected[key])
    require(expected['root_receipt_manifest_sha256'] == ROOT_RECEIPT_MANIFEST_SHA256
            and expected['fixture_manifest_sha256'] == FIXTURE_MANIFEST_SHA256, 'FROZEN_FORMAT_PROVENANCE')
    require(type(expected['integrated_revision']) is str and re.fullmatch(r'[0-9a-f]{40}', expected['integrated_revision']), 'REVISION')
    require(type(expected['image_digest']) is str and re.fullmatch(r'sha256:[0-9a-f]{64}', expected['image_digest'])
            and expected['image_digest'] != 'sha256:' + '0' * 64, 'IMAGE')
    require(type(expected['session_generation']) is str and KEY.fullmatch(expected['session_generation']), 'GENERATION')
    require(expected['dispatch_disposition'] == 'effect_unknown' and type(expected['dispatch_attempts']) is int
            and expected['dispatch_attempts'] == 1 and type(expected['rpc_send_observations']) is int
            and expected['rpc_send_observations'] == 1 and expected['original_session_retained'] is True, 'CALLER_DISPOSITION')
    require(type(expected['rpc_wire']) is str, 'RPC_WIRE_TYPE')
    rpc = loads(expected['rpc_wire'])
    require(digest(expected['rpc_wire'].encode('utf-8')) == expected['rpc_wire_sha256'], 'RPC_WIRE_HASH')
    exact(rpc, ('type', 'id', 'method', 'params'), 'RPC_SCHEMA')
    require(rpc['type'] == 'rpc' and rpc['method'] == 'managed.openPreparedProjectV1'
            and type(rpc['id']) is str and KEY.fullmatch(rpc['id']), 'RPC_OPERATION')
    p = rpc['params']
    exact(p, ('command_key', 'project_alias', 'backend', 'model', 'profile_id'), 'RPC_PARAMS')
    require(p['command_key'] == expected['command_key'] and p['backend'] == 'codex', 'RPC_PARAMS')
    require(type(p['project_alias']) is str and KEY.fullmatch(p['project_alias']), 'PROJECT_ALIAS')
    for key in ('model', 'profile_id'):
        text(p[key], 256)
    capture_valid(capture, stdout, stderr)
    root, native = frames(stdout)
    exact(root, ROOT_KEYS, 'ROOT_SCHEMA')
    exact(native, NATIVE_KEYS, 'NATIVE_SCHEMA')
    for frame in (root, native):
        require(type(frame['schema']) is int and frame['schema'] == 1 and frame['case'] == expected['case'], 'FRAME_IDENTITY')
        for key in ('source_accepted', 'runtime_ready', 'retirement_complete'):
            require(frame[key] is False, 'FALSE_AUTHORITY')
        require(frame['reporting_worker_pending'] is True, 'REPORTER_PENDING')
    c = root['constraint']
    exact(c, ('format_version', 'maximum_milliseconds', 'boottime_deadline_nanoseconds'), 'CONSTRAINT_SCHEMA')
    require(type(c['format_version']) is int and c['format_version'] == 1
            and type(c['maximum_milliseconds']) is int and c['maximum_milliseconds'] == 20000
            and type(c['boottime_deadline_nanoseconds']) is str
            and re.fullmatch(r'[1-9][0-9]{0,19}', c['boottime_deadline_nanoseconds'])
            and int(c['boottime_deadline_nanoseconds']) <= 18446744073709551615, 'ORIGINAL_CONSTRAINT')
    c = {key: c[key] for key in ('format_version', 'maximum_milliseconds', 'boottime_deadline_nanoseconds')}
    operation = {'op': 'OpenPreparedProjectV1', **{key: p[key] for key in ('project_alias', 'backend', 'model', 'profile_id')}}
    # Exact insertion order matches the pinned image-owned host-control emit + PacketQueue.
    command = {'version': 1, 'session_generation': expected['session_generation'], 'rpc_id': rpc['id'],
               'command_key': expected['command_key'], 'operation': operation, 'constraint': c}
    packet = json.dumps(command, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
    require(root['canonical_command_sha256'] == digest(canonical(command)), 'CANONICAL_COMMAND_HASH')
    require(root['raw_packet_sha256'] == digest(packet) and type(root['raw_packet_bytes']) is int
            and root['raw_packet_bytes'] == len(packet) <= 32768, 'SIDECAR_PACKET_HASH')
    for key, raw in [('command_key_sha256', p['command_key'].encode()), ('rpc_id_sha256', rpc['id'].encode()),
                     ('operation_sha256', canonical(operation)), ('session_generation_sha256', expected['session_generation'].encode())]:
        require(root[key] == digest(raw), 'ROOT_COMPONENT_HASH')
    require(root['original_forward_outcome'] == 'KnownForwarded', 'ROOT_NOT_KNOWN_FORWARDED')
    for key in ('original_send_attempt_armed', 'original_pre_forward_sample_live', 'root_known_forwarded'):
        require(root[key] is True, 'ROOT_HISTORICAL_SAMPLE')
    for key in ('fixture_boundary_observed', 'current', 'original_job_retained', 'candidate_pre_effect', 'no_descendants',
                'coordinator_joined_including_tls', 'original_ingress_worker_pending', 'original_runtime_worker_pending', 'covered_roots_closed'):
        require(native[key] is True, 'NATIVE_HISTORICAL_SAMPLE')
    for key, value in [('root_present', root_present), ('exact_original_root_captured', root_exact),
                       ('original_root_refusal_retained', refused), ('actual_worker_carrier_would_block', contention)]:
        require(native[key] is value, 'LEAF_BOUNDARY')
    require(native['command_sha256'] == root['canonical_command_sha256']
            and native['session_generation_sha256'] == root['session_generation_sha256'], 'ROOT_NATIVE_MISMATCH')
    # Fixed projection has no raw command/profile/model, private error, or live/current claim.
    return {'schema': 1, 'kind': 'HISTORICAL_GENUINE_INGRESS_DATA', 'case': expected['case'],
            'experiment_nonce': expected['experiment_nonce'], 'feature': feature,
            'externally_supplied_lifecycle_data': {key: expected[key] for key in ('app', 'machine', 'instance_id', 'image_digest',
                'integrated_revision', 'source_manifest_sha256', 'root_receipt_manifest_sha256', 'fixture_manifest_sha256',
                'native_elf_sha256', 'engine_elf_sha256')},
            'rpc_wire_sha256': expected['rpc_wire_sha256'], 'canonical_command_sha256': root['canonical_command_sha256'],
            'root_frame_sha256': digest(canonical(root)), 'native_frame_sha256': digest(canonical(native)),
            'whole_stdout_sha256': digest(stdout), 'whole_stderr_sha256': digest(stderr),
            'source_accepted': False, 'runtime_ready': False, 'retirement_complete': False,
            'root_acknowledgment': False, 'serving': False, 'current_claim': False, 'qualified': False}


def project(expected, capture, stdout, stderr):
    try:
        return _project(expected, capture, stdout, stderr)
    except Refusal:
        raise
    except (KeyError, TypeError, ValueError, UnicodeError, OverflowError, RecursionError):
        raise Refusal('SCHEMA_VALUE') from None
