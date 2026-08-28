#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$ROOT/target/ongrok-reconnect-e2e-$$"
mkdir -p "$TMP"

SERVER_PID=""
CLIENT_PID=""
ECHO_PID=""

cleanup() {
  for pid in "$SERVER_PID" "$CLIENT_PID" "$ECHO_PID"; do
    if test -n "$pid" && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT

wait_for_health() {
  for _ in $(seq 1 80); do
    curl -fsS http://127.0.0.1:18280/healthz >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "control API did not become healthy" >&2
  return 1
}

wait_for_echo() {
  for _ in $(seq 1 120); do
    if python3 -c '
import socket
s = socket.create_connection(("127.0.0.1", 19201), timeout=1)
s.sendall(b"reconnect-e2e")
assert s.recv(64) == b"reconnect-e2e"
s.close()
' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "TCP relay did not recover after server restart" >&2
  return 1
}

wait_for_node_online() {
  for _ in $(seq 1 120); do
    if curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18280/v1/nodes \
      | python3 -c 'import json, sys; nodes=json.load(sys.stdin); assert len(nodes) == 1; assert nodes[0]["status"] == "Online"' \
      >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "client did not re-register an online node" >&2
  return 1
}

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  >/dev/null 2>&1

start_server() {
  "$ROOT/target/debug/ongrok-relay-server" run \
    --tls-cert "$TMP/cert.pem" --tls-key "$TMP/key.pem" --db-path "$TMP/ongrok.redb" \
    --api-listen 127.0.0.1:18280 --quic-listen 127.0.0.1:14643 --tcp-tls-listen 127.0.0.1:14644 \
    --public-host 127.0.0.1 --tcp-port-start 19201 --tcp-port-end 19210 \
    --admin-token admin-test --user-token user-test >"$TMP/server.log" 2>&1 &
  SERVER_PID=$!
  wait_for_health
}

python3 -u -c '
import socket
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 18288))
listener.listen()
while True:
    connection, _ = listener.accept()
    while data := connection.recv(65536):
        connection.sendall(data)
    connection.close()
' >"$TMP/echo.log" 2>&1 &
ECHO_PID=$!

start_server
"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/state" service publish \
  --server 127.0.0.1:14643 --tcp-tls-server 127.0.0.1:14644 --server-name localhost \
  --ca-cert "$TMP/cert.pem" --token user-test --name reconnect --local-address 127.0.0.1:18288 \
  --protocol tcp --public-port 19201 >"$TMP/client.log" 2>&1 &
CLIENT_PID=$!

wait_for_echo
kill "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
start_server
wait_for_node_online
wait_for_echo
echo "CLIENT_RECONNECT_AND_TCP_LEASE_E2E_OK"
