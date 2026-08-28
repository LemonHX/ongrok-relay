import { test, expect } from "@playwright/test";

test("logs in and renders services and node metrics", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("ongrok.locale", "en"));
  await page.route("**/v1/auth/validate", async (route) =>
    route.fulfill({ json: { kind: "User" } }),
  );
  await page.route("**/v1/services", async (route) =>
    route.fulfill({
      json: [{
        service_id: "svc-1", service_name: "ssh", node_id: "node-1", protocol: "Tcp",
        public_host: "relay.example", public_port: 22022, status: "Online", transport: "Quic", rtt_ms: 12,
      }],
    }),
  );
  await page.route("**/v1/nodes", async (route) =>
    route.fulfill({ json: [{
      node_id: "node-1", public_ip: "203.0.113.7", source_port: 51000, transport: "Quic",
      status: "Online", rtt_ms: 12, last_heartbeat_at_unix_ms: 1,
      metadata: { hostname: "workstation", os: "linux", arch: "x86_64", client_version: "0.1.0" },
    }] }),
  );
  await page.route("**/v1/events", async (route) =>
    route.fulfill({ json: [{
      event_id: "event-1", occurred_at_unix_ms: 1, kind: "NodeOnline",
      node_id: "node-1", service_id: null, token_kind: null,
    }] }),
  );
  await page.route("**/v1/nodes/node-1/metrics", async (route) =>
    route.fulfill({ json: [{
      recorded_at_unix_ms: 1, rtt_ms: 12,
      snapshot: { cpu_percent: 20, memory_percent: 35, load_average: 0.5 },
    }] }),
  );

  await page.goto("/");
  await page.getByLabel("API address").fill("http://127.0.0.1:8080");
  await page.getByLabel("Token").fill("user-token");
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page.getByRole("heading", { name: "Relay status" })).toBeVisible();
  await expect(page.getByText("ssh")).toBeVisible();
  await page.getByRole("button", { name: "Nodes" }).click();
  await expect(page.getByText("workstation")).toBeVisible();
  await expect(page.getByText("203.0.113.7:51000")).toBeVisible();
  await page.getByRole("button", { name: "Events" }).click();
  await expect(page.getByText("Node online")).toBeVisible();
  await expect(page.getByText("node-1")).toBeVisible();
});
