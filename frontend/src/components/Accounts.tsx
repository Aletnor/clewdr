import { useCallback, useEffect, useState } from "react";
import toast from "react-hot-toast";
import {
  addAccount,
  AuthError,
  getAccounts,
  oauthFinish,
  oauthStart,
  removeAccount,
  type Account,
  type Accounts as AccountsData,
  type Usage,
} from "../api";
import { accountName, compact, isOAuth, maskCookie, reasonText, span, toDate, until, when } from "../format";
import { CopyButton, Icon, Spinner } from "./ui";

const level = (p: number) => (p >= 90 ? "lvl-hot" : p >= 70 ? "lvl-warn" : "lvl-ok");

function Ring({ pct: raw }: { pct?: number }) {
  // 上游偶尔给 37.5 这种小数，圈里只放整数；这一回没查到用量就不写数，放一道横线
  const known = typeof raw === "number";
  const pct = typeof raw === "number" ? Math.round(raw) : 0;
  const r = 40;
  const c = 2 * Math.PI * r;
  const [shown, setShown] = useState(0);
  useEffect(() => {
    const id = requestAnimationFrame(() => setShown(pct));
    return () => cancelAnimationFrame(id);
  }, [pct]);
  return (
    <div className={`ring ${level(pct)}`}>
      <svg viewBox="0 0 100 100">
        <circle className="track" cx="50" cy="50" r={r} fill="none" strokeWidth="9" />
        <circle
          className="bar"
          cx="50"
          cy="50"
          r={r}
          fill="none"
          stroke="currentColor"
          strokeWidth="9"
          strokeLinecap="round"
          strokeDasharray={c}
          strokeDashoffset={c * (1 - Math.min(shown, 100) / 100)}
        />
      </svg>
      <div className="val">
        {known ? (
          <span>
            {pct}
            <small>%</small>
          </span>
        ) : (
          <span style={{ color: "var(--muted)" }}>—</span>
        )}
      </div>
    </div>
  );
}

interface MeterData {
  label: string;
  tag?: string;
  // 没有这一项就是这一回没从上游查到用量，和查到了是 0 不一样
  pct?: number;
  resets?: string | null;
}

function Meter({ label, tag, pct, resets }: MeterData) {
  const d = toDate(resets);
  const idle = pct === undefined ? "用量没查到" : pct === 0 ? "尚未计量" : "重置时间未知";
  return (
    <div className="card meter">
      <Ring pct={pct} />
      <div style={{ minWidth: 0 }}>
        <div className="label">
          {label}
          {tag && <span className="tag">{tag}</span>}
        </div>
        <div className="big">{d ? span(d) || "即将重置" : idle}</div>
        {d && <div className="foot">{when(d)} 重置</div>}
      </div>
    </div>
  );
}

function Stat({ k, u }: { k: string; u?: Usage }) {
  const i = u?.total_input_tokens ?? 0;
  const o = u?.total_output_tokens ?? 0;
  return (
    <div className="stat">
      <div className="k">{k}</div>
      <div>
        <div className="v num">
          {compact(i)}
          <span className="sep">/</span>
          {compact(o)}
        </div>
        <div className="d">输入 / 输出</div>
      </div>
    </div>
  );
}

function meters(acc: Account): MeterData[] {
  const list: MeterData[] = [
    { label: "5 小时窗口", pct: acc.session_utilization, resets: acc.session_resets_at },
    { label: "7 天窗口", pct: acc.seven_day_utilization, resets: acc.seven_day_resets_at },
  ];
  for (const s of acc.scoped_limits ?? []) {
    list.push({ label: "7 天窗口", tag: s.name, pct: s.percent, resets: s.resets_at });
  }
  const legacy: [string, number | undefined, string | null | undefined][] = [
    ["Opus", acc.seven_day_opus_utilization, acc.seven_day_opus_resets_at],
    ["Sonnet", acc.seven_day_sonnet_utilization, acc.seven_day_sonnet_resets_at],
  ];
  for (const [name, pct, resets] of legacy) {
    if (typeof pct === "number" && pct > 0 && !list.some((m) => m.tag === name)) {
      list.push({ label: "7 天窗口", tag: name, pct, resets });
    }
  }
  return list;
}

function Overview({ acc, label }: { acc: Account; label?: string }) {
  const ms = meters(acc);
  return (
    <section className="fade-in">
      {label && <div className="section-label">{label}</div>}
      <div className={`quota n${ms.length}`}>
        {ms.map((m) => (
          <Meter key={m.label + (m.tag ?? "")} {...m} />
        ))}
      </div>
      <div className="card stats">
        <Stat k="当前窗口" u={acc.session_usage} />
        <Stat k="本周" u={acc.weekly_usage} />
        <Stat k="累计" u={acc.lifetime_usage} />
      </div>
    </section>
  );
}

type Kind = "valid" | "exhausted" | "invalid";

function cooldown(a: Account) {
  const d =
    toDate(a.reset_time ?? null) ||
    toDate(a.seven_day_opus_resets_at) ||
    toDate(a.seven_day_sonnet_resets_at) ||
    toDate(a.seven_day_resets_at);
  return d ? `冷却中 · ${until(d)}恢复（${when(d)}）` : "冷却中 · 恢复时间未知";
}

function AccountCard({
  acc,
  kind,
  onChanged,
  onAuthFail,
}: {
  acc: Account;
  kind: Kind;
  onChanged: () => void;
  onAuthFail: () => void;
}) {
  const [arming, setArming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [reveal, setReveal] = useState(false);

  useEffect(() => {
    if (!arming) return;
    const t = setTimeout(() => setArming(false), 3000);
    return () => clearTimeout(t);
  }, [arming]);

  const fail = (e: unknown) => {
    if (e instanceof AuthError) return onAuthFail();
    toast.error(e instanceof Error ? e.message : String(e));
  };

  const del = async () => {
    setBusy(true);
    try {
      await removeAccount(acc.cookie);
      toast.success("账户已删除");
      onChanged();
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
      setArming(false);
    }
  };

  // 授权登录的账户没有 Cookie 可看可抄，名字显示邮箱，失效了就是登录过期
  const oauth = isOAuth(acc.cookie);
  const loginUntil = oauth ? toDate(acc.oauth_refresh_expires_at ?? null) : null;
  // 登录已经到期、账户还排在「可用」里：手里的令牌用完就续不上了，不能再写运行正常
  const loginOver = kind === "valid" && !!loginUntil && loginUntil.getTime() <= Date.now();
  const tone = loginOver ? "tone-wait" : kind === "valid" ? "tone-ok" : kind === "exhausted" ? "tone-wait" : "tone-bad";
  const okText = oauth
    ? loginOver
      ? "授权登录 · 登录已到期，重新登录一次即可"
      : loginUntil
        ? `授权登录 · 运行正常 · 登录有效至 ${when(loginUntil)}`
        : "授权登录 · 运行正常"
    : "运行正常";
  const badText = oauth ? "授权已过期，重新登录一次即可" : reasonText(acc.reason);
  const meta = kind === "valid" ? okText : kind === "exhausted" ? cooldown(acc) : badText;

  return (
    <div className="card acc">
      <div className="acc-row">
        <span className={`dot ${tone} ${kind === "valid" && !loginOver ? "live" : ""}`} />
        <div className="acc-main">
          {oauth ? (
            <div className="acc-key">{accountName(acc)}</div>
          ) : (
            <button type="button" className="acc-key" onClick={() => setReveal(!reveal)}>
              {maskCookie(acc.cookie)}
            </button>
          )}
          <div className={`acc-meta ${kind === "invalid" ? "tone-bad" : loginOver ? "tone-wait" : ""}`}>{meta}</div>
        </div>
        <div className="acc-actions">
          {!oauth && (
            <>
              <button
                type="button"
                className={`icon-btn ${reveal ? "on" : ""}`}
                title={reveal ? "收起完整 Cookie" : "展开完整 Cookie"}
                aria-expanded={reveal}
                onClick={() => setReveal(!reveal)}
              >
                <Icon name={reveal ? "eyeOff" : "eye"} size={16} />
              </button>
              <CopyButton text={acc.cookie} title="复制完整 Cookie" />
            </>
          )}
          {arming ? (
            <button className="confirm-del" onClick={del} disabled={busy}>
              {busy ? <Spinner size={14} /> : "确认删除"}
            </button>
          ) : (
            <button className="icon-btn danger" title="删除" onClick={() => setArming(true)}>
              <Icon name="trash" size={16} />
            </button>
          )}
        </div>
      </div>
      {reveal && !oauth && <div className="acc-full fade-in">{acc.cookie}</div>}
    </div>
  );
}

// 授权登录：生成登录页网址 → 在那边登录并点授权 → 把页面上显示的授权码贴回来
function OAuthAdder({ onDone, onAuthFail }: { onDone: () => void; onAuthFail: () => void }) {
  const [link, setLink] = useState<{ url: string; state: string } | null>(null);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState<"" | "start" | "finish">("");

  const fail = (e: unknown) => {
    if (e instanceof AuthError) return onAuthFail();
    toast.error(e instanceof Error ? e.message : String(e));
  };

  const begin = async () => {
    setBusy("start");
    try {
      setLink(await oauthStart());
      setCode("");
    } catch (e) {
      fail(e);
    } finally {
      setBusy("");
    }
  };

  const finish = async () => {
    if (!link || !code.trim()) return;
    setBusy("finish");
    try {
      const r = await oauthFinish(link.state, code.trim());
      toast.success(`${r.email || "账户"} ${r.replaced ? "已重新登录" : "已添加"}`);
      setLink(null);
      setCode("");
      onDone();
    } catch (e) {
      fail(e);
    } finally {
      setBusy("");
    }
  };

  return (
    <>
      <div className="oauth-row">
        {link ? (
          <>
            <a className="btn btn-primary oauth-open" href={link.url} target="_blank" rel="noreferrer">
              <Icon name="send" size={16} />
              打开登录页
            </a>
            <CopyButton text={link.url} title="复制登录链接" />
            <button
              type="button"
              className="icon-btn"
              title="重新生成链接"
              aria-label="重新生成链接"
              onClick={begin}
              disabled={busy !== ""}
            >
              {busy === "start" ? <Spinner size={14} /> : <Icon name="refresh" size={16} />}
            </button>
          </>
        ) : (
          <button className="btn btn-primary" onClick={begin} disabled={busy !== ""}>
            {busy === "start" ? <Spinner /> : <Icon name="key" size={16} />}
            生成登录链接
          </button>
        )}
      </div>
      {link && (
        <>
          <input
            className="input mono"
            placeholder="粘贴登录页上显示的授权码"
            value={code}
            onChange={(e) => setCode(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && finish()}
            disabled={busy !== ""}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
          />
          <div className="adder-foot">
            <span>链接生成后一刻钟内有效</span>
            <button className="btn btn-primary" onClick={finish} disabled={busy !== "" || !code.trim()}>
              {busy === "finish" ? <Spinner /> : <Icon name="check" size={16} />}
              完成登录
            </button>
          </div>
        </>
      )}
    </>
  );
}

function Adder({ onDone, onAuthFail }: { onDone: () => void; onAuthFail: () => void }) {
  const [mode, setMode] = useState<"cookie" | "oauth">("cookie");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<{ c: string; ok: boolean; msg: string }[]>([]);

  const lines = text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

  const submit = async () => {
    if (!lines.length) return;
    setBusy(true);
    const out: typeof results = [];
    for (const c of lines) {
      try {
        await addAccount(c);
        out.push({ c, ok: true, msg: "已添加" });
      } catch (e) {
        if (e instanceof AuthError) {
          setBusy(false);
          return onAuthFail();
        }
        out.push({ c, ok: false, msg: e instanceof Error ? e.message : String(e) });
      }
    }
    setResults(out);
    setBusy(false);
    const okN = out.filter((r) => r.ok).length;
    if (okN) onDone();
    if (okN === out.length) {
      toast.success(`已添加 ${okN} 个账户`);
      setText("");
    } else {
      toast.error(`${out.length - okN} 个添加失败`);
      setText(
        out
          .filter((r) => !r.ok)
          .map((r) => r.c)
          .join("\n"),
      );
    }
  };

  const modes = (
    <div className="seg adder-seg">
      <button type="button" aria-pressed={mode === "cookie"} onClick={() => setMode("cookie")}>
        输入曲奇
      </button>
      <button type="button" aria-pressed={mode === "oauth"} onClick={() => setMode("oauth")}>
        授权登录
      </button>
    </div>
  );

  if (mode === "oauth") {
    return (
      <div className="card adder">
        {modes}
        <OAuthAdder onDone={onDone} onAuthFail={onAuthFail} />
      </div>
    );
  }

  return (
    <div className="card adder">
      {modes}
      <textarea
        className="textarea mono"
        rows={4}
        placeholder="粘贴 Cookie（sk-ant-sid…），每行一个，支持批量导入"
        value={text}
        onChange={(e) => setText(e.target.value)}
        disabled={busy}
      />
      {results.some((r) => !r.ok) && (
        <div className="results">
          {results.map((r, i) => (
            <div key={i} className={`result ${r.ok ? "ok" : "bad"}`}>
              <Icon name={r.ok ? "check" : "minus"} size={14} />
              <span className="mono">{maskCookie(r.c)}</span>
              <span style={{ marginLeft: "auto" }}>{r.msg}</span>
            </div>
          ))}
        </div>
      )}
      <div className="adder-foot">
        <span>{lines.length ? `已识别 ${lines.length} 条` : "等待输入"}</span>
        <button className="btn btn-primary" onClick={submit} disabled={busy || !lines.length}>
          {busy ? <Spinner /> : <Icon name="send" size={16} />}
          导入
        </button>
      </div>
    </div>
  );
}

export default function Accounts({ onAuthFail }: { onAuthFail: () => void }) {
  const [data, setData] = useState<AccountsData | null>(null);
  const [err, setErr] = useState("");
  const [loading, setLoading] = useState(false);
  // 添加账户的卡片平时收着；上回是开是收记在浏览器里，下次进来照旧
  const [adding, setAdding] = useState(() => {
    try {
      return localStorage.getItem("adderOpen") === "1";
    } catch {
      return false;
    }
  });
  const toggleAdding = () => {
    const v = !adding;
    setAdding(v);
    try {
      localStorage.setItem("adderOpen", v ? "1" : "0");
    } catch {
      /* 存不了就不记 */
    }
  };
  const [, tick] = useState(0);

  const load = useCallback(
    async (force = false) => {
      setLoading(true);
      try {
        setData(await getAccounts(force));
        setErr("");
      } catch (e) {
        if (e instanceof AuthError) return onAuthFail();
        setErr(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [onAuthFail],
  );

  useEffect(() => {
    load();
    const id = setInterval(() => {
      if (document.visibilityState === "visible") load();
      tick((n) => n + 1);
    }, 60_000);
    return () => clearInterval(id);
  }, [load]);

  const sortByReset = (xs: Account[]) => [...xs].sort((a, b) => (a.reset_time ?? 0) - (b.reset_time ?? 0));

  const total = data ? data.valid.length + data.exhausted.length + data.invalid.length : 0;
  const counts: [string, string, number][] = [
    ["tone-ok", "可用", data?.valid.length ?? 0],
    ["tone-wait", "冷却", data?.exhausted.length ?? 0],
    ["tone-bad", "失效", data?.invalid.length ?? 0],
  ];

  return (
    <div className="page">
      <div className="page-head acc-head">
        <h1 className="page-title">账户</h1>
        {/* 标题后面一颗小胶囊：共 / 可用 / 冷却 / 失效 四个数量，最右头是刷新 */}
        <div className="acc-chip">
          <span className="chip-cell">
            共<b className="num">{data ? total : "—"}</b>
          </span>
          {counts.map(([tone, name, n]) => (
            <span className="chip-cell" key={name}>
              <i className={`dot sm ${tone}`} />
              {name}
              <b className="num">{data ? n : "—"}</b>
            </span>
          ))}
          <span className="chip-btns">
            {/* 手机上没有 Ctrl，手点的刷新一律向上游重查额度；每分钟的自动刷新才走缓存 */}
            <button
              type="button"
              className="icon-btn chip-refresh"
              title="刷新"
              aria-label="刷新"
              onClick={() => load(true)}
              disabled={loading}
            >
              <Icon name="refresh" size={14} className={loading ? "spin" : undefined} />
            </button>
            {/* 加号：点了下面出来添加账户的卡片，自己转成叉；再点收走 */}
            <button
              type="button"
              className={`icon-btn chip-plus ${adding ? "x" : ""}`}
              title={adding ? "收起" : "添加账户"}
              aria-label={adding ? "收起" : "添加账户"}
              aria-expanded={adding}
              onClick={toggleAdding}
            >
              <Icon name="plus" size={14} />
            </button>
          </span>
        </div>
      </div>

      {err && (
        <div className="banner">
          <span>无法获取账户数据：{err}</span>
          <button className="btn btn-ghost" onClick={() => load()}>
            重试
          </button>
        </div>
      )}

      {adding && <Adder onDone={() => load()} onAuthFail={onAuthFail} />}

      {!data && !err && (
        <div className="quota n3" style={{ marginTop: 4 }}>
          <div className="skeleton" style={{ height: 132 }} />
          <div className="skeleton" style={{ height: 132 }} />
          <div className="skeleton" style={{ height: 132 }} />
        </div>
      )}

      {data?.valid.map((acc) => (
        <Overview
          key={acc.cookie}
          acc={acc}
          label={data.valid.length > 1 ? `额度 · ${accountName(acc)}` : undefined}
        />
      ))}

      {data && total === 0 && !adding && (
        <div className="card empty" style={{ marginTop: 14 }}>
          暂无账户。点标题旁边的加号，粘贴 Cookie 或者用授权登录。
        </div>
      )}

      {data &&
        (
          [
            ["valid", "可用", data.valid],
            ["exhausted", "冷却中", sortByReset(data.exhausted)],
            ["invalid", "失效", data.invalid],
          ] as [Kind, string, Account[]][]
        )
          .filter(([, , xs]) => xs.length > 0)
          .map(([kind, title, xs]) => (
            <section key={kind}>
              <div className="section-label">
                {title}
                <span className="count">{xs.length}</span>
              </div>
              <div className="acc-list">
                {xs.map((acc) => (
                  <AccountCard
                    key={acc.cookie}
                    acc={acc}
                    kind={kind}
                    onChanged={() => load()}
                    onAuthFail={onAuthFail}
                  />
                ))}
              </div>
            </section>
          ))}
    </div>
  );
}
