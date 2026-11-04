# Rudder metric catalog

This is the contract between the code that emits metrics (the home supervisor,
the control plane, the scaler) and everything that reads them (Prometheus
rules, Grafana dashboards, the scaler's queries). Change a name here first.

Emitters use the OpenTelemetry metrics SDK and push OTLP/HTTP to the
collector, which forwards to Prometheus' native OTLP receiver. Prometheus
translates OTel names with its default `UnderscoreEscapingWithSuffixes`
strategy: dots become underscores, the unit is appended (`By` -> `_bytes`,
`s` -> `_seconds`, `1` on a gauge -> `_ratio`), counters gain `_total`, and
annotation units like `{agent}` add nothing. The tables list both names;
dashboards and queries use the Prometheus name.

## Labels every series carries

Prometheus promotes these OTel resource attributes to labels
(`otlp.promote_resource_attributes` in `prometheus/prometheus.yml`):

| Resource attribute | Prometheus label | Set by |
|---|---|---|
| `service.name` | `job` | `rudder-home`, `rudder-control` |
| `service.instance.id` | `instance` | Fly machine id, else hostname |
| `service.version` | `service_version` | image / package version |
| `rudder.home.id` | `rudder_home_id` | home only: the account's home id (`RUDDER_WORKSPACE_ID`) |
| `fly.machine.id` | `fly_machine_id` | `FLY_MACHINE_ID` |
| `fly.region` | `fly_region` | `FLY_REGION` |
| `deployment.environment.name` | `deployment_environment_name` | `production`, `local`, `sim` |

The scaler scrapes its own `/metrics` endpoint (Prometheus text format), so its
series carry `job="rudder-scaler"` and the labels listed below.

## Home machine (`job="rudder-home"`, `cloud/worker/telemetry.mjs`)

Read from `/proc`, `statfs` and the supervisor's own state every
`RUDDER_OTEL_INTERVAL_MS` (default 30 s). Counters start at 0 at supervisor
start for every attribute set listed, so `increase()` sees the first event.

| OTel instrument | Kind | Unit | Attributes | Prometheus name |
|---|---|---|---|---|
| `rudder.home.memory.total` | gauge | By | | `rudder_home_memory_total_bytes` |
| `rudder.home.memory.available` | gauge | By | | `rudder_home_memory_available_bytes` |
| `rudder.home.swap.total` | gauge | By | | `rudder_home_swap_total_bytes` |
| `rudder.home.swap.free` | gauge | By | | `rudder_home_swap_free_bytes` |
| `rudder.home.pressure` | gauge | 1 | `resource` = memory\|io\|cpu, `kind` = some\|full, `window` = avg10\|avg60\|avg300 | `rudder_home_pressure_ratio` (0..1, share of time stalled) |
| `rudder.home.cpu.count` | gauge | {cpu} | | `rudder_home_cpu_count` |
| `rudder.home.load` | gauge | (none) | `window` = 1m\|5m\|15m | `rudder_home_load` |
| `rudder.home.disk.total` | gauge | By | `mount` = /workspace\|/ | `rudder_home_disk_total_bytes` |
| `rudder.home.disk.free` | gauge | By | `mount` | `rudder_home_disk_free_bytes` |
| `rudder.home.processes` | gauge | {process} | | `rudder_home_processes` |
| `rudder.home.process.memory` | gauge | By | `group` = claude\|codex\|opencode\|dashboard\|supervisor\|mcp\|build\|node\|other | `rudder_home_process_memory_bytes` (RSS summed per group) |
| `rudder.home.agents` | gauge | {agent} | `state` = working\|waiting\|idle\|asleep\|stopped | `rudder_home_agents` |
| `rudder.home.projects` | gauge | {project} | | `rudder_home_projects` |
| `rudder.home.supervisor.event_loop.delay` | gauge | s | `stat` = p50\|p99\|max | `rudder_home_supervisor_event_loop_delay_seconds` |
| `rudder.home.control.connected` | gauge | 1 | | `rudder_home_control_connected_ratio` (1 while the control socket is open) |
| `rudder.home.control.reconnects` | counter | {reconnect} | | `rudder_home_control_reconnects_total` |
| `rudder.home.cleanup.runs` | counter | {run} | `trigger` = hourly\|memory\|boot, `outcome` = ok\|failed\|timeout | `rudder_home_cleanup_runs_total` |
| `rudder.home.cleanup.actions` | counter | {action} | `action` = park\|stop | `rudder_home_cleanup_actions_total` |

Agent `state` meanings: `working` (a live turn), `waiting` (live, asking the
user), `idle` (live process, no turn), `asleep` (parked by the cleanup),
`stopped` (no process: done, paused, failed).

Process `group` rules (first match wins): agent CLIs (`claude`, `codex`,
`opencode`, including Claude team members under `claude/versions/`), the
`rudder-native` dashboards, the supervisor (`/opt/rudder-worker/`), MCP
helpers (`mcp` in the command line), builds (`rustc`, `cargo`, `cc1*`, `ld`,
`tsc`, `esbuild`, `webpack`, `next build`, `vite build`, `go`, `gcc`, `clang`),
other `node`, then `other`.

## Control plane (`job="rudder-control"`, `cloud/src/telemetry.ts`)

| OTel instrument | Kind | Unit | Attributes | Prometheus name |
|---|---|---|---|---|
| `http.server.request.duration` | histogram | s | `http.request.method`, `http.route` (normalized, ids replaced by `:id`), `http.response.status_code` | `http_server_request_duration_seconds_{bucket,sum,count}` (plain HTTP only: WebSocket upgrades never reach the request handler) |
| `rudder.control.homes` | gauge | {home} | `state` (queued, running, paused, stopped, failed) | `rudder_control_homes` |
| `rudder.control.home.state` | gauge | (none) | `rudder.home.id`, `state` | `rudder_control_home_state` (1 for the home's current state: queued, running, paused, stopped, failed; a stopped home is quiet on purpose) |
| `rudder.control.sails` | gauge | {sail} | `state` | `rudder_control_sails` |
| `rudder.control.home.heartbeat.age` | gauge | s | `rudder.home.id` | `rudder_control_home_heartbeat_age_seconds` (running homes only; a stopped home is not expected to beat) |
| `rudder.control.home.control_connected` | gauge | 1 | `rudder.home.id` | `rudder_control_home_control_connected_ratio` |
| `rudder.control.home.clients` | gauge | {client} | `rudder.home.id` | `rudder_control_home_clients` (attached terminals, phones, browsers) |
| `rudder.control.ws.connections` | gauge | {connection} | `role` = worker\|client\|home-control | `rudder_control_ws_connections` |
| `rudder.control.home.disconnects` | counter | {disconnect} | `rudder.home.id` | `rudder_control_home_disconnects_total` (home control socket closed: what users see as "reconnecting") |
| `rudder.control.event_loop.delay` | gauge | s | `stat` = p50\|p99\|max | `rudder_control_event_loop_delay_seconds` |
| `rudder.control.process.memory` | gauge | By | `kind` = rss\|heap | `rudder_control_process_memory_bytes` |
| `rudder.control.builders` | gauge | {builder} | `state` = leased\|idle\|creating\|failed\|destroying | `rudder_control_builders` (burst build pool; only when `RUDDER_BUILDERS=1`) |
| `rudder.control.builder.leases` | counter | {lease} | `outcome` = granted\|busy\|failed, `start` = warm\|cold\|none | `rudder_control_builder_leases_total` (busy means every builder of that home was leased) |

Attribute `rudder.home.id` on a data point becomes label `rudder_home_id`.

## Scaler (`job="rudder-scaler"`, `observability/scaler/`)

Exposed on `:9464/metrics`, scraped every 15 s.

| Prometheus name | Type | Labels | Meaning |
|---|---|---|---|
| `rudder_scaler_decision` | gauge | `rudder_home_id`, `action`, `state` | 1 for each current decision. `action` = run-cleanup\|add-swap\|scale-up-memory\|scale-down-memory\|extend-volume\|investigate-silent. `state` = recommended\|waiting-for-idle\|cooldown\|applied\|failed |
| `rudder_scaler_target_memory_bytes` | gauge | `rudder_home_id` | memory the scaler wants for the home (only while a resize is pending) |
| `rudder_scaler_evaluations_total` | counter | `outcome` = ok\|error | evaluation loops |
| `rudder_scaler_last_evaluation_timestamp_seconds` | gauge | | unix time of the last successful evaluation (0 before the first) |
| `rudder_scaler_apply_enabled` | gauge | | 1 when the scaler may change machines (`RUDDER_SCALER_APPLY=1`), 0 for dry run |
