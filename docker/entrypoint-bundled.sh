#!/bin/sh
set -eu

case "${CADDY_TLS_MODE:-manual}" in
  auto)
    if [ -z "${LORE_AUTH_DOMAIN:-}" ]; then
      echo "LORE_AUTH_DOMAIN is required when CADDY_TLS_MODE=auto" >&2
      exit 1
    fi
    site="https://${LORE_AUTH_DOMAIN}:10443"
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
    echo "CADDY_TLS_MODE must be auto or manual" >&2
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
  echo "Lore Auth exited; stopping the bundled container" >&2
else
  echo "Caddy exited; stopping the bundled container" >&2
fi
exit 1
