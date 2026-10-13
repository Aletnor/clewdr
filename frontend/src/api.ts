// 管理面板用到的全部接口。认证统一走 Bearer <admin_password>，存在 localStorage.authToken。

export interface Usage {
  total_input_tokens?: number;
  total_output_tokens?: number;
  sonnet_input_tokens?: number;
  sonnet_output_tokens?: number;
  opus_input_tokens?: number;
  opus_output_tokens?: number;
}

export interface Account {
  cookie: string;
  reason?: unknown;
  reset_time?: number | null;
  session_usage?: Usage;
  weekly_usage?: Usage;
  weekly_sonnet_usage?: Usage;
  weekly_opus_usage?: Usage;
  lifetime_usage?: Usage;
  session_utilization?: number;
  seven_day_utilization?: number;
  seven_day_opus_utilization?: number;
  seven_day_sonnet_utilization?: number;
  session_resets_at?: string | null;
  seven_day_resets_at?: string | null;
  seven_day_opus_resets_at?: string | null;
  seven_day_sonnet_resets_at?: string | null;
  scoped_limits?: ScopedLimit[];
  // 授权登录的账户才有：登录时查到的邮箱、登录凭据的到期时刻（epoch 秒）
  oauth_email?: string | null;
  oauth_refresh_expires_at?: number | null;
}

export interface ScopedLimit {
  name: string;
  percent: number;
  resets_at?: string | null;
  severity?: string | null;
}

export interface Accounts {
  valid: Account[];
  exhausted: Account[];
  invalid: Account[];
}

export type Config = Record<string, unknown>;

export interface MonitorRow {
  ts: number;
  model: string | null;
  short: string;
  real: string | null;
  sig: "ok" | "bad" | "none";
  code: string | null;
  ok: boolean;
  flag: string | null;
  reason: string | null;
  refusal: { category: string | null; stage: "input" | "output" } | null;
  tags: string[];
  in: number;
  out: number;
  think: number | null;
  reply: number | null;
  ms: number | null;
  rate: number | null;
  cache_read: number | null;
  cache_write: number | null;
  stream: boolean;
  format: string | null;
  extra: string | null;
}

export interface MonitorData {
  now: string;
  total: number;
  hours: [number, number][];
  today: {
    count: number;
    bad: number;
    reasons: { key: string; category: string | null; n: number; input: number; output: number }[];
    out: number;
    reply: number | null;
    think: number | null;
    median_ms: number | null;
    cache_pct: number | null;
  };
  swapped: { count: number; model: string | null; real: string | null };
  rows: MonitorRow[];
  tally: {
    since: string | null;
    today: TallyRow[];
    month: TallyRow[];
    all: TallyRow[];
  };
}

export interface TallyRow {
  name: string;
  n: number;
  bad: number;
  out: number;
  swap: number;
}

export class AuthError extends Error {}
export class HttpError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

const TOKEN_KEY = "authToken";

export const token = {
  get: () => localStorage.getItem(TOKEN_KEY) || "",
  set: (t: string) => localStorage.setItem(TOKEN_KEY, t),
  clear: () => localStorage.removeItem(TOKEN_KEY),
};

async function call(path: string, init: RequestInit = {}, auth = token.get()) {
  const res = await fetch(path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${auth}`,
      ...(init.headers || {}),
    },
  });
  if (res.status === 401) throw new AuthError("凭据无效或已过期");
  if (!res.ok) {
    let detail = "";
    try {
      const body = await res.json();
      if (typeof body?.error === "string") detail = body.error;
      else if (typeof body?.message === "string") detail = body.message;
    } catch {
      /* 响应不是 JSON */
    }
    throw new HttpError(detail || `请求失败（HTTP ${res.status}）`, res.status);
  }
  return res;
}

export async function getVersion() {
  const res = await fetch("/api/version");
  const text = await res.text();
  return text.split(/\s/)[0] || "";
}

export async function checkPassword(pw: string) {
  try {
    await call("/api/auth", {}, pw);
    return true;
  } catch (e) {
    if (e instanceof AuthError) return false;
    throw e;
  }
}

export async function getAccounts(force = false): Promise<Accounts> {
  const res = await call(force ? "/api/cookies?refresh=true" : "/api/cookies");
  const d = await res.json();
  const list = (x: unknown) => (Array.isArray(x) ? (x as Account[]) : []);
  return { valid: list(d?.valid), exhausted: list(d?.exhausted), invalid: list(d?.invalid) };
}

export async function addAccount(cookie: string) {
  try {
    await call("/api/cookie", { method: "POST", body: JSON.stringify({ cookie }) });
  } catch (e) {
    if (e instanceof HttpError && e.status === 400) throw new Error("Cookie 格式无效");
    throw e;
  }
}

// 授权登录分两步：先要一条登录页网址，登完把页面上显示的授权码交回来
export async function oauthStart(): Promise<{ url: string; state: string }> {
  const res = await call("/api/oauth/start", { method: "POST" });
  return res.json();
}

export async function oauthFinish(state: string, code: string): Promise<{ email: string | null; replaced: boolean }> {
  const res = await call("/api/oauth/finish", { method: "POST", body: JSON.stringify({ state, code }) });
  return res.json();
}

export async function removeAccount(cookie: string) {
  await call("/api/cookie", { method: "DELETE", body: JSON.stringify({ cookie }) });
}

export async function getConfig(): Promise<Config> {
  const res = await call("/api/config");
  return res.json();
}

export async function saveConfig(cfg: Config) {
  await call("/api/config", { method: "POST", body: JSON.stringify(cfg) });
}

// 监控数据由 clewdr 自己从响应记录里数出来
export async function getMonitor(): Promise<MonitorData> {
  const res = await call("/api/monitor", { cache: "no-store" });
  return res.json();
}
