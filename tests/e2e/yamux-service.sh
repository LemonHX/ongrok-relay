#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$ROOT/target/ongrok-yamux-e2e-$$"
mkdir -p "$TMP"

wait_for() {
  local command=$1
  for _ in $(seq 1 100); do
    eval "$command" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "timed out waiting for: $command" >&2
  return 1
}

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost \
  -addext basicConstraints=critical,CA:FALSE \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" >/dev/null 2>&1

SERVER_PID=0
TCP_CLIENT_PID=0
HTTP_CLIENT_PID=0
HTTPS_CLIENT_PID=0
ECHO_PID=0
HTTP_FIXTURE_PID=0
trap 'kill "$SERVER_PID" "$TCP_CLIENT_PID" "$HTTP_CLIENT_PID" "$HTTPS_CLIENT_PID" "$ECHO_PID" "$HTTP_FIXTURE_PID" 2>/dev/null || true' EXIT

"$ROOT/target/debug/ongrok-relay-server" run \
  --tls-cert "$TMP/cert.pem" --tls-key "$TMP/key.pem" \
  --db-path "$TMP/ongrok.redb" \
  --api-listen 127.0.0.1:18180 --quic-listen 127.0.0.1:14543 \
  --tcp-tls-listen 127.0.0.1:14544 \
  --http-listen 127.0.0.1:18181 --http-domain example.test \
  --https-listen 127.0.0.1:18182 \
  --public-host 127.0.0.1 --tcp-port-start 19101 --tcp-port-end 19110 \
  --admin-token admin-test --user-token user-test >"$TMP/server.log" 2>&1 &
SERVER_PID=$!

python3 -u -c '
import socket
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 18188))
listener.listen()
while True:
    connection, _ = listener.accept()
    while data := connection.recv(65536):
        connection.sendall(data)
    connection.close()
' >"$TMP/echo.log" 2>&1 &
ECHO_PID=$!

python3 -u -c '
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = ("yamux=1 path={} host={}".format(self.path, self.headers.get("Host", ""))).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
ThreadingHTTPServer(("127.0.0.1", 18189), Handler).serve_forever()
' >"$TMP/http-fixture.log" 2>&1 &
HTTP_FIXTURE_PID=$!

wait_for 'curl -fsS http://127.0.0.1:18180/healthz'

# Port 14545 has no UDP listener, forcing the client onto TCP/TLS Yamux at 14544.
"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/tcp-state" service publish \
  --server 127.0.0.1:14545 --tcp-tls-server 127.0.0.1:14544 \
  --server-name localhost --ca-cert "$TMP/cert.pem" --token user-test \
  --name yamux-tcp --local-address 127.0.0.1:18188 --protocol tcp --public-port 19101 \
  >"$TMP/tcp-client.log" 2>&1 &
TCP_CLIENT_PID=$!

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/http-state" service publish \
  --server 127.0.0.1:14545 --tcp-tls-server 127.0.0.1:14544 \
  --server-name localhost --ca-cert "$TMP/cert.pem" --token user-test \
  --name yamux-web --local-address 127.0.0.1:18189 --protocol http \
  >"$TMP/http-client.log" 2>&1 &
HTTP_CLIENT_PID=$!

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/https-state" service publish \
  --server 127.0.0.1:14545 --tcp-tls-server 127.0.0.1:14544 \
  --server-name localhost --ca-cert "$TMP/cert.pem" --token user-test \
  --name yamux-secure --local-address 127.0.0.1:18189 --protocol https \
  >"$TMP/https-client.log" 2>&1 &
HTTPS_CLIENT_PID=$!

wait_for "python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\", 19101), timeout=1); s.sendall(b\"yamux-e2e\"); assert s.recv(64) == b\"yamux-e2e\"'"
wait_for "test \"\$(curl -fsS -H 'Host: yamux-web.example.test' http://127.0.0.1:18181/hello)\" = 'yamux=1 path=/hello host=yamux-web.example.test'"
wait_for "test \"\$(curl --noproxy '*' -kfsS --resolve yamux-secure.example.test:18182:127.0.0.1 https://yamux-secure.example.test:18182/secure)\" = 'yamux=1 path=/secure host=yamux-secure.example.test:18182'"

curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18180/v1/services | grep -q '"transport":"TcpTlsYamux"'
curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18180/v1/nodes | grep -q '"transport":"TcpTlsYamux"'
echo "TCP_TLS_YAMUX_RELAY_E2E_OK"
