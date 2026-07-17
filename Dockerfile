FROM caddy:2.10-alpine AS caddy

FROM rust:1.96-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data lore-auth \
    && mkdir -p /data /certs /caddy-data /caddy-config \
    && chown -R lore-auth:lore-auth /data /certs /caddy-data /caddy-config
COPY --from=builder /build/target/release/lore-auth /usr/local/bin/lore-auth
COPY --from=caddy /usr/bin/caddy /usr/local/bin/caddy
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/entrypoint

ENV LORE_AUTH_URL="" \
    LORE_AUTH_PASSWORD="" \
    LORE_SERVER_URL="" \
    LORE_AUTH_TLS_MODE=manual

USER lore-auth
VOLUME ["/data", "/certs", "/caddy-data", "/caddy-config"]
EXPOSE 10443
HEALTHCHECK CMD ["curl", "--fail", "--silent", "--insecure", "https://127.0.0.1:10443/health"]
ENTRYPOINT ["/usr/bin/tini", "-g", "--", "/usr/local/bin/entrypoint"]
