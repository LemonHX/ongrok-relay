import {
  Activity,
  Languages,
  LayoutDashboard,
  LogOut,
  History,
  KeyRound,
  Server,
  Moon,
  RefreshCw,
  Sun,
  Monitor,
} from "lucide-react";
import { FormEvent, useState } from "react";
import { useTranslation } from "react-i18next";
import { useAppTheme, type Theme } from "./theme";
import { endpointLabel, metricPoints, type MetricSample } from "./view-model";

type Service = {
  service_id: string;
  service_name: string;
  node_id: string;
  protocol: string;
  public_host: string | null;
  public_port: number | null;
  status: "Online" | "Offline";
  transport: string | null;
  rtt_ms: number | null;
};
type Node = {
  node_id: string;
  public_ip: string;
  source_port: number;
  transport: string;
  status: "Online" | "Offline";
  rtt_ms: number | null;
  last_heartbeat_at_unix_ms: number | null;
  metadata: { hostname: string; os: string; arch: string; client_version: string };
};
type Metric = MetricSample;
type Auth = { kind: string };
type TokenKind = "Admin" | "User";
type ControlEvent = {
  event_id: string;
  occurred_at_unix_ms: number;
  kind: "NodeOnline" | "NodeOffline" | "ServiceRegistered" | "ServiceDeleted" | "TokenRotated" | "TokenRevoked";
  node_id: string | null;
  service_id: string | null;
  token_kind: "Admin" | "User" | null;
};

export function App() {
  const { t, i18n } = useTranslation();
  const { theme, setTheme } = useAppTheme();
  const [server, setServer] = useState("https://api.relay.lemonhx.moe");
  const [token, setToken] = useState("");
  const [auth, setAuth] = useState<Auth | null>(null);
  const [services, setServices] = useState<Service[]>([]);
  const [nodes, setNodes] = useState<Node[]>([]);
  const [metrics, setMetrics] = useState<Metric[]>([]);
  const [events, setEvents] = useState<ControlEvent[]>([]);
  const [view, setView] = useState<"overview" | "nodes" | "events" | "admin">("overview");
  const [error, setError] = useState("");
  const [syncStatus, setSyncStatus] = useState(t("waiting"));
  const [syncedAt, setSyncedAt] = useState("--:--");
  const api = server.replace(/\/$/, "");
  const request = async <T,>(path: string, init?: RequestInit): Promise<T> => {
    const response = await fetch(api + path, {
      ...init,
      headers: { Authorization: "Bearer " + token, ...init?.headers },
    });
    if (!response.ok)
      throw new Error(
        ((await response.json().catch(() => ({}))) as { error?: string }).error ??
          "HTTP " + response.status,
      );
    return response.json() as Promise<T>;
  };
  const sync = async () => {
    setSyncStatus(t("syncing"));
    try {
      const [serviceResult, nodeResult, eventResult] = await Promise.all([
        request<Service[]>("/v1/services"),
        request<Node[]>("/v1/nodes"),
        request<ControlEvent[]>("/v1/events"),
      ]);
      setServices(serviceResult);
      setNodes(nodeResult);
      setEvents(eventResult);
      if (nodeResult[0]) {
        setMetrics(await request<Metric[]>(`/v1/nodes/${nodeResult[0].node_id}/metrics`));
      } else {
        setMetrics([]);
      }
      setSyncedAt(new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }));
      setSyncStatus(t("synced"));
    } catch (cause) {
      setSyncStatus(cause instanceof Error ? cause.message : "Request failed");
    }
  };
  const login = async (event: FormEvent) => {
    event.preventDefault();
    setError("");
    try {
      const result = await request<Auth>("/v1/auth/validate", { method: "POST" });
      setAuth(result);
      await sync();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Request failed");
    }
  };
  if (!auth)
    return (
      <main className="screen grid-bg login">
        <div className="brand">
          <span className="brand-mark">o</span>
          <span>{t("brand")}</span>
        </div>
        <section className="card">
          <p className="eyebrow">{t("relayConsole")}</p>
          <h1>{t("loginTitle")}</h1>
          <p className="muted">{t("loginSubtitle")}</p>
          <form className="form" onSubmit={login}>
            <label className="field">
              {t("apiAddress")}
              <input
                type="url"
                value={server}
                onChange={(event) => setServer(event.target.value)}
                required
              />
            </label>
            <label className="field">
              {t("token")}
              <input
                type="password"
                value={token}
                onChange={(event) => setToken(event.target.value)}
                autoComplete="off"
                required
              />
            </label>
            <button className="button" type="submit">
              {t("connect")}
            </button>
            <p className="error">{error}</p>
          </form>
        </section>
      </main>
    );
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">o</span>
          <span>{t("brand")}</span>
        </div>
        <nav>
          <button className={`nav-item ${view === "overview" ? "active" : ""}`} onClick={() => setView("overview")}>
            <LayoutDashboard size={17} />
            {t("overview")}
          </button>
          <button className={`nav-item ${view === "nodes" ? "active" : ""}`} onClick={() => setView("nodes")}>
            <Server size={17} />
            {t("nodes")}
          </button>
          <button className={`nav-item ${view === "events" ? "active" : ""}`} onClick={() => setView("events")}>
            <History size={17} />
            {t("events")}
          </button>
          {auth.kind.toLowerCase() === "admin" && (
            <button className={`nav-item ${view === "admin" ? "active" : ""}`} onClick={() => setView("admin")}>
              <KeyRound size={17} />
              {t("adminSettings")}
            </button>
          )}
        </nav>
        <div className="sidebar-bottom">
          <label className="field">
            <span>
              <Languages size={14} /> {t("language")}
            </span>
            <select
              className="select"
              value={i18n.language}
              onChange={(event) => void i18n.changeLanguage(event.target.value)}
            >
              <option value="zh-CN">{t("zh")}</option>
              <option value="en">{t("english")}</option>
            </select>
          </label>
          <label className="field">
            <span>
              {theme === "dark" ? (
                <Moon size={14} />
              ) : theme === "light" ? (
                <Sun size={14} />
              ) : (
                <Monitor size={14} />
              )}{" "}
              {t("theme")}
            </span>
            <select
              className="select"
              value={theme}
              onChange={(event) => setTheme(event.target.value as Theme)}
            >
              <option value="system">{t("system")}</option>
              <option value="light">{t("light")}</option>
              <option value="dark">{t("dark")}</option>
            </select>
          </label>
        </div>
      </aside>
      <main className="main">
        <header className="topbar">
          <span className="pill">{auth.kind}</span>
          <button
            className="button secondary"
            onClick={() => {
              setAuth(null);
              setToken("");
            }}
          >
            <LogOut size={15} /> {t("logout")}
          </button>
        </header>
        <div className="content">
          <div className="heading">
            <div>
              <p className="eyebrow">{t(view)}</p>
              <h1>{view === "overview" ? t("relayStatus") : t(view)}</h1>
            </div>
            <button className="button secondary" onClick={() => void sync()}>
              <RefreshCw size={15} /> {t("refresh")}
            </button>
          </div>
          {view === "overview" ? (
          <>
          <div className="stats">
            <Stat label={t("onlineServices")} value={services.filter((service) => service.status === "Online").length} />
            <Stat label={t("totalServices")} value={services.length} />
            <Stat label={t("lastSynced")} value={syncedAt} />
          </div>
          <section className="section">
            <div className="section-head">
              <h2>{t("services")}</h2>
              <span className="muted">{syncStatus}</span>
            </div>
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>{t("serviceName")}</th>
                    <th>{t("protocol")}</th>
                    <th>{t("node")}</th>
                    <th>{t("endpoint")}</th>
                    <th>{t("status")}</th>
                  </tr>
                </thead>
                <tbody>
                  {services.length === 0 ? (
                    <tr>
                      <td colSpan={5} className="empty">
                        {t("noServices")}
                      </td>
                    </tr>
                  ) : (
                    services.map((service) => (
                      <tr key={service.service_id}>
                        <td>
                          <strong>{service.service_name}</strong>
                        </td>
                        <td>{service.protocol}</td>
                        <td>{service.node_id}</td>
                        <td>
                          {endpointLabel(service.public_host, service.public_port)}
                        </td>
                        <td className={service.status === "Online" ? "online" : "offline"}>
                          <Activity size={13} /> {t("online")}
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>
          </section>
          </>
          ) : view === "nodes" ? (
            <NodePanel nodes={nodes} metrics={metrics} syncStatus={syncStatus} />
          ) : view === "events" ? (
            <EventPanel events={events} />
          ) : (
            <AdminPanel request={request} />
          )}
        </div>
      </main>
    </div>
  );
}

function AdminPanel({ request }: { request: <T>(path: string, init?: RequestInit) => Promise<T> }) {
  const { t } = useTranslation();
  const [kind, setKind] = useState<TokenKind>("User");
  const [newToken, setNewToken] = useState("");
  const [message, setMessage] = useState("");
  const [confirmRevoke, setConfirmRevoke] = useState(false);
  const mutate = async (action: "rotate" | "revoke") => {
    setMessage("");
    try {
      const result = await request<{ token?: string; revoked?: boolean }>(`/v1/admin/tokens/${action}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ kind }),
      });
      setNewToken(result.token ?? "");
      setMessage(action === "rotate" ? t("tokenRotated") : t("tokenRevoked"));
      setConfirmRevoke(false);
    } catch (cause) {
      setMessage(cause instanceof Error ? cause.message : t("requestFailed"));
    }
  };
  return (
    <section className="section admin-settings">
      <div className="section-head"><h2>{t("tokenManagement")}</h2></div>
      <label className="field admin-kind">
        {t("tokenKind")}
        <select className="select" value={kind} onChange={(event) => { setKind(event.target.value as TokenKind); setConfirmRevoke(false); setNewToken(""); }}>
          <option value="User">{t("userToken")}</option>
          <option value="Admin">{t("adminToken")}</option>
        </select>
      </label>
      <div className="admin-actions">
        <button className="button" onClick={() => void mutate("rotate")}><RefreshCw size={15} /> {t("rotateToken")}</button>
        {!confirmRevoke ? (
          <button className="button danger" onClick={() => setConfirmRevoke(true)}>{t("revokeToken")}</button>
        ) : (
          <>
            <button className="button danger" onClick={() => void mutate("revoke")}>{t("confirmRevoke")}</button>
            <button className="button secondary" onClick={() => setConfirmRevoke(false)}>{t("cancel")}</button>
          </>
        )}
      </div>
      {newToken && <label className="field token-result">{t("newToken")}<input readOnly value={newToken} /></label>}
      <p className="muted" role="status">{message}</p>
    </section>
  );
}

function EventPanel({ events }: { events: ControlEvent[] }) {
  const { t } = useTranslation();
  return (
    <section className="section">
      <div className="section-head"><h2>{t("recentEvents")}</h2><span className="muted">{events.length}</span></div>
      <div className="table-wrap">
        <table>
          <thead><tr><th>{t("time")}</th><th>{t("event")}</th><th>{t("node")}</th><th>{t("service")}</th></tr></thead>
          <tbody>
            {events.length === 0 ? <tr><td colSpan={4} className="empty">{t("noEvents")}</td></tr> : events.map((event) => (
              <tr key={event.event_id}>
                <td>{new Date(event.occurred_at_unix_ms).toLocaleString()}</td>
                <td><strong>{t(`eventKinds.${event.kind}`)}</strong></td>
                <td>{event.node_id ?? "--"}</td>
                <td>{event.service_id ?? event.token_kind ?? "--"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function NodePanel({ nodes, metrics, syncStatus }: { nodes: Node[]; metrics: Metric[]; syncStatus: string }) {
  const node = nodes[0];
  if (!node) return <section className="section empty">{syncStatus}</section>;
  const latest = metrics.at(-1);
  return (
    <>
      <div className="stats">
        <Stat label="RTT" value={node.rtt_ms == null ? "--" : `${node.rtt_ms} ms`} />
        <Stat label="CPU" value={latest?.snapshot.cpu_percent == null ? "--" : `${latest.snapshot.cpu_percent.toFixed(1)}%`} />
        <Stat label="Memory" value={latest?.snapshot.memory_percent == null ? "--" : `${latest.snapshot.memory_percent.toFixed(1)}%`} />
      </div>
      <section className="section">
        <div className="section-head"><h2>{node.metadata.hostname}</h2><span className={node.status === "Online" ? "online" : "offline"}>{node.status}</span></div>
        <p className="muted node-meta">{node.public_ip}:{node.source_port} · {node.metadata.os}/{node.metadata.arch} · {node.transport}</p>
        <MetricChart metrics={metrics} />
      </section>
    </>
  );
}

function MetricChart({ metrics }: { metrics: Metric[] }) {
  const points = metricPoints(metrics, (metric) => metric.snapshot.cpu_percent);
  return (
    <div className="chart">
      <div className="chart-label"><span>CPU</span><span>{metrics.length} samples</span></div>
      <svg viewBox="0 0 100 100" preserveAspectRatio="none" role="img" aria-label="CPU history">
        <polyline points={points.join(" ")} fill="none" stroke="currentColor" strokeWidth="2" vectorEffect="non-scaling-stroke" />
      </svg>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: number | string }) {
  return (
    <article className="stat">
      <span>{label}</span>
      <strong>{value}</strong>
    </article>
  );
}
