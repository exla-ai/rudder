FROM registry.fly.io/rudder-workers-exla@sha256:2ebaf5f8124bbbe1b51850e6dcf60ccd6a6841d0bde92a5bb3afe77db1ba2420
USER root
COPY --chown=0:0 --chmod=0555 rudder-remote-execution-qualifier /usr/local/bin/rudder-remote-execution-qualifier
COPY --chown=0:0 --chmod=0444 remote-execution-provenance.json /opt/rudder/qualification/remote-execution-provenance.json
RUN chmod 0555 /opt/rudder/qualification && test -x /usr/bin/python3
