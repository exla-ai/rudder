# One Fly machine for the whole stack: collector, Prometheus, Grafana and the
# scaler, supervised by deploy/run.mjs (any process exiting exits the machine
# so Fly restarts it). Built from the repo root:
#   fly deploy --config observability/deploy/fly.toml --dockerfile observability/deploy/Dockerfile .
# Not deployed yet; see observability/README.md ("Going to production").
FROM otel/opentelemetry-collector-contrib:0.162.0 AS collector
FROM prom/prometheus:v3.15.0 AS prometheus

FROM grafana/grafana:13.2.4
USER root
# The scaler and the supervisor are plain Node ESM with no dependencies.
RUN apk add --no-cache nodejs su-exec
COPY --from=collector /otelcol-contrib /usr/local/bin/otelcol-contrib
COPY --from=prometheus /bin/prometheus /usr/local/bin/prometheus
COPY --from=prometheus /bin/promtool /usr/local/bin/promtool

COPY observability/collector/config.yaml /etc/otelcol/config.yaml
COPY observability/prometheus/prometheus.yml /etc/prometheus/prometheus.yml
COPY observability/prometheus/rules /etc/prometheus/rules
COPY observability/prometheus/targets-fly /etc/prometheus/targets
COPY observability/grafana/provisioning /etc/grafana/provisioning
COPY observability/grafana/dashboards /var/lib/grafana/dashboards
COPY observability/scaler /opt/rudder-scaler
COPY observability/deploy/run.mjs /opt/rudder-observability/run.mjs

# Prometheus and Grafana state live on the volume mounted at /data.
ENV PROMETHEUS_URL=http://localhost:9090 \
    PROMETHEUS_OTLP_ENDPOINT=http://localhost:9090/api/v1/otlp \
    RUDDER_OTEL_ENVIRONMENT=production \
    GF_PATHS_DATA=/data/grafana \
    GF_SERVER_HTTP_PORT=3000
ENTRYPOINT ["node", "/opt/rudder-observability/run.mjs"]
