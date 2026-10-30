#!/bin/bash
set -euo pipefail
umask 077
source_dir=$1
qualification=$2
owned_machine=$3
[[ "$qualification" =~ ^[0-9a-f-]{36}$ ]]
[[ "$owned_machine" =~ ^[0-9a-f]+$ && "$owned_machine" != 8edd225a7d0378 ]]
test "$(cat /run/rudder-fleet-qualification)" = "$qualification:$owned_machine"
work=/var/tmp/rudder-fleet-$qualification
test ! -e "$work"
mkdir -m 700 "$work"
chown postgres:postgres "$work"
out=$work/evidence
socket=$work/socket
data=$work/data
mkdir -m 700 "$out" "$socket"
chown postgres:postgres "$out" "$socket"
server_running=false
current_phase=initialization
pgbin=$(pg_config --bindir)

cleanup() {
  local guest_exit=$?
  if $server_running; then gosu postgres "$pgbin/pg_ctl" -D "$data" -m immediate -w stop >/dev/null 2>&1 || true; fi
  if test "$guest_exit" != 0 && test -d "$out"; then
    printf '{"guest_exit":%s,"partial":true,"phase":"%s"}\n' "$guest_exit" "$current_phase" >"$out/failure.json"
    rm -rf "$data" "$socket"
    printf '{"cluster_removed":true,"socket_removed":true,"partial_failure":true}\n' >"$out/guest-cleanup.json"
    tar -czf /var/tmp/rudder-fleet-evidence.tar.gz -C "$out" . || true
  fi
}
trap cleanup EXIT
gosu postgres "$pgbin/initdb" -D "$data" --auth-local=peer --auth-host=reject \
  --encoding=UTF8 --locale=C --data-checksums >"$out/initdb.log" 2>&1
cat >>"$data/postgresql.conf" <<CONFIG
listen_addresses = ''
unix_socket_directories = '$socket'
unix_socket_permissions = 0700
port = 55432
fsync = on
synchronous_commit = on
full_page_writes = on
shared_buffers = '128MB'
max_connections = 24
timezone = 'UTC'
search_path = 'rudder_fleet_design,pg_catalog'
log_line_prefix = '%m [%p] %a '
log_statement = 'all'
log_lock_waits = on
deadlock_timeout = '100ms'
CONFIG
gosu postgres "$pgbin/pg_ctl" -D "$data" -l "$out/postgresql.log" -w start >"$out/start.log" 2>&1
server_running=true
pg() {
  local application=$1
  shift
  gosu postgres "$pgbin/psql" -X -v ON_ERROR_STOP=1 \
    "host=$socket port=55432 dbname=postgres user=postgres application_name=$application" "$@"
}
sql() { pg observer -qAt -c "$1"; }
phase() { current_phase=$1; printf '{"phase":"%s","at":"%s"}\n' "$1" "$(date -u +%FT%TZ)"; }
session() {
  local label=$1
  local statement=$2
  printf '%s\n' "$statement" >"$out/$label.sql"
  (set +e; pg "$label" -a <"$out/$label.sql" >"$out/$label.log" 2>&1; printf '%s\n' "$?" >"$out/$label.exit") &
  session_pid=$!
}
await_state() {
  local label=$1
  local condition=$2
  for attempt in $(seq 1 100); do
    if test "$(sql "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name='$label' AND ($condition))")" = t; then
      sql "SELECT jsonb_build_object('application',application_name,'pid',pid,'wait_event_type',wait_event_type,'wait_event',wait_event,'blockers',pg_blocking_pids(pid),'at',clock_timestamp()) FROM pg_stat_activity WHERE application_name='$label'" >>"$out/concurrency.jsonl"
      return
    fi
    sleep 0.025
  done
  printf 'session did not reach expected state: %s\n' "$label" >&2
  return 1
}
exit_is() { test "$(cat "$out/$1.exit")" = "$2"; }
lock_then() { printf "BEGIN; SELECT q_lock('%s'); SELECT pg_sleep(1.5); %s COMMIT;\n" "$1" "$2"; }

phase schema
sed '$s/ROLLBACK;/COMMIT;/' "$source_dir/examples/fleet-authority/schema.sql" >"$out/installed-schema.sql"
pg install -a <"$out/installed-schema.sql" >"$out/install-schema.log" 2>&1
pg install -a <"$source_dir/documented-updates.sql" >"$out/install-updates.log" 2>&1
pg install -a <"$source_dir/examples/fleet-authority/qualify-protocol.sql" >"$out/install-protocol.log" 2>&1
sql "SELECT jsonb_build_object('version',version(),'server_version_num',current_setting('server_version_num'),'fsync',current_setting('fsync'),'synchronous_commit',current_setting('synchronous_commit'),'full_page_writes',current_setting('full_page_writes'),'listen_addresses',current_setting('listen_addresses'),'unix_socket_permissions',current_setting('unix_socket_permissions'),'data_checksums',current_setting('data_checksums'),'wal_sync_method',current_setting('wal_sync_method'),'boot_id',pg_read_file('/proc/sys/kernel/random/boot_id'))" >"$out/server.json"
sql "SELECT q_assert('durability-settings',current_setting('fsync')='on' AND current_setting('synchronous_commit')='on' AND current_setting('full_page_writes')='on'); SELECT q_assert('unix-socket-only',current_setting('listen_addresses')='' AND inet_server_addr() IS NULL AND current_setting('unix_socket_permissions')='0700');"
stat -c '%a %U %n' "$socket" >"$out/socket-permissions.txt"

phase competing-owner-claims
session acquire-a "$(lock_then acquire "INSERT INTO q_results VALUES('acquire-a',q_claim('acquire','a','boot-a',1,'claim-a'),pg_backend_pid(),clock_timestamp());")"
first=$session_pid
await_state acquire-a "wait_event='PgSleep'"
session acquire-b "INSERT INTO q_results(label,receipt) VALUES('acquire-b',q_claim('acquire','b','boot-b',1,'claim-b'));"
second=$session_pid
await_state acquire-b "wait_event_type='Lock'"
wait "$first" "$second"
exit_is acquire-a 0
exit_is acquire-b 0
sql "SELECT q_assert('one-owner-winner',(SELECT count(*)=1 FROM q_results WHERE label IN ('acquire-a','acquire-b') AND receipt->>'status'='accepted')); SELECT q_assert('one-owner-conflict',(SELECT count(*)=1 FROM q_results WHERE label IN ('acquire-a','acquire-b') AND receipt->>'status'='conflict')); SELECT q_assert('owner-epoch-two',(SELECT owner_epoch=2 AND event_seq=1 FROM projects WHERE project_id=q_project('acquire'))); SELECT q_assert('different-owner-session-pids',(SELECT count(DISTINCT backend_pid)=2 FROM q_results WHERE label IN ('acquire-a','acquire-b')));"

phase process-and-renewal-fences
sql "INSERT INTO q_results(label,receipt) VALUES('old-boot',q_renew('acquire','a','boot-old',2,0,'old-boot')),('new-process-old-grant',q_renew('acquire','a-new-process','boot-a',2,0,'old-process')),('stale-owner-epoch',q_renew('acquire','a','boot-a',1,0,'stale-epoch')),('renew-once',q_renew('acquire','a','boot-a',2,0,'renew-1')); INSERT INTO q_results(label,receipt) VALUES('renew-duplicate',q_renew('acquire','a','boot-a',2,0,'renew-1')),('renew-stale-serial',q_renew('acquire','a','boot-a',2,0,'renew-2'));"
sql "SELECT q_assert('reject-old-boot',(SELECT receipt->>'reason'='engine_boot' FROM q_results WHERE label='old-boot')); SELECT q_assert('reject-old-process',(SELECT receipt->>'reason'='engine_instance' FROM q_results WHERE label='new-process-old-grant')); SELECT q_assert('reject-stale-owner-epoch',(SELECT receipt->>'reason'='stale_epoch' FROM q_results WHERE label='stale-owner-epoch')); SELECT q_assert('renewal-duplicate-original',(SELECT a.receipt=b.receipt AND a.receipt->>'status'='renewed' FROM q_results a,q_results b WHERE a.label='renew-once' AND b.label='renew-duplicate')); SELECT q_assert('renewal-serial-once',(SELECT renew_serial=1 FROM projects WHERE project_id=q_project('acquire'))); SELECT q_assert('reject-stale-renewal-serial',(SELECT receipt->>'status'='conflict' FROM q_results WHERE label='renew-stale-serial'));"

phase row-lock-expiry-clock
sql "SELECT q_claim('clock','a','boot-a',1,'clock-claim',1000);" >"$out/clock-initial-receipt.json"
session clock-blocker "BEGIN; SELECT q_lock('clock'); SELECT pg_sleep(2); COMMIT;"
first=$session_pid
await_state clock-blocker "wait_event='PgSleep'"
session clock-waiter "BEGIN; SET LOCAL lock_timeout='3s'; SET LOCAL statement_timeout='4s'; INSERT INTO q_results(label,receipt) VALUES('clock-waiter',q_renew('clock','a','boot-a',2,0,'clock-renew')); COMMIT;"
second=$session_pid
await_state clock-waiter "wait_event_type='Lock'"
wait "$first" "$second"
exit_is clock-blocker 0
exit_is clock-waiter 0
sql "SELECT q_assert('clock-after-lock-rejects-expired',(SELECT receipt->>'status'='conflict' AND receipt->>'reason'='expired_or_serial' AND (receipt->>'checked_at')::timestamptz>(receipt->>'lease_until')::timestamptz FROM q_results WHERE label='clock-waiter')); SELECT q_assert('transaction-clock-counterexample',(SELECT (receipt->>'transaction_clock_would_allow')::boolean AND (receipt->>'transaction_started_at')::timestamptz<(receipt->>'lease_until')::timestamptz FROM q_results WHERE label='clock-waiter')); SELECT q_assert('expired-lease-not-renewed',(SELECT renew_serial=0 FROM projects WHERE project_id=q_project('clock')));"

phase expired-renew-versus-claim
session expired-renew "$(lock_then clock "INSERT INTO q_results(label,receipt) VALUES('expired-renew',q_renew('clock','a','boot-a',2,0,'late-renew'));")"
first=$session_pid
await_state expired-renew "wait_event='PgSleep'"
session expired-claim "INSERT INTO q_results(label,receipt) VALUES('expired-claim',q_claim('clock','b','boot-b',2,'replacement-claim'));"
second=$session_pid
await_state expired-claim "wait_event_type='Lock'"
wait "$first" "$second"
exit_is expired-renew 0
exit_is expired-claim 0
sql "SELECT q_assert('expired-renew-cannot-revive',(SELECT receipt->>'status'='conflict' FROM q_results WHERE label='expired-renew')); SELECT q_assert('replacement-claim-advances-epoch',(SELECT receipt->>'status'='accepted' AND receipt->>'owner_epoch'='3' FROM q_results WHERE label='expired-claim')); INSERT INTO q_results(label,receipt) VALUES('old-owner-after-transfer',q_renew('clock','a','boot-a',2,0,'post-transfer')); SELECT q_assert('old-owner-after-transfer-fenced',(SELECT receipt->>'reason'='stale_epoch' FROM q_results WHERE label='old-owner-after-transfer'));"

phase exact-head-and-topic-integration
for project in head topic candidate; do
  sql "SELECT q_claim('$project','a','boot-a',1,'initial-owner'); SELECT q_topic('$project','policy-1',0); SELECT q_verifications('$project');" >"$out/$project-setup.log"
done
session head-a "$(lock_then head "INSERT INTO q_results(label,receipt) VALUES('head-a',q_integrate('head','a','boot-a',2,'a','a','integration-a'));")"
first=$session_pid
await_state head-a "wait_event='PgSleep'"
session head-b "INSERT INTO q_results(label,receipt) VALUES('head-b',q_integrate('head','a','boot-a',2,'b','b','integration-b'));"
second=$session_pid
await_state head-b "wait_event_type='Lock'"
wait "$first" "$second"
exit_is head-a 0
exit_is head-b 0
sql "SELECT q_assert('one-integration-winner',(SELECT count(*)=1 FROM q_results WHERE label IN ('head-a','head-b') AND receipt->>'status'='accepted')); SELECT q_assert('one-integration-conflict',(SELECT count(*)=1 FROM q_results WHERE label IN ('head-a','head-b') AND receipt->>'status'='conflict')); SELECT q_assert('exact-head-once',(SELECT integration_version=1 AND head_vcs_commit='candidate-a' FROM projects WHERE project_id=q_project('head'))); SELECT q_assert('one-integration-record',(SELECT count(*)=1 FROM integration_commits WHERE project_id=q_project('head')));"
session topic-writer "$(lock_then topic "INSERT INTO q_results(label,receipt) VALUES('topic-writer',q_topic('topic','policy-2',1));")"
first=$session_pid
await_state topic-writer "wait_event='PgSleep'"
session topic-integrator "INSERT INTO q_results(label,receipt) VALUES('topic-integrator',q_integrate('topic','a','boot-a',2,'a','a','stale-policy-integration'));"
second=$session_pid
await_state topic-integrator "wait_event_type='Lock'"
wait "$first" "$second"
exit_is topic-writer 0
exit_is topic-integrator 0
sql "SELECT q_assert('changed-topic-invalidates-verification',(SELECT receipt->>'status'='conflict' FROM q_results WHERE label='topic-integrator')); SELECT q_assert('topic-conflict-preserves-head',(SELECT integration_version=0 AND head_vcs_commit='base' FROM projects WHERE project_id=q_project('topic'))); INSERT INTO q_results(label,receipt) VALUES('candidate-mismatch',q_integrate('candidate','a','boot-a',2,'b','a','mismatched-candidate')); SELECT q_assert('reject-untested-candidate',(SELECT receipt->>'status'='conflict' FROM q_results WHERE label='candidate-mismatch')); SELECT q_assert('candidate-conflict-no-acceptance',(SELECT count(*)=0 FROM idempotency WHERE project_id=q_project('candidate') AND operation_kind='integration'));"

phase discarded-commit-reply-and-reconnect
session discarded-reply "BEGIN; INSERT INTO q_results(label,receipt) VALUES('discarded-reply',q_claim('lostreply','a','boot-a',1,'lost-reply')); COMMIT; SELECT pg_sleep(30);"
first=$session_pid
await_state discarded-reply "wait_event='PgSleep'"
sql "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='discarded-reply';" >"$out/discarded-reply-termination.log"
wait "$first"
test "$(cat "$out/discarded-reply.exit")" != 0
sql "INSERT INTO q_results(label,receipt) VALUES('reconnected-retry',q_claim('lostreply','a','boot-a',1,'lost-reply')); SELECT q_assert('lost-reply-identical-receipt',(SELECT a.receipt=b.receipt FROM q_results a,q_results b WHERE a.label='discarded-reply' AND b.label='reconnected-retry')); SELECT q_assert('lost-reply-one-event',(SELECT event_seq=1 AND owner_epoch=2 FROM projects WHERE project_id=q_project('lostreply'))); INSERT INTO q_results(label,receipt) VALUES('changed-byte-key',q_claim('lostreply','b','boot-b',1,'lost-reply')); SELECT q_assert('changed-request-key-conflict',(SELECT receipt->>'reason'='idempotency_conflict' FROM q_results WHERE label='changed-byte-key'));"

phase client-crash-before-commit
session client-crash "BEGIN; INSERT INTO q_results(label,receipt) VALUES('client-crash-uncommitted',q_claim('client-crash','a','boot-a',1,'never-committed')); SELECT pg_sleep(30); COMMIT;"
first=$session_pid
await_state client-crash "wait_event='PgSleep'"
sql "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='client-crash';" >"$out/client-crash-termination.log"
wait "$first"
test "$(cat "$out/client-crash.exit")" != 0
sql "SELECT q_assert('client-crash-restores-project',(SELECT owner_epoch=1 AND event_seq=0 AND owner_engine_instance IS NULL FROM projects WHERE project_id=q_project('client-crash'))); SELECT q_assert('client-crash-no-receipt-or-outbox',NOT EXISTS(SELECT 1 FROM idempotency WHERE project_id=q_project('client-crash')) AND NOT EXISTS(SELECT 1 FROM outbox WHERE project_id=q_project('client-crash')));"

phase immediate-server-crash-and-wal-recovery
sql "SELECT jsonb_build_object('receipt',(SELECT accepted_receipt FROM idempotency WHERE project_id=q_project('lostreply') AND idempotency_key='lost-reply'),'wal_lsn',pg_current_wal_lsn(),'flushed_lsn',pg_current_wal_flush_lsn(),'at',clock_timestamp())" >"$out/pre-crash-witness.json"
session server-crash "BEGIN; INSERT INTO q_results(label,receipt) VALUES('server-crash-uncommitted',q_claim('server-crash','a','boot-a',1,'never-committed')); SELECT pg_sleep(30); COMMIT;"
first=$session_pid
await_state server-crash "wait_event='PgSleep'"
gosu postgres "$pgbin/pg_ctl" -D "$data" -m immediate -w stop >"$out/immediate-stop.log" 2>&1
server_running=false
wait "$first"
test "$(cat "$out/server-crash.exit")" != 0
gosu postgres "$pgbin/pg_controldata" -D "$data" >"$out/control-before-recovery.txt"
gosu postgres "$pgbin/pg_ctl" -D "$data" -l "$out/postgresql.log" -w start >"$out/recovery-start.log" 2>&1
server_running=true
sql "INSERT INTO q_results(label,receipt) VALUES('post-recovery-retry',q_claim('lostreply','a','boot-a',1,'lost-reply')); SELECT q_assert('server-crash-restores-project',(SELECT owner_epoch=1 AND event_seq=0 AND owner_engine_instance IS NULL FROM projects WHERE project_id=q_project('server-crash'))); SELECT q_assert('server-crash-no-partial-ledger',NOT EXISTS(SELECT 1 FROM events WHERE project_id=q_project('server-crash')) AND NOT EXISTS(SELECT 1 FROM idempotency WHERE project_id=q_project('server-crash')) AND NOT EXISTS(SELECT 1 FROM outbox WHERE project_id=q_project('server-crash'))); SELECT q_assert('committed-receipt-survives-crash',(SELECT a.receipt=b.receipt FROM q_results a,q_results b WHERE a.label='discarded-reply' AND b.label='post-recovery-retry')); SELECT q_assert('post-recovery-no-duplicate',(SELECT count(*)=1 FROM idempotency WHERE project_id=q_project('lostreply'))); SELECT q_assert('all-project-sequences-match-events',NOT EXISTS(SELECT 1 FROM projects p WHERE p.event_seq<>(SELECT count(*) FROM events e WHERE e.project_id=p.project_id)));"
sql "SELECT jsonb_build_object('receipt',(SELECT accepted_receipt FROM idempotency WHERE project_id=q_project('lostreply') AND idempotency_key='lost-reply'),'wal_lsn',pg_current_wal_lsn(),'at',clock_timestamp())" >"$out/post-crash-witness.json"
sql "SELECT jsonb_build_object('ok',true,'kind','single-server-postgresql-multisession','assertion_count',(SELECT count(*) FROM q_assertions),'assertions',(SELECT jsonb_agg(to_jsonb(a) ORDER BY label) FROM q_assertions a),'results',(SELECT jsonb_agg(to_jsonb(r) ORDER BY label) FROM q_results r),'limits','Synthetic privileged fixture; no application authentication, provider HA or RPO qualification')" >"$out/result.json"
gosu postgres "$pgbin/pg_ctl" -D "$data" -m fast -w stop >"$out/final-stop.log" 2>&1
server_running=false
rm -rf "$data" "$socket"
test ! -e "$data" && test ! -e "$socket"
printf '{"cluster_removed":true,"socket_removed":true,"server_stopped":true}\n' >"$out/guest-cleanup.json"
tar -czf /var/tmp/rudder-fleet-evidence.tar.gz -C "$out" .
sha256sum /var/tmp/rudder-fleet-evidence.tar.gz
cat "$out/result.json"
phase complete
