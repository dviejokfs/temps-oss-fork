# MariaDB + WAL-G image for direct-to-S3 physical backups and restore proof.
#
# Build:
#   docker build --build-arg MARIADB_VERSION=11.4 -t gotempsh/mariadb-walg:11.4 .
#
# The MySQL WAL-G binary invokes mariadb-backup through
# WALG_STREAM_CREATE_COMMAND. The resulting mbstream never lands on the Temps
# host filesystem.

ARG MARIADB_VERSION=11.4
FROM mariadb:${MARIADB_VERSION}

ARG WALG_VERSION=v3.0.8
ARG TARGETARCH

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl ca-certificates mariadb-backup; \
    WALG_ARCH="${TARGETARCH}"; \
    if [ "$WALG_ARCH" = "arm64" ]; then WALG_ARCH="aarch64"; fi; \
    curl -fSL "https://github.com/wal-g/wal-g/releases/download/${WALG_VERSION}/wal-g-mysql-24.04-${WALG_ARCH}.tar.gz" \
      -o /tmp/wal-g.tar.gz; \
    tar -xzf /tmp/wal-g.tar.gz -C /usr/local/bin/; \
    mv /usr/local/bin/wal-g-mysql-* /usr/local/bin/wal-g; \
    chmod +x /usr/local/bin/wal-g; \
    rm -f /tmp/wal-g.tar.gz; \
    apt-get purge -y curl; \
    apt-get autoremove -y; \
    rm -rf /var/lib/apt/lists/*; \
    wal-g --version; \
    mariadb-backup --version
