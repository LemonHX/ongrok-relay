import i18next from "i18next";
import { initReactI18next } from "react-i18next";

const resources = {
  en: {
    translation: {
      brand: "ongrok",
      relayConsole: "RELAY CONSOLE",
      loginTitle: "Connect your relay",
      loginSubtitle: "Use a trusted long-lived token to inspect nodes and services.",
      apiAddress: "API address",
      token: "Long-lived token",
      connect: "Connect",
      overview: "Overview",
      nodes: "Nodes",
      events: "Events",
      recentEvents: "Recent events",
      event: "Event",
      time: "Time",
      service: "Service",
      noEvents: "No events yet",
      eventKinds: {
        NodeOnline: "Node online",
        NodeOffline: "Node offline",
        ServiceRegistered: "Service registered",
        ServiceDeleted: "Service deleted",
        TokenRotated: "Token rotated",
        TokenRevoked: "Token revoked",
      },
      relayStatus: "Relay status",
      refresh: "Refresh",
      logout: "Sign out",
      onlineServices: "Online services",
      totalServices: "Total services",
      lastSynced: "Last synced",
      services: "Services",
      waiting: "Waiting to sync",
      synced: "Synced",
      syncing: "Syncing",
      serviceName: "Name",
      protocol: "Protocol",
      node: "Node",
      endpoint: "Public endpoint",
      status: "Status",
      online: "Online",
      noServices: "No services yet",
      theme: "Theme",
      language: "Language",
      dark: "Dark",
      light: "Light",
      system: "System",
      zh: "中文",
      english: "English",
    },
  },
  "zh-CN": {
    translation: {
      brand: "ongrok",
      relayConsole: "RELAY CONSOLE",
      loginTitle: "进入你的 relay",
      loginSubtitle: "使用受信任的长期 token 查看节点与服务状态。",
      apiAddress: "API 地址",
      token: "长期 token",
      connect: "连接",
      overview: "概览",
      nodes: "节点",
      events: "事件",
      recentEvents: "最近事件",
      event: "事件",
      time: "时间",
      service: "服务",
      noEvents: "暂无事件",
      eventKinds: {
        NodeOnline: "节点上线",
        NodeOffline: "节点离线",
        ServiceRegistered: "服务注册",
        ServiceDeleted: "服务删除",
        TokenRotated: "Token 已轮换",
        TokenRevoked: "Token 已撤销",
      },
      relayStatus: "Relay 状态",
      refresh: "刷新",
      logout: "退出",
      onlineServices: "在线服务",
      totalServices: "服务总数",
      lastSynced: "最后同步",
      services: "服务",
      waiting: "等待同步",
      synced: "已同步",
      syncing: "同步中",
      serviceName: "名称",
      protocol: "协议",
      node: "节点",
      endpoint: "公开地址",
      status: "状态",
      online: "在线",
      noServices: "暂无服务",
      theme: "主题",
      language: "语言",
      dark: "深色",
      light: "浅色",
      system: "跟随系统",
      zh: "中文",
      english: "English",
    },
  },
} as const;

const localeKey = "ongrok.locale";
const stored = localStorage.getItem(localeKey);
const initialLocale =
  stored === "en" || stored === "zh-CN"
    ? stored
    : navigator.language.toLowerCase().startsWith("zh")
      ? "zh-CN"
      : "en";
void i18next.use(initReactI18next).init({
  resources,
  lng: initialLocale,
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});
i18next.on("languageChanged", (locale) => {
  document.documentElement.lang = locale;
  localStorage.setItem(localeKey, locale);
});
document.documentElement.lang = initialLocale;
export { i18next };
