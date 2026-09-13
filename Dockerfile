# syntax=docker/dockerfile:1
# Static musl build in an Alpine stage, shipped in a scratch image containing
# only the binary and an empty, writable /data directory.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY docs/agent.md ./docs/agent.md
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/orcis /orcis
RUN mkdir /data

FROM scratch
COPY --from=build /orcis /orcis
COPY --from=build --chown=65534:65534 /data /data

# Loopback is the safe default for a local binary; a container must listen on
# all interfaces. The SQLite database lives under /data; mount a volume there
# that is writable by uid 65534 (the process is not root) to keep it.
ENV ORCIS_ADDR=0.0.0.0:8080
ENV ORCIS_DB_PATH=/data/orcis.db
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/orcis"]
