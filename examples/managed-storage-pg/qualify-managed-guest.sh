#!/bin/bash
# ROOT-RUN ONLY inside one exact-owned disposable PostgreSQL image.
# No source compilation, packages, provider/client auth or model execution.
set -euo pipefail
umask 077
source_dir=$1
runtime_dir=$2
qualification=$3
owned=$4
[[ "$qualification" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]
[[ "$owned" =~ ^[0-9a-f]+$ && "$owned" != 8edd225a7d0378 ]]
test "$(cat /run/rudder-authority-node-qualification)" = "$qualification:$owned"
command -v python3 >/dev/null
command -v openssl >/dev/null
command -v gosu >/dev/null
pgbin=$(pg_config --bindir)
work=/var/tmp/rudder-managed-storage-pg-$qualification
test ! -e "$work"
mkdir -m 700 "$work"
chown postgres:postgres "$work"
out=$work/evidence
socket=$work/socket
data=$work/data
mkdir -m 700 "$out" "$socket" "$work/tls"
chown postgres:postgres "$out" "$socket" "$work/tls"
printf '%s\n' "$qualification" >"$work/qualification-nonce"
chown postgres:postgres "$work/qualification-nonce"
server_running=false
cleanup_verified=false
cleanup() {
  result=$?
  if $server_running; then
    if gosu postgres "$pgbin/pg_ctl" -D "$data" -m immediate -w stop >"$out/emergency-stop.log" 2>&1; then server_running=false; fi
  fi
  # Never remove a possibly live/uncertain cluster or claim normal runtime drain.
  if ! $server_running; then
    rm -rf -- "$data" "$socket"
    test ! -e "$data" && test ! -e "$socket" && cleanup_verified=true
  fi
  printf '{"fixture_exit":%s,"cluster_stop_confirmed":%s,"cluster_removed":%s,"normal_runtime_retirement":false,"exact_vm_teardown_required":true}\n' \
    "$result" "$(! $server_running && printf true || printf false)" "$cleanup_verified" >"$out/guest-cleanup.json"
  tar -czf /var/tmp/rudder-managed-storage-pg-evidence.tar.gz -C "$out" .
}
trap cleanup EXIT
gosu postgres "$pgbin/initdb" -D "$data" --auth-local=trust --auth-host=reject --encoding=UTF8 --locale=C --data-checksums >"$out/initdb.log" 2>&1
# Test-only CA/server certificate. SAN intentionally contains DNS localhost ONLY;
# production factory host127.0.0.1 must fail certificate hostname verification.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost -addext basicConstraints=critical,CA:TRUE \
  -keyout "$work/tls/server.key" -out "$work/tls/server.crt" >"$out/tls-generation.log" 2>&1
chmod 0600 "$work/tls/server.key"
chmod 0644 "$work/tls/server.crt"
chown postgres:postgres "$work/tls/server.key" "$work/tls/server.crt"
cat >>"$data/postgresql.conf" <<CONFIG
listen_addresses = 'localhost'
unix_socket_directories = '$socket'
unix_socket_permissions = 0700
port = 55432
ssl = on
ssl_min_protocol_version = 'TLSv1.2'
ssl_cert_file = '$work/tls/server.crt'
ssl_key_file = '$work/tls/server.key'
password_encryption = 'scram-sha-256'
fsync = on
synchronous_commit = on
full_page_writes = on
shared_buffers = '128MB'
max_connections = 64
timezone = 'UTC'
log_statement = 'none'
log_lock_waits = on
deadlock_timeout = '50ms'
CONFIG
cat >"$data/pg_hba.conf" <<'HBA'
local all postgres trust
hostnossl all all 127.0.0.1/32 reject
hostnossl all all ::1/128 reject
hostssl all all 127.0.0.1/32 scram-sha-256
hostssl all all ::1/128 scram-sha-256
HBA
chown postgres:postgres "$data/pg_hba.conf"
gosu postgres "$pgbin/pg_ctl" -D "$data" -l "$out/postgresql.log" -w start >"$out/start.log" 2>&1
server_running=true
password=$(openssl rand -hex 24)
gosu postgres "$pgbin/psql" -XAt -v ON_ERROR_STOP=1 "host=$socket port=55432 dbname=postgres user=postgres" \
  -c "ALTER ROLE postgres PASSWORD '$password'" >"$out/admin-password-set.log" 2>&1
state=$work/test-state.json
printf '{"nonce":"%s","work":"%s","socket":"%s","data":"%s","host":"localhost","port":55432,"ca":"%s","adminPassword":"%s"}\n' \
  "$qualification" "$work" "$socket" "$data" "$work/tls/server.crt" "$password" >"$state"
unset password
# Optional explicit ROOT-installed descriptor contains module paths/hashes ONLY.
# It never transports a credential, adds an HTTP Context issuer or runs by default.
if [[ -n "${5:-}" ]]; then
  python3 - "$state" "$5" <<'PYDATA'
import json,os,sys
from pathlib import Path
state=Path(sys.argv[1]);extra=json.loads(Path(sys.argv[2]).read_text())
assert set(extra)<= {'backendModule','followupModule'}
value=json.loads(state.read_text());value.update(extra)
with open(state,'w') as f:json.dump(value,f,separators=(',',':'));f.write('\n');f.flush();os.fsync(f.fileno())
PYDATA
fi
chmod 0600 "$state"
chown postgres:postgres "$state"
gosu postgres "$pgbin/psql" -XAt -v ON_ERROR_STOP=1 "host=$socket port=55432 dbname=postgres user=postgres" \
  -c "SELECT jsonb_build_object('version',version(),'fsync',current_setting('fsync'),'synchronous_commit',current_setting('synchronous_commit'),'full_page_writes',current_setting('full_page_writes'),'ssl',current_setting('ssl'),'listen_addresses',current_setting('listen_addresses'),'data_checksums',current_setting('data_checksums'))" >"$out/server.json"
chmod 0755 "$runtime_dir"
mkdir -p "$runtime_dir/tests"
cp "$source_dir/tests/managed-storage-pg-qualification.mjs" "$source_dir/tests/managed-storage-pg-fixtures.mjs" "$runtime_dir/tests/"
chmod 0755 "$runtime_dir/tests"
chmod 0644 "$runtime_dir/tests/"*.mjs
"$runtime_dir/node" --version >"$out/node-version.txt"
# The actual single Node child intentionally stays live with original Registry
# carriers. The supervisor observes completed proof, then performs declared
# fixture PROCESS teardown and actual wait, never normal Registry retirement.
python3 "$source_dir/examples/managed-storage-pg/retained-fixture-runner.py" \
  "$runtime_dir/node" "$runtime_dir/tests/managed-storage-pg-qualification.mjs" "$state" "$out"
test -s "$out/node-proof.json"
gosu postgres "$pgbin/pg_ctl" -D "$data" -m fast -w stop >"$out/final-stop.log" 2>&1
server_running=false
printf '{"ok":true,"scope":"real single PostgreSQL TLS/roles/transactions plus scoped fixture transports","normal_runtime_retirement":false}\n'
