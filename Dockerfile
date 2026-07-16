FROM rust:1.96-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data lore-auth \
    && mkdir -p /data \
    && chown lore-auth:lore-auth /data
COPY --from=builder /build/target/release/lore-auth /usr/local/bin/lore-auth
USER lore-auth
VOLUME ["/data"]
EXPOSE 18080 15051
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:18080/health >/dev/null || exit 1
ENTRYPOINT ["lore-auth"]
CMD ["serve"]
