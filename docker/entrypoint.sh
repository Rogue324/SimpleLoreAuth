#!/bin/sh
set -eu

public_url="${LORE_AUTH_PUBLIC_BASE_URL:-${LORE_AUTH_URL:-}}"
if [ -z "$public_url" ]; then
  echo "LORE_AUTH_URL is required (for example: https://auth.example.com:10443)" >&2
  exit 1
fi
case "$public_url" in
  https://*) ;;
  *)
    echo "LORE_AUTH_URL must start with https://" >&2
    exit 1
    ;;
esac

url_authority="${public_url#*://}"
url_authority="${url_authority%%/*}"
public_domain="${url_authority%%:*}"

# Four concise Docker settings are mapped to the service's detailed settings.
# The old variable names remain accepted as advanced compatibility overrides.
export LORE_AUTH_DATA_DIR="${LORE_AUTH_DATA_DIR:-/data}"
export LORE_AUTH_HTTP_ADDR="${LORE_AUTH_HTTP_ADDR:-127.0.0.1:18080}"
export LORE_AUTH_GRPC_ADDR="${LORE_AUTH_GRPC_ADDR:-127.0.0.1:15051}"
export LORE_AUTH_PUBLIC_BASE_URL="$public_url"
export LORE_AUTH_ISSUER="${LORE_AUTH_ISSUER:-$public_url}"
export LORE_AUTH_AUDIENCE="${LORE_AUTH_AUDIENCE:-$public_domain}"
export LORE_AUTH_ENVIRONMENT="${LORE_AUTH_ENVIRONMENT:-local}"
export LORE_AUTH_TOKEN_TTL_SECONDS="${LORE_AUTH_TOKEN_TTL_SECONDS:-3600}"
export LORE_AUTH_LOGIN_TTL_SECONDS="${LORE_AUTH_LOGIN_TTL_SECONDS:-300}"
export LORE_AUTH_LORE_GRPC_URL="${LORE_AUTH_LORE_GRPC_URL:-${LORE_SERVER_URL:-}}"
export LORE_AUTH_BOOTSTRAP_USERNAME="${LORE_AUTH_BOOTSTRAP_USERNAME:-admin}"
export LORE_AUTH_BOOTSTRAP_PASSWORD="${LORE_AUTH_BOOTSTRAP_PASSWORD:-${LORE_AUTH_PASSWORD:-}}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-/caddy-data}"
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-/caddy-config}"
export RUST_LOG="${RUST_LOG:-lore_auth=info}"

tls_mode="${CADDY_TLS_MODE:-${LORE_AUTH_TLS_MODE:-manual}}"
case "$tls_mode" in
  auto)
    domain="${LORE_AUTH_DOMAIN:-$public_domain}"
    if [ -z "$domain" ]; then
      echo "Could not derive the certificate domain from LORE_AUTH_URL" >&2
      exit 1
    fi
    site="https://${domain}:10443"
    tls_directive=""
    ;;
  manual)
    if [ ! -r "${CADDY_CERT_FILE:-/certs/server.pem}" ] || [ ! -r "${CADDY_KEY_FILE:-/certs/server.key}" ]; then
      echo "Caddy certificate or key is not readable" >&2
      exit 1
    fi
    site="https://:10443"
    tls_directive="tls ${CADDY_CERT_FILE:-/certs/server.pem} ${CADDY_KEY_FILE:-/certs/server.key}"
    ;;
  *)
    echo "LORE_AUTH_TLS_MODE must be auto or manual" >&2
    exit 1
    ;;
esac

cat > /tmp/Caddyfile <<EOF
${site} {
    ${tls_directive}

    @grpc header Content-Type application/grpc*

    handle @grpc {
        reverse_proxy 127.0.0.1:15051 {
            transport http {
                versions h2c
            }
        }
    }

    handle {
        reverse_proxy 127.0.0.1:18080
    }
}
EOF

caddy validate --config /tmp/Caddyfile --adapter caddyfile

auth_pid=""
caddy_pid=""

cleanup() {
  trap - EXIT TERM INT
  [ -z "$auth_pid" ] || kill "$auth_pid" 2>/dev/null || true
  [ -z "$caddy_pid" ] || kill "$caddy_pid" 2>/dev/null || true
  [ -z "$auth_pid" ] || wait "$auth_pid" 2>/dev/null || true
  [ -z "$caddy_pid" ] || wait "$caddy_pid" 2>/dev/null || true
}

trap cleanup EXIT
trap 'exit 0' TERM INT

lore-auth serve &
auth_pid=$!
caddy run --config /tmp/Caddyfile --adapter caddyfile &
caddy_pid=$!

while kill -0 "$auth_pid" 2>/dev/null && kill -0 "$caddy_pid" 2>/dev/null; do
  sleep 1
done

if ! kill -0 "$auth_pid" 2>/dev/null; then
  echo "Lore Auth exited; stopping the container" >&2
else
  echo "Caddy exited; stopping the container" >&2
fi
exit 1
