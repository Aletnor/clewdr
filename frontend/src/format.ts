export function compact(n: number | undefined) {
  const v = n ?? 0;
  if (v >= 1e9) return (v / 1e9).toFixed(2) + "B";
  if (v >= 1e6) return (v / 1e6).toFixed(v >= 1e8 ? 0 : 1) + "M";
  if (v >= 1e3) return (v / 1e3).toFixed(v >= 1e5 ? 0 : 1) + "k";
  return String(v);
}

export function maskCookie(c: string) {
  if (c.length <= 20) return c;
  return `${c.slice(0, 13)}…${c.slice(-6)}`;
}

// 授权登录的账户没有 Cookie，账户表里用一个 oauth-login- 开头的占位串当主键
export const isOAuth = (c: string) => c.startsWith("oauth-login-");

// 账户在面板上叫什么：授权登录的显示邮箱，贴 Cookie 的显示打码的 Cookie
export function accountName(a: { cookie: string; oauth_email?: string | null }) {
  if (isOAuth(a.cookie)) return a.oauth_email || "授权登录账户";
  return maskCookie(a.cookie);
}

export function toDate(t: string | number | null | undefined) {
  if (t == null || t === "") return null;
  const d = typeof t === "number" ? new Date(t * 1000) : new Date(t);
  return isNaN(d.getTime()) ? null : d;
}

// 「2 小时 13 分钟」
export function span(d: Date | null, now = Date.now()) {
  if (!d) return "";
  const mins = Math.round((d.getTime() - now) / 60000);
  if (mins <= 0) return "";
  if (mins < 60) return `${mins} 分钟`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  if (h < 24) return m ? `${h} 小时 ${m} 分钟` : `${h} 小时`;
  const days = Math.floor(h / 24);
  const hh = h % 24;
  return hh ? `${days} 天 ${hh} 小时` : `${days} 天`;
}

// 「2 小时 13 分钟后」
export function until(d: Date | null, now = Date.now()) {
  if (!d) return "";
  const s = span(d, now);
  return s ? `${s}后` : "即将";
}

const WEEK = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

// 「今天 13:20」「明天 09:00」「周四 17:00」
export function when(d: Date | null, now = new Date()) {
  if (!d) return "";
  // 上游的重置时刻常是 12:59:59，直接截掉秒会和整点那次数据来回跳
  d = new Date(Math.round(d.getTime() / 60000) * 60000);
  const hm = d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
  const day0 = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const diff = Math.floor((d.getTime() - day0) / 86400000);
  if (diff === 0) return `今天 ${hm}`;
  if (diff === 1) return `明天 ${hm}`;
  if (diff > 1 && diff < 7) return `${WEEK[d.getDay()]} ${hm}`;
  return `${d.getMonth() + 1}月${d.getDate()}日 ${hm}`;
}

// 失效原因。上游可能给字符串 "Null"，也可能给 { Restricted: 1790000000 }
export function reasonText(reason: unknown) {
  const names: Record<string, string> = {
    Free: "免费账户",
    Disabled: "组织已停用",
    Banned: "账户已封禁",
    Null: "会话已失效",
    NonPro: "非 Pro 账户",
    Restricted: "受限",
    TooManyRequest: "限流",
  };
  if (!reason) return "原因未知";
  if (typeof reason === "string") return names[reason] || reason;
  if (typeof reason === "object") {
    const [k, v] = Object.entries(reason as Record<string, unknown>)[0] || [];
    if (!k) return "原因未知";
    const base = names[k] || k;
    const d = typeof v === "number" ? toDate(v) : null;
    return d ? `${base}至 ${when(d)}` : base;
  }
  return String(reason);
}
