#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$ROOT/target/ongrok-e2e-$$"
mkdir -p "$TMP"

wait_for_health() {
  for _ in $(seq 1 50); do
    curl -fsS http://127.0.0.1:18080/healthz >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "control API did not become healthy" >&2
  return 1
}

wait_for_tcp_echo() {
  for _ in $(seq 1 50); do
    if python3 -c '
import socket
connection = socket.create_connection(("127.0.0.1", 19001), timeout=1)
connection.sendall(b"ongrok-e2e")
assert connection.recv(64) == b"ongrok-e2e"
connection.close()
' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "TCP relay did not echo the visitor payload" >&2
  return 1
}

wait_for_http_response() {
  for _ in $(seq 1 50); do
    response=$(curl -fsS -H 'Host: web.example.test' 'http://127.0.0.1:18081/hello?from=e2e' 2>/dev/null || true)
    if test "$response" = 'path=/hello?from=e2e host=web.example.test'; then
      return 0
    fi
    sleep 0.1
  done
  echo "HTTP relay did not return the expected proxied response" >&2
  return 1
}

wait_for_chunked_http_response() {
  for _ in $(seq 1 50); do
    response=$(printf 'chunk-one\nchunk-two\n' | curl --noproxy '*' --http1.1 -fsS \
      -H 'Host: web.example.test' -H 'Transfer-Encoding: chunked' -H 'Expect:' \
      --data-binary @- 'http://127.0.0.1:18081/upload?from=e2e' 2>/dev/null || true)
    if test "$response" = 'method=POST path=/upload?from=e2e host=web.example.test body=chunk-one|chunk-two|'; then
      return 0
    fi
    sleep 0.1
  done
  echo "chunked HTTP relay did not return the expected proxied response" >&2
  return 1
}

wait_for_https_response() {
  for _ in $(seq 1 50); do
    response=$(curl --noproxy '*' --http2 -kfsS --resolve secure.example.test:18082:127.0.0.1 \
      'https://secure.example.test:18082/secure?from=e2e' 2>/dev/null || true)
    if test "$response" = 'path=/secure?from=e2e host=secure.example.test:18082'; then
      return 0
    fi
    sleep 0.1
  done
  echo "HTTPS relay did not return the expected proxied response" >&2
  return 1
}

wait_for_node_offline() {
  for _ in $(seq 1 50); do
    if curl -fsS -H "Authorization: Bearer $ACTIVE_USER_TOKEN" http://127.0.0.1:18080/v1/nodes \
      | python3 -c 'import json, sys; assert json.load(sys.stdin)[0]["status"] == "Offline"' \
      >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "rotated token did not disconnect its node" >&2
  return 1
}

assert_tcp_relay_is_offline() {
  python3 -c '
import socket
try:
    connection = socket.create_connection(("127.0.0.1", 19001), timeout=1)
    connection.settimeout(1)
    connection.sendall(b"should-not-forward")
    assert connection.recv(64) != b"should-not-forward"
    connection.close()
except OSError:
    pass
'
}

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost \
  -addext basicConstraints=critical,CA:FALSE \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" >/dev/null 2>&1

SERVER_PID=0
ACTIVE_USER_TOKEN=user-test
CLIENT_PID=0
HTTP_CLIENT_PID=0
HTTPS_CLIENT_PID=0
BROKEN_CLIENT_PID=0
ECHO_PID=0
HTTP_FIXTURE_PID=0
trap 'kill "$SERVER_PID" "$CLIENT_PID" "$HTTP_CLIENT_PID" "$HTTPS_CLIENT_PID" "$BROKEN_CLIENT_PID" "$ECHO_PID" "$HTTP_FIXTURE_PID" 2>/dev/null || true' EXIT

"$ROOT/target/debug/ongrok-relay-server" run \
  --tls-cert "$TMP/cert.pem" --tls-key "$TMP/key.pem" \
  --db-path "$TMP/ongrok.redb" \
  --api-listen 127.0.0.1:18080 --quic-listen 127.0.0.1:14443 \
  --http-listen 127.0.0.1:18081 --http-domain example.test \
  --https-listen 127.0.0.1:18082 \
  --public-host 127.0.0.1 --tcp-port-start 19001 --tcp-port-end 19010 \
  --admin-token admin-test --user-token user-test >"$TMP/server.log" 2>&1 &
SERVER_PID=$!

python3 -u -c '
import socket
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 18088))
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
    def read_body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            chunks = []
            while True:
                size = int(self.rfile.readline().strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()
            return b"".join(chunks)
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length)
    def do_GET(self):
        body = "path={} host={}".format(self.path, self.headers.get("Host", "")).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_POST(self):
        body = self.read_body().decode()
        response = "method=POST path={} host={} body={}".format(
            self.path, self.headers.get("Host", ""), body.replace("\n", "|")
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)
    def log_message(self, *args):
        pass
ThreadingHTTPServer(("127.0.0.1", 18089), Handler).serve_forever()
' >"$TMP/http-fixture.log" 2>&1 &
HTTP_FIXTURE_PID=$!

wait_for_health

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/doctor-state" doctor \
  --server 127.0.0.1:14443 --server-name localhost --ca-cert "$TMP/cert.pem" \
  | grep -q '^relay=reachable$'

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/state" service publish \
  --server 127.0.0.1:14443 --server-name localhost --ca-cert "$TMP/cert.pem" \
  --token user-test --name demo --local-address 127.0.0.1:18088 --protocol tcp --public-port 19001 \
  >"$TMP/client.log" 2>&1 &
CLIENT_PID=$!

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/http-state" service publish \
  --server 127.0.0.1:14443 --server-name localhost --ca-cert "$TMP/cert.pem" \
  --token user-test --name web --local-address 127.0.0.1:18089 --protocol http \
  >"$TMP/http-client.log" 2>&1 &
HTTP_CLIENT_PID=$!

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/https-state" service publish \
  --server 127.0.0.1:14443 --server-name localhost --ca-cert "$TMP/cert.pem" \
  --token user-test --name secure --local-address 127.0.0.1:18089 --protocol https \
  >"$TMP/https-client.log" 2>&1 &
HTTPS_CLIENT_PID=$!

"$ROOT/target/debug/ongrok-relay-client" --state-dir "$TMP/broken-state" service publish \
  --server 127.0.0.1:14443 --server-name localhost --ca-cert "$TMP/cert.pem" \
  --token user-test --name broken --local-address 127.0.0.1:1 --protocol http \
  >"$TMP/broken-client.log" 2>&1 &
BROKEN_CLIENT_PID=$!

wait_for_tcp_echo
wait_for_http_response
wait_for_chunked_http_response
wait_for_https_response
curl --noproxy '*' --http2 -kfsS --resolve api.example.test:18082:127.0.0.1 \
  -H 'Authorization: Bearer user-test' \
  -X POST https://api.example.test:18082/v1/auth/validate \
  | grep -q '"kind":"user"'
status=$(curl -sS -o /dev/null -w '%{http_code}' \
  -H 'Host: unknown.example.test' \
  http://127.0.0.1:18081/should-not-route)
test "$status" = 502
status=$(curl -sS -o /dev/null -w '%{http_code}' \
  -H 'Host: broken.example.test' \
  http://127.0.0.1:18081/local-target-refused)
test "$status" = 502
"$ROOT/target/debug/ongrok-relay-client" services list \
  --server http://127.0.0.1:18080 --token user-test \
  | grep -q '"service_name":"demo"'

curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18080/v1/services \
  | grep -q '"service_name":"demo"'
curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18080/v1/services \
  | grep -q '"public_host":"web.example.test"'
service_id=$(curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18080/v1/services | python3 -c '
import json, sys
services = json.load(sys.stdin)
print(next(item["service_id"] for item in services if item["service_name"] == "demo"))
')
curl -fsS -X PATCH -H 'Authorization: Bearer user-test' -H 'Content-Type: application/json' \
  --data '{"metadata":{"environment":"e2e","owner":"relay-test"}}' \
  "http://127.0.0.1:18080/v1/services/$service_id" \
  | grep -q '"environment":"e2e"'
curl -fsS -H 'Authorization: Bearer user-test' "http://127.0.0.1:18080/v1/services/$service_id" \
  | grep -q '"owner":"relay-test"'
curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18080/v1/events | python3 -c '
import json, sys
kinds = {item["kind"] for item in json.load(sys.stdin)}
assert "NodeOnline" in kinds
assert "ServiceRegistered" in kinds
'
node_id=$(curl -fsS -H 'Authorization: Bearer user-test' http://127.0.0.1:18080/v1/nodes | python3 -c '
import json, sys
nodes = json.load(sys.stdin)
assert nodes and nodes[0]["status"] == "Online"
assert len(nodes[0]["public_key"]) == 32
print(nodes[0]["node_id"])
')
curl -fsS -H 'Authorization: Bearer user-test' "http://127.0.0.1:18080/v1/nodes/$node_id/metrics" \
  | grep -q '"cpu_percent"'
status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'Authorization: Bearer user-test' -H 'Content-Type: application/json' \
  --data '{"service_name":"missing","node_id":"00000000-0000-7000-8000-000000000000","protocol":"Tcp","local_address":"127.0.0.1:1"}' \
  http://127.0.0.1:18080/v1/services)
test "$status" = 404
status=$(curl -sS -o /dev/null -w '%{http_code}' http://127.0.0.1:18080/v1/services)
test "$status" != 200
oversized_body=$(python3 -c 'print("x" * 17000)')
status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'Authorization: Bearer user-test' -H 'Content-Type: application/json' \
  --data "{\"service_name\":\"$oversized_body\"}" \
  http://127.0.0.1:18080/v1/services)
test "$status" = 413
oversized_header=$(python3 -c 'print("h" * 70000)')
status=$(curl -sS -o /dev/null -w '%{http_code}' \
  -H 'Host: web.example.test' \
  -H "X-Ongrok-Large: $oversized_header" \
  http://127.0.0.1:18081/hello 2>/dev/null || true)
test "$status" = 431
ACTIVE_USER_TOKEN=$(curl -fsS -X POST http://127.0.0.1:18080/v1/admin/tokens/rotate \
  -H 'Authorization: Bearer admin-test' -H 'Content-Type: application/json' \
  --data '{"kind":"User"}' | python3 -c 'import json, sys; value=json.load(sys.stdin); assert value["kind"] == "user"; print(value["token"])')
status=$(curl -sS -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer user-test' http://127.0.0.1:18080/v1/services)
test "$status" = 401
wait_for_node_offline
curl -fsS -H "Authorization: Bearer $ACTIVE_USER_TOKEN" http://127.0.0.1:18080/v1/events | python3 -c '
import json, sys
kinds = {item["kind"] for item in json.load(sys.stdin)}
assert "TokenRotated" in kinds
assert "NodeOffline" in kinds
'
assert_tcp_relay_is_offline
status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $ACTIVE_USER_TOKEN" -H 'Content-Type: application/json' \
  --data "{\"service_name\":\"offline\",\"node_id\":\"$node_id\",\"protocol\":\"Tcp\",\"local_address\":\"127.0.0.1:1\"}" \
  http://127.0.0.1:18080/v1/services)
test "$status" = 409
curl -fsS -H "Authorization: Bearer $ACTIVE_USER_TOKEN" http://127.0.0.1:18080/v1/services \
  | grep -q '"service_name":"secure"'

kill "$CLIENT_PID" 2>/dev/null || true
wait "$CLIENT_PID" 2>/dev/null || true
CLIENT_PID=0
kill "$HTTP_CLIENT_PID" 2>/dev/null || true
wait "$HTTP_CLIENT_PID" 2>/dev/null || true
HTTP_CLIENT_PID=0
kill "$HTTPS_CLIENT_PID" 2>/dev/null || true
wait "$HTTPS_CLIENT_PID" 2>/dev/null || true
HTTPS_CLIENT_PID=0
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=0

"$ROOT/target/debug/ongrok-relay-server" run \
  --tls-cert "$TMP/cert.pem" --tls-key "$TMP/key.pem" \
  --db-path "$TMP/ongrok.redb" \
  --api-listen 127.0.0.1:18080 --quic-listen 127.0.0.1:14443 \
  --http-listen 127.0.0.1:18081 --http-domain example.test \
  --https-listen 127.0.0.1:18082 \
  --public-host 127.0.0.1 --tcp-port-start 19001 --tcp-port-end 19010 \
  --admin-token admin-test --user-token user-test >"$TMP/server-restart.log" 2>&1 &
SERVER_PID=$!
wait_for_health
curl -fsS -H "Authorization: Bearer $ACTIVE_USER_TOKEN" http://127.0.0.1:18080/v1/services \
  | grep -q '"service_name":"demo"'
curl -fsS -H "Authorization: Bearer $ACTIVE_USER_TOKEN" http://127.0.0.1:18080/v1/services \
  | grep -q '"public_host":"web.example.test"'
curl -fsS -H "Authorization: Bearer $ACTIVE_USER_TOKEN" http://127.0.0.1:18080/v1/events \
  | python3 -c 'import json, sys; assert any(item["kind"] == "ServiceRegistered" for item in json.load(sys.stdin))'
echo "QUIC_TCP_HTTP_RELAY_E2E_OK"
