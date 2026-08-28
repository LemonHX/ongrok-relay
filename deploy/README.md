# ongrok deployment templates

These files are examples only. Install the server and client binaries from a
release bundle, provide a PEM full chain and matching private key, then run
`ongrok-relay-server doctor` before enabling the service manager unit.

Both binaries also load an optional `.env` from the current directory. Shell
environment variables override values from that file; never commit a real
`.env` because it contains long-lived tokens.

## Linux systemd

1. Create an `ongrok` user, `/etc/ongrok`, and `/var/lib/ongrok`.
2. Copy `ongrok-server.env.example` to `/etc/ongrok/server.env`, replace every
   placeholder, and set mode `0600`.
3. Copy `ongrok-relay-server.service` to `/etc/systemd/system/`.
4. Run `systemctl daemon-reload` and `systemctl enable --now ongrok-relay-server`.
5. Upgrade by replacing the binary, running `systemctl restart ongrok-relay-server`,
   and checking `journalctl -u ongrok-relay-server`.

## macOS launchd

Edit the paths and environment values in the plist, install it under
`~/Library/LaunchAgents/`, then run `launchctl bootstrap gui/$UID <plist>`.
Unload the old plist before a rollback. The redb file and certificates are
external state and must be backed up before upgrades.

The server does not issue or renew certificates. Use an external ACME client
or certificate provider, then restart the server after replacing the PEM files.

## Windows

See `windows-service.md`. Use a dedicated service account and an established
service wrapper; the ongrok binary intentionally does not modify the Windows
service registry itself.
