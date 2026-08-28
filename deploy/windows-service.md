# Windows service

The binaries do not install a Windows service automatically. Use a service
wrapper approved by your organization, or create a service with an existing
wrapper such as WinSW. The wrapper command must be equivalent to:

```text
ongrok-relay-server.exe run
```

Set these values in the wrapper environment: `ONGROK_TLS_CERT`,
`ONGROK_TLS_KEY`, `ONGROK_DB_PATH`, `ONGROK_PUBLIC_HOST`,
`ONGROK_ADMIN_TOKEN`, and `ONGROK_USER_TOKEN`. Keep the PEM files and the
environment file readable only by the service account.

Before first start, run `ongrok-relay-server.exe doctor` with the same
certificate and database paths. For an upgrade, stop the service, replace the
binary, start it again, and check the service log. Roll back by restoring the
previous binary; the redb file format is part of the release and must be
backed up before upgrades. Certificates are externally managed and require a
server restart after replacement.
