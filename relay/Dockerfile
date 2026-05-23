# Minimal Bun image for the claudetalk relay.
#
# Build context is repo root because the relay imports from ../../src/
# (shared protocol + crypto types). Build from the repo root:
#   docker build -f relay/Dockerfile -t claudetalk-relay .
#
# Run:
#   docker run -d -p 7878:7878 \
#     -e RELAY_SHARED_SECRET="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')" \
#     -v claudetalk-relay-data:/data \
#     -e RELAY_DB_PATH=/data/relay_db.sqlite \
#     claudetalk-relay
FROM oven/bun:1.3 AS runtime

WORKDIR /app

# Copy package metadata first for layer caching.
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile --production

# Then sources. The relay needs both relay/ and src/ (shared types).
COPY src ./src
COPY relay ./relay

# Default config: relay listens on 0.0.0.0:7878 with SQLite at /data.
ENV RELAY_HOST=0.0.0.0 \
    RELAY_PORT=7878 \
    RELAY_DB_PATH=/data/relay_db.sqlite \
    RELAY_RETENTION_DAYS=30

VOLUME ["/data"]
EXPOSE 7878

# RELAY_SHARED_SECRET MUST be provided via env/secret at run time.
# Container will exit(2) if it's missing.
CMD ["bun", "run", "relay/src/index.ts"]
