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

# Declare the supported runtime settings in the final image so NAS Docker
# interfaces can pre-populate their environment-variable editor. Required
# deployment-specific values intentionally remain empty until configured.
ENV LORE_AUTH_DATA_DIR=/data \
    LORE_AUTH_HTTP_ADDR=0.0.0.0:18080 \
    LORE_AUTH_GRPC_ADDR=0.0.0.0:15051 \
    LORE_AUTH_PUBLIC_BASE_URL="" \
    LORE_AUTH_ISSUER="" \
    LORE_AUTH_AUDIENCE=lore-service \
    LORE_AUTH_ENVIRONMENT=local \
    LORE_AUTH_TOKEN_TTL_SECONDS=3600 \
    LORE_AUTH_LOGIN_TTL_SECONDS=300 \
    LORE_AUTH_LORE_GRPC_URL="" \
    LORE_AUTH_BOOTSTRAP_USERNAME=admin \
    LORE_AUTH_BOOTSTRAP_PASSWORD="" \
    RUST_LOG=lore_auth=info

USER lore-auth
VOLUME ["/data"]
EXPOSE 18080 15051
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:18080/health >/dev/null || exit 1
ENTRYPOINT ["lore-auth"]
CMD ["serve"]
