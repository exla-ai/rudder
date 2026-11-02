# ROOT BUILD ONLY. Separate control/PG fixture, NEVER a managed worker image.
FROM registry.fly.io/rudder-workers-exla@sha256:b0c21439e441671351766e8a2f4e87810887a7d6add0bac4959d7c16388143a4
USER root
COPY --chown=0:0 --chmod=0555 payload/node /usr/local/bin/rudder-runtime-qualification-node
COPY --chown=0:0 --chmod=0555 payload/flyctl /workspace/home/.fly/bin/flyctl
COPY --chown=0:0 payload/cloud /opt/rudder-runtime-qualification/cloud
COPY --chown=0:0 payload/examples /opt/rudder-runtime-qualification/examples
COPY --chown=0:0 --chmod=0444 payload/context-manifest.json /opt/rudder-runtime-qualification/context-manifest.json
RUN test -x /usr/lib/postgresql/17/bin/postgres \
 && test -x /usr/local/bin/gosu && test -x /usr/bin/openssl && test -x /usr/bin/python3 \
 && chmod -R go-w /opt/rudder-runtime-qualification \
 && mkdir -p /run/rudder-runtime-qualification/receipts /var/lib/rudder-runtime-qualification \
 && chmod 0700 /run/rudder-runtime-qualification /run/rudder-runtime-qualification/receipts /var/lib/rudder-runtime-qualification \
 && mkdir /var/lib/rudder-runtime-qualification-pg \
 && chgrp postgres /var/lib/rudder-runtime-qualification-pg && chmod 0710 /var/lib/rudder-runtime-qualification-pg
# Future separately enabled target mechanics check. Never calls Fly or listener mains.
RUN /usr/local/bin/rudder-runtime-qualification-node /opt/rudder-runtime-qualification/examples/managed-runtime-image-staging/genuine-ingress-target-check.mjs
EXPOSE 8080
ENTRYPOINT ["/usr/bin/python3","/opt/rudder-runtime-qualification/examples/managed-runtime-image-staging/supervise.py"]
CMD []
