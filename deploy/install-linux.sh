#!/usr/bin/env bash
set -euo pipefail

# Install a release bundle on a Linux host. Credentials are supplied through
# the environment and are never written into this script or source control.
: "${ONGROK_ADMIN_TOKEN:?set ONGROK_ADMIN_TOKEN}"
: "${ONGROK_USER_TOKEN:?set ONGROK_USER_TOKEN}"
: "${ONGROK_TLS_CERT:?set ONGROK_TLS_CERT to a PEM full chain}"
: "${ONGROK_TLS_KEY:?set ONGROK_TLS_KEY to the matching PEM key}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR=/opt/ongrok/bin
ETC_DIR=/etc/ongrok
STATE_DIR=/var/lib/ongrok

install -d -m 0755 "$BIN_DIR" "$ETC_DIR" "$STATE_DIR"
install -m 0755 "$ROOT/target/x86_64-unknown-linux-musl/release/ongrok-relay-server" "$BIN_DIR/ongrok-relay-server"
install -m 0755 "$ROOT/target/x86_64-unknown-linux-musl/release/ongrok-relay-client" "$BIN_DIR/ongrok-relay-client"
install -m 0644 "$ONGROK_TLS_CERT" "$ETC_DIR/fullchain.pem"
install -m 0600 "$ONGROK_TLS_KEY" "$ETC_DIR/private.key"

getent group ongrok >/dev/null || groupadd --system ongrok
id ongrok >/dev/null 2>&1 || useradd --system --gid ongrok --home-dir "$STATE_DIR" --no-create-home ongrok
chown -R ongrok:ongrok "$STATE_DIR"

umask 077
cat > "$ETC_DIR/server.env" <<EOF
ONGROK_TLS_CERT=$ETC_DIR/fullchain.pem
ONGROK_TLS_KEY=$ETC_DIR/private.key
ONGROK_DB_PATH=$STATE_DIR/ongrok.redb
ONGROK_API_LISTEN=0.0.0.0:8080
ONGROK_QUIC_LISTEN=0.0.0.0:443
ONGROK_TCP_TLS_LISTEN=0.0.0.0:8443
ONGROK_HTTP_LISTEN=0.0.0.0:80
ONGROK_HTTPS_LISTEN=0.0.0.0:443
ONGROK_HTTP_DOMAIN=relay.lemonhx.moe
ONGROK_PUBLIC_HOST=relay.lemonhx.moe
ONGROK_TCP_PORT_START=20000
ONGROK_TCP_PORT_END=30000
ONGROK_ADMIN_TOKEN=$ONGROK_ADMIN_TOKEN
ONGROK_USER_TOKEN=$ONGROK_USER_TOKEN
EOF
chown root:ongrok "$ETC_DIR/server.env"
chmod 0640 "$ETC_DIR/server.env"

install -m 0644 "$ROOT/deploy/ongrok-relay-server.service" /etc/systemd/system/ongrok-relay-server.service
"$BIN_DIR/ongrok-relay-server" doctor
systemctl daemon-reload
systemctl enable --now ongrok-relay-server.service
systemctl --no-pager --full status ongrok-relay-server.service
