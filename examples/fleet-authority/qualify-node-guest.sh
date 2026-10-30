#!/bin/bash
set -euo pipefail
umask 077
source_dir=$1
runtime_dir=$2
qualification=$3
owned=$4
[[ "$qualification" =~ ^[0-9a-f-]{36}$ && "$owned" =~ ^[0-9a-f]+$ && "$owned" != 8edd225a7d0378 ]]
test "$(cat /run/rudder-authority-node-qualification)" = "$qualification:$owned"
# The separate public runtime archive includes its 0700 staging root entry.
chmod 0755 "$runtime_dir"
work=/var/tmp/rudder-authority-node-$qualification
test ! -e "$work"
mkdir -m 700 "$work"
chown postgres:postgres "$work"
out=$work/evidence
socket=$work/socket
data=$work/data
mkdir -m 700 "$out" "$socket"
chown postgres:postgres "$out" "$socket"
pgbin=$(pg_config --bindir)
server_running=false
phase=initialization
cleanup() {
  result=$?
  if $server_running; then gosu postgres "$pgbin/pg_ctl" -D "$data" -m immediate -w stop >/dev/null 2>&1 || true; fi
  if test "$result" != 0; then
    printf '{"guest_exit":%s,"phase":"%s"}\n' "$result" "$phase" >"$out/failure.json"
    rm -rf "$data" "$socket"
    printf '{"cluster_removed":true,"socket_removed":true,"partial_failure":true}\n' >"$out/guest-cleanup.json"
    tar -czf /var/tmp/rudder-authority-node-evidence.tar.gz -C "$out" . || true
  fi
}
trap cleanup EXIT
printf '%s\n' "$qualification" >"$work/qualification-nonce"
chown postgres:postgres "$work/qualification-nonce"
gosu postgres "$pgbin/initdb" -D "$data" --auth-local=trust --auth-host=reject --encoding=UTF8 --locale=C --data-checksums >"$out/initdb.log" 2>&1
cat >>"$data/postgresql.conf" <<CONFIG
listen_addresses = ''
unix_socket_directories = '$socket'
unix_socket_permissions = 0700
port = 55432
fsync = on
synchronous_commit = on
full_page_writes = on
shared_buffers = '128MB'
max_connections = 32
timezone = 'UTC'
log_line_prefix = '%m [%p] %a '
log_statement = 'all'
log_lock_waits = on
deadlock_timeout = '50ms'
CONFIG
gosu postgres "$pgbin/pg_ctl" -D "$data" -l "$out/postgresql.log" -w start >"$out/start.log" 2>&1
server_running=true
gosu postgres "$pgbin/psql" -XAt -v ON_ERROR_STOP=1 "host=$socket port=55432 dbname=postgres user=postgres" \
 -c "SELECT jsonb_build_object('version',version(),'fsync',current_setting('fsync'),'synchronous_commit',current_setting('synchronous_commit'),'full_page_writes',current_setting('full_page_writes'),'listen_addresses',current_setting('listen_addresses'),'socket_permissions',current_setting('unix_socket_permissions'),'data_checksums',current_setting('data_checksums'),'boot_id',pg_read_file('/proc/sys/kernel/random/boot_id'))" >"$out/server.json"
mkdir -p "$runtime_dir/tests"
chmod 0755 "$runtime_dir/tests"
cp "$source_dir"/tests/project-authority*.mjs "$runtime_dir/tests/"
chmod 644 "$runtime_dir"/tests/*.mjs
state=$work/test-state.json
printf '{"nonce":"%s","socket":"%s","data":"%s","pgbin":"%s","log":"%s"}\n' "$qualification" "$socket" "$data" "$pgbin" "$out/postgresql.log" >"$state"
chown postgres:postgres "$state"
"$runtime_dir/node" --version >"$out/node-version.txt"
phase=node-adapter-tests
set +e
RUDDER_AUTHORITY_PG_TEST_STATE="$state" gosu postgres "$runtime_dir/node" --test --test-reporter=tap "$runtime_dir"/tests/project-authority*.mjs >"$out/node-tests.tap.log" 2>&1
test_exit=$?
set -e
printf '%s\n' "$test_exit" >"$out/node-tests.exit"
test "$test_exit" = 0
cp "$work/node-proof.json" "$out/node-proof.json"
phase=cleanup
gosu postgres "$pgbin/pg_ctl" -D "$data" -m fast -w stop >"$out/final-stop.log" 2>&1
server_running=false
rm -rf "$data" "$socket"
test ! -e "$data" && test ! -e "$socket"
printf '{"cluster_removed":true,"socket_removed":true,"server_stopped":true,"node_test_exit":0}\n' >"$out/guest-cleanup.json"
tar -czf /var/tmp/rudder-authority-node-evidence.tar.gz -C "$out" .
printf '{"ok":true,"scope":"Node driver transactions on one isolated server; provider HA and RPO remain unqualified"}\n'
