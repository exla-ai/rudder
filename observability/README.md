# Rudder observability

Metrics, dashboards, alerts and a scaler for Rudder Cloud, so a home machine
never again falls over before anyone notices.

**Why it exists.** On 2026-11-02 the 16 GB home `home-53c2` ran ~100 Claude
processes (34 of them sessions resumed at boot and idle for 79 hours), 23
dashboards, dev servers and builds, with no swap. Memory pressure reached 75%
"full" (every running task stalled three quarters of the time), 0.7 GB was
available, load hit 235 on 4 CPUs, the supervisor starved, and every attached
terminal showed "Cloud machine reconnecting". Nothing told us; a user did.

**Status.** Built and running locally. Nothing is deployed: Rudder v4 is
mid-flight, and production rollout is a separate, deliberate step (see
"Going to production").

## How it fits together

```
 home machine (one per account)        control plane (rudder-cloud-control)
 cloud/worker/telemetry.mjs            cloud/src/telemetry.ts
   /proc, PSI, statfs, agent rows        requests, sockets, heartbeats
          |  OTLP/HTTP (Fly private network, bearer token)  |
          +-------------------------+-----------------------+
                                    v
                  rudder-observability (one Fly machine)
          OpenTelemetry Collector  ->  Prometheus (OTLP receiver, rules)
                                          |            ^
                                          v            | scrape /metrics
                                      Grafana      scaler (dry run)
                                          |
                  https://rudder.exla.ai/dashboard/  (Vercel rewrite)
```

- **Emitters** use the OpenTelemetry metrics SDK and are inert unless
  `RUDDER_OTEL_ENDPOINT` is set. They use `RUDDER_OTEL_*` names, never the
  standard `OTEL_*` ones, which would leak into every agent and send their
  own apps' telemetry to us. The supervisor strips both from agents' env.
- **Collector** authenticates, bounds memory, batches and forwards to
  Prometheus' native OTLP endpoint.
- **Prometheus** stores 30 days, promotes the resource attributes we query by
  (home id, machine, region, version) to labels, records ratios and evaluates
  alerts.
- **Grafana** dashboards are code (`grafana/build-dashboards.mjs`), provisioned
  read-only.
- **Scaler** reads Prometheus every minute and decides, per home, what would
  keep it healthy. Dry run by default.

The metric names, units and labels are a contract: [metrics.md](metrics.md).

## Run it locally

```sh
docker compose -f observability/docker-compose.yml up -d
open http://localhost:3300/dashboard/        # Grafana, anonymous viewer
open http://localhost:19090                  # Prometheus
curl -s localhost:19464/decisions            # the scaler's current decisions (dry run)
docker compose -f observability/docker-compose.yml down -v   # stop and forget
```

Host ports are Grafana 3300, Prometheus 19090, the collector 14318 and the
scaler 19464, because
9090 and 4318 are often taken by other local stacks.

What you will see:

- `home-local`: the real `cloud/worker/telemetry.mjs` inside a Linux
  container, reading that container's `/proc` (the production code path, end
  to end).
- A synthetic fleet with five hours of history (`sim/fleet-sim.mjs`), one home
  per thing we care about: `home-53c2` replays the 2026-11-02 incident (stall
  climbs to 75%, the control socket flaps, swap arrives, the machine grows to
  32 GB, the cleanup parks 28 agents); `home-b2e4` is nearly out of disk;
  `home-c91f` is under steady pressure after its cleanup already ran;
  `home-d0aa` is a 32 GB home that barely uses 4; `home-e5f3` stopped
  reporting 25 minutes ago.
- The scaler's decisions for that fleet, in dry run.

Regenerate dashboards after editing the builder:

```sh
node observability/grafana/build-dashboards.mjs          # writes grafana/dashboards/*.json
node observability/grafana/build-dashboards.mjs --check  # CI: fails on drift
```

Tests (no Docker needed) run with the root suite: `tests/cloud-telemetry`,
`tests/cloud-control-telemetry`, `tests/observability-dashboards`,
`tests/observability-scaler`.

## Alerts

`prometheus/rules/rudder.yml`. Prometheus evaluates them; there is no
Alertmanager yet, so they show up as firing in Grafana and in Prometheus'
`ALERTS` series, not in Slack.

| Alert | When | Meaning |
|---|---|---|
| HomeMemoryStalling | memory full stall > 10% for 10 min | the cleanup should be catching up; the scaler starts to want memory |
| HomeMemoryExhausted | > 30% for 3 min | thrashing; terminals show reconnecting |
| HomeMemoryAvailableLow | < 10% available for 10 min | |
| HomeSwapNearlyFull / HomeWithoutSwap | swap > 85% used / no swap for 30 min | |
| HomeDiskLow / HomeDiskCritical | /workspace < 10% / < 4% free | |
| HomeSupervisorStalling | supervisor event loop p99 > 0.5 s | the thing users feel directly |
| HomeControlFlapping | > 3 control disconnects in 15 min | |
| HomeHeartbeatStale | no heartbeat for 10 min and no control socket | expected while a home is stopped |
| ControlPlaneErrors / EventLoopSlow / MemoryHigh | 5xx > 5%, loop p99 > 200 ms, RSS > 800 MB on the 1 GB machine | |
| ScalerStale / ScalerWantsChange | no evaluation for 5 min / a resize or volume change wanted for 30 min | |

## The scaler

`scaler/`, plain Node with no dependencies. Every minute: collect per-home
inputs from Prometheus (`prometheus.mjs`), decide (`policy.mjs`, a pure
function), act only if allowed (`fly.mjs`), remember cooldowns (`state.mjs`),
and expose its decisions on `:9464/metrics` and `/decisions`.

The cheapest fix comes first, and nothing that restarts a machine happens
while its agents are working:

| Action | When | Effect |
|---|---|---|
| run-cleanup | memory short now, no cleanup in 15 min | recommend (a control-plane RPC to trigger it is a follow-up) |
| add-swap | no swap and memory short | recommend (new images add swap at boot) |
| scale-up-memory | stall sustained over 30 min and the cleanup already ran | next step of 16 / 24 / 32 GB, only when no agent worked for 15 min; 6 h cooldown |
| scale-down-memory | used < 40% and no stall for 24 h | previous step, same idle rule; 24 h cooldown |
| extend-volume | /workspace < 10% or < 8 GiB free | grow 50% (cap 500 GiB); never restarts; 12 h cooldown |
| investigate-silent | no samples for 10 min | recommend |

Applying needs both `RUDDER_SCALER_APPLY=1` and `FLY_API_TOKEN`. Before any
change it reads the machine from Fly and refuses unless that machine's own
config says it is this home (`RUDDER_HOME=1`, matching `RUDDER_WORKSPACE_ID`):
labels in Prometheus are self-reported and could be spoofed.

## Trust

Homes push over the Fly private network with one shared bearer token. Agents
on a home run as the same user as the supervisor and could read it, so the
token keeps strangers out, not a home's own processes; a malicious account
could send misleading numbers for its own or another home. The impact is
bounded: dashboards could mislead, and the scaler re-checks identity with Fly
and is capped by steps, idle gating and cooldowns. A per-home token issued by
the control plane is the follow-up if this ever matters.

## Going to production (not done)

1. `fly apps create rudder-observability` and
   `fly volumes create rudder_observability_data --app rudder-observability --region iad --size 20`.
2. Secrets: `fly secrets set --app rudder-observability RUDDER_OTEL_TOKEN=... GF_SECURITY_ADMIN_PASSWORD=... GF_AUTH_GOOGLE_CLIENT_ID=... GF_AUTH_GOOGLE_CLIENT_SECRET=...`
   (a Google OAuth client with redirect URI
   `https://rudder.exla.ai/dashboard/login/google`; sign-in is limited to
   exla.ai accounts).
3. Deploy from the repo root:
   `fly deploy --config observability/deploy/fly.toml --dockerfile observability/deploy/Dockerfile .`
4. Control plane: set `RUDDER_OTEL_ENDPOINT=http://rudder-observability.internal:4318`
   and the same `RUDDER_OTEL_TOKEN`. New homes get both at creation; existing
   homes on their next idle image update (`homeTelemetryEnv`, server.ts).
5. Route `rudder.exla.ai/dashboard` in `site/vercel.json` (Grafana answers both
   `/dashboard` and `/dashboard/`, so the site's `trailingSlash: false` is safe):
   ```json
   { "source": "/dashboard", "destination": "https://rudder-observability.fly.dev/dashboard/" },
   { "source": "/dashboard/:path*", "destination": "https://rudder-observability.fly.dev/dashboard/:path*" }
   ```
   in `rewrites`.
6. Watch the scaler in dry run for a week. Only then consider
   `RUDDER_SCALER_APPLY=1` plus a Fly token scoped to `rudder-workers-exla`.

## Follow-ups

- Alertmanager (or Grafana alerting) to Slack.
- A control-plane RPC so the scaler's `run-cleanup` can trigger the home's
  cleanup job instead of only recommending it.
- Per-home telemetry tokens.
- Logs and traces through the same collector.
