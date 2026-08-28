# ongrok relay

ongrok is a self-hosted reverse relay with a Rust server, Rust client, and a
small shared protocol library. QUIC is preferred for multiplexed streams;
TCP/TLS with Yamux is used when UDP is unavailable.

## Workspace

- `crates/libongrok`: wire frames, IDs, metadata, QUIC/Yamux I/O adapters
- `crates/ongrok-relay-server`: relay listeners, Hyper ingress, redb control plane
- `crates/ongrok-relay-client`: node identity, local forwarding, reconnect logic
- `frontend`: React/Vite console with i18n, themes, metrics, and events
- `deploy`: systemd, launchd, and environment templates

## Quick start

Build the binaries:

```sh
cargo build --workspace --release
```

Initialize a server database and keep the printed long-lived tokens private:

```sh
ongrok-relay-server init --db-path ./ongrok.redb
```

Start the server with a PEM full chain and matching private key:

```sh
export ONGROK_TLS_CERT=/etc/ongrok/fullchain.pem
export ONGROK_TLS_KEY=/etc/ongrok/privkey.pem
export ONGROK_ADMIN_TOKEN='...'
export ONGROK_USER_TOKEN='...'
export ONGROK_PUBLIC_HOST=relay.example.com
ongrok-relay-server run
```

Both binaries load an optional `.env` in the current directory. Explicit shell
environment variables override it. Run `ongrok-relay-server doctor` before
starting a deployment.

On a client machine, initialize the stable node identity and publish a local
TCP service:

```sh
ongrok-relay-client init
ongrok-relay-client service publish \
  --server 203.0.113.10:443 --server-name relay.example.com \
  --ca-cert /etc/ongrok/ca.pem --token "$ONGROK_USER_TOKEN" \
  --name ssh --local-address 127.0.0.1:22 --protocol tcp
```

The client tries QUIC first and automatically falls back to TCP/TLS/Yamux.
`ongrok-relay-client doctor` reports identity, CA, and transport reachability
without printing the token.

## Control API and console

The server control API defaults to `127.0.0.1:8080`. It exposes health,
services, nodes, metrics, events, and admin token rotation/revocation under
`/v1`. The React console keeps tokens in memory and requires re-entry after a
refresh. User tokens can read the complete service directory; admin-only token
controls are enforced by the API as well as the UI.

Heartbeat snapshots are sent once per minute and include RTT and available
CPU, memory, load, and network counters. Metric history is pruned to the most
recent three days. Certificates are supplied by the operator; ongrok does not
issue or renew ACME certificates.

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
pnpm --dir frontend test
pnpm --dir frontend build
pnpm --dir frontend run test:e2e
tests/e2e/quic-service.sh
tests/e2e/yamux-service.sh
tests/e2e/reconnect-service.sh
```

See `design.md`, `techspec.md`, and `PLAN.md` for product boundaries and
protocol details. P2P hole punching, TUN/VPN mode, local DNS, and multi-server
connections are intentionally outside the current MVP.
