# ROOT-BUILD ONLY. Component fixture; no Rudder worker/home/model entrypoint.
# Root first resolves/verifies the official postgres:17-bookworm amd64 digest
# using the existing vetted controller, then supplies that IMMUTABLE base.
ARG POSTGRES_IMAGE
FROM ${POSTGRES_IMAGE}
USER root
RUN apt-get update \
  && apt-get install -y --no-install-recommends python3 openssl \
  && rm -rf /var/lib/apt/lists/* \
  && test -x /usr/local/bin/gosu \
  && test -x /usr/bin/python3 \
  && test -x /usr/bin/openssl
# Exact final digest and source/OS package receipt belong to Root's build/run.
# No production image, PostgreSQL fleet durability or backend proof is implied.
ENTRYPOINT ["/bin/sleep"]
CMD ["900"]
