import { useCallback, useEffect, useRef, useState, type PointerEvent } from "react";
import { getMonitor, type MonitorData, type MonitorRow } from "../api";
import { Icon } from "./ui";

function human(n: number) {
  if (n >= 1_000_000) return `${(n / 1e6).toFixed(2)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

function spent(ms: number | null | undefined) {
  if (ms == null) return "—";
  if (ms >= 60_000) return `${Math.floor(ms / 60000)}m${String(Math.floor((ms % 60000) / 1000)).padStart(2, "0")}s`;
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`;
}

const pad = (n: number) => String(n).padStart(2, "0");
const dayKey = (d: Date) => `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
const num = (n: number) => n.toLocaleString("en-US");

// 标签一律用上游和 clewdr 的原词，中文只做说明
// 异常原因卡片里放不下的长分类名，用短写，完整原词放在悬停提示里
const SHORT: Record<string, string> = {
  reasoning_extraction: "reasoning",
};
const REFUSAL_DESC: Record<string, string> = {
  reasoning_extraction: "思维链提取",
  cyber: "网络安全",
};
const STAGE = { input: "输入拦截", output: "生成中断" } as const;

const FLAG_DESC: Record<string, string> = {
  cut: "疑似截断",
  "no stop": "未收到结束事件",
  "not sent": "未送达客户端",
  max_tokens: "达到输出上限",
  model_context_window_exceeded: "超出上下文长度",
  overloaded: "上游过载",
};
function flagDesc(k: string) {
  const key = k.replace(/_error$/, "");
  if (FLAG_DESC[key]) return FLAG_DESC[key];
  if (/^4\d\d$/.test(key)) return "请求错误";
  if (/^5\d\d$/.test(key)) return "服务端错误";
  return "其他异常";
}

function niceTicks(max: number) {
  const step = [1, 2, 5, 10, 20, 50, 100].find((s) => max / s <= 4) ?? 200;
  const top = Math.max(step, Math.ceil(max / step) * step);
  const ticks: number[] = [];
  for (let v = 0; v <= top; v += step) ticks.push(v);
  return { top, ticks };
}

function HourChart({ hours, nowHour }: { hours: [number, number][]; nowHour: number }) {
  const [active, setActive] = useState<number | null>(null);
  const [leaving, setLeaving] = useState(false);
  const timers = useRef<number[]>([]);
  const clearTimers = () => {
    timers.current.forEach((t) => window.clearTimeout(t));
    timers.current = [];
  };
  // 手指松开 1 秒后提示框淡出消失；鼠标移开立即消失
  const release = () => {
    clearTimers();
    timers.current.push(window.setTimeout(() => setLeaving(true), 1000));
    timers.current.push(
      window.setTimeout(() => {
        setActive(null);
        setLeaving(false);
      }, 1200),
    );
  };
  useEffect(() => clearTimers, []);
  const { top, ticks } = niceTicks(Math.max(1, ...hours.map(([a, b]) => a + b)));
  const tip = active != null ? hours[active] : null;
  const pick = (e: PointerEvent<HTMLDivElement>) => {
    clearTimers();
    setLeaving(false);
    const r = e.currentTarget.getBoundingClientRect();
    setActive(Math.min(23, Math.max(0, Math.floor(((e.clientX - r.left) / r.width) * 24))));
  };
  return (
    <div className="hchart" onPointerLeave={(e) => e.pointerType === "mouse" && setActive(null)}>
      <div className="hchart-plot">
        {ticks.map((t) => (
          <div key={t} className="hgrid" style={{ bottom: `${(t / top) * 100}%` }}>
            <span>{t}</span>
          </div>
        ))}
        <div
          className="hcols"
          onPointerDown={(e) => {
            pick(e);
            try {
              e.currentTarget.setPointerCapture(e.pointerId);
            } catch {
              /* 个别浏览器不给捕获，拖动照样能用，只是手指滑出图表后不再跟随 */
            }
          }}
          onPointerMove={(e) => {
            if (e.pointerType === "mouse" || e.buttons) pick(e);
          }}
          onPointerUp={(e) => e.pointerType !== "mouse" && release()}
          onPointerCancel={() => release()}
        >
          {hours.map(([ok, bad], h) => {
            const total = ok + bad;
            return (
              <div
                key={h}
                className={`hcol ${h === nowHour ? "now" : ""} ${active === h ? "active" : ""}`}
                aria-label={`${pad(h)}:00 至 ${pad(h + 1)}:00，${total} 次请求，${bad} 次异常`}
              >
                {total > 0 && (
                  <div className="hbar" style={{ height: `${(total / top) * 100}%` }}>
                    {bad > 0 && <div className="seg-bad" style={{ flexGrow: bad }} />}
                    {ok > 0 && <div className="seg-ok" style={{ flexGrow: ok }} />}
                  </div>
                )}
              </div>
            );
          })}
        </div>
        {tip && active != null && (
          <div
            className={`htip ${active < 4 ? "l" : active > 19 ? "r" : ""} ${leaving ? "leaving" : ""}`}
            style={{ left: `${((active + 0.5) / 24) * 100}%` }}
          >
            <div className="htip-t">
              {pad(active)}:00 – {pad(active + 1)}:00
            </div>
            <div>
              <i className="key key-ok" />
              正常 <b>{tip[0]}</b>
            </div>
            <div>
              <i className="key key-bad" />
              异常 <b>{tip[1]}</b>
            </div>
          </div>
        )}
      </div>
      <div className="haxis">
        {[0, 6, 12, 18, 24].map((h) => (
          <span key={h} style={{ left: `${(h / 24) * 100}%` }}>
            {pad(h)}:00
          </span>
        ))}
      </div>
    </div>
  );
}

function Row({ r, open, toggle, maxOut }: { r: MonitorRow; open: boolean; toggle: () => void; maxOut: number }) {
  const t = new Date(r.ts * 1000);
  const hot = !!r.code && /^[45]/.test(r.code);
  const split = r.reply != null && r.think != null;
  const badge = r.flag ?? (r.ok ? null : r.code);
  const cat = r.refusal?.category ?? null;
  const stopTag = r.tags.find((x) => x.startsWith("stop_reason "));
  const stop = stopTag ? stopTag.slice(12) : "no stop";
  // stop_reason 和拒绝分类各有专行，这里只留其余的标记
  const others = r.tags.filter((x) => !x.startsWith("stop_reason ") && x !== "no stop" && x !== cat);
  const badges = (
    <>
      {badge && (
        <span className="rbadge" title={cat ?? undefined}>
          {badge}
        </span>
      )}
      {r.sig === "bad" && <span className="rbadge">model mismatch</span>}
    </>
  );
  return (
    <div className={`lrow ${r.ok ? "" : "bad"} ${open ? "open" : ""}`}>
      <button type="button" className="lrow-head" onClick={toggle} aria-expanded={open}>
        <span className="c-time mono">
          {pad(t.getHours())}:{pad(t.getMinutes())}
          <span className="faint only-d">:{pad(t.getSeconds())}</span>
        </span>
        <span className="c-model">
          <span className="c-model-name">{r.short}</span>
        </span>
        <span className="c-result">
          {badge || r.sig === "bad" ? badges : <span className="rdone">完成</span>}
          {cat && <span className="rstage mono">{cat}</span>}
        </span>
        <span className="c-out mono">
          {r.out > 0 ? (
            <>
              <span className="only-d">
                {split ? (
                  <>
                    {num(r.reply!)}
                    <span className="faint"> + {num(r.think!)}</span>
                  </>
                ) : (
                  num(r.out)
                )}
              </span>
              <span className="only-m">
                {split ? (
                  <>
                    {human(r.reply!)}
                    <span className="faint">+{human(r.think!)}</span>
                  </>
                ) : (
                  human(r.out)
                )}
              </span>
              <span className="obar only-d" style={{ width: `${Math.max(4, (r.out / maxOut) * 100)}%` }}>
                {split ? (
                  <>
                    <i className="o-reply" style={{ flexGrow: r.reply! }} />
                    <i className="o-think" style={{ flexGrow: r.think! }} />
                  </>
                ) : (
                  <i className="o-reply" style={{ flexGrow: 1 }} />
                )}
              </span>
            </>
          ) : (
            <span className="faint">—</span>
          )}
        </span>
        <span className="c-dur mono">{spent(r.ms)}</span>
        <Icon name="chevron" size={14} className="caret" />
      </button>
      {open && (
        <div className="lrow-body fade-in">
          <dl>
            <dt>状态</dt>
            <dd className={`mono ${hot ? "tone-bad" : ""}`}>HTTP {r.code ?? "—"}</dd>
            <dt>结束原因</dt>
            <dd className="mono">{stop}</dd>
            {r.refusal && (
              <>
                <dt>拒绝分类</dt>
                <dd>
                  <span className="mono">{cat ?? "—"}</span>
                  {cat && REFUSAL_DESC[cat] && <span className="faint">{REFUSAL_DESC[cat]}</span>}
                </dd>
                <dt>拦截阶段</dt>
                <dd>{r.refusal.stage === "input" ? "输入拦截，未产生输出" : `生成中断，已输出 ${num(r.out)} token`}</dd>
              </>
            )}
            {others.length > 0 && (
              <>
                <dt>其他标记</dt>
                <dd className="mono">{others.join(" · ")}</dd>
              </>
            )}
            <dt>请求模型</dt>
            <dd className="mono">{r.model ?? "?"}</dd>
            <dt>签名模型</dt>
            {/* opus-5-5、fable-5-1 这一代起签名整段加密，不再带明文模型名，有思考却解不出名字就是这种 */}
            <dd className={r.real ? `mono ${r.sig === "bad" ? "tone-bad" : ""}` : "faint"}>
              {r.real ?? ((r.think ?? 0) > 0 ? "新版签名，不含模型名" : "无签名")}
            </dd>
            <dt>输入 / 输出</dt>
            <dd className="mono">
              {num(r.in)} / {num(r.out)}
            </dd>
            {split && (
              <>
                <dt>正文 / 思考</dt>
                <dd className="mono">
                  {num(r.reply!)} / {num(r.think!)}
                </dd>
              </>
            )}
            {r.cache_read != null && (
              <>
                <dt>缓存读 / 写</dt>
                <dd className="mono">
                  {num(r.cache_read)} / {num(r.cache_write ?? 0)}
                </dd>
              </>
            )}
            {r.rate != null && (
              <>
                <dt>输出速度</dt>
                <dd className="mono">{r.rate} token/s</dd>
              </>
            )}
            <dt>通道</dt>
            <dd>
              {r.stream ? "流式" : "非流式"} · {r.format ?? "?"}
            </dd>
          </dl>
          {r.extra && <pre>{r.extra}</pre>}
        </div>
      )}
    </div>
  );
}

type Period = "today" | "month" | "all";
const PERIODS: [Period, string][] = [
  ["today", "今天"],
  ["month", "本月"],
  ["all", "全部"],
];

function Tally({ data }: { data: MonitorData["tally"] }) {
  const [period, setPeriod] = useState<Period>("month");
  const list = data[period];
  const sum = list.reduce((t, r) => ({ n: t.n + r.n, bad: t.bad + r.bad, out: t.out + r.out, swap: t.swap + r.swap }), {
    n: 0,
    bad: 0,
    out: 0,
    swap: 0,
  });
  // 模型不符从没出现过就不占一列，真出现了页面顶上另有提醒
  const showSwap = sum.swap > 0;
  const since = data.since ? `${Number(data.since.slice(5, 7))} 月 ${Number(data.since.slice(8, 10))} 日` : "";
  return (
    <details className="card tally">
      <summary>
        按模型统计
        <Icon name="chevron" size={14} className="caret" />
      </summary>
      <div className="tally-bar">
        <div className="seg" role="group">
          {PERIODS.map(([k, label]) => (
            <button type="button" key={k} aria-pressed={period === k} onClick={() => setPeriod(k)}>
              {label}
            </button>
          ))}
        </div>
        {period === "all" && since && <span className="tally-note">自 {since}起</span>}
      </div>
      {list.length === 0 ? (
        <div className="empty">这段时间没有请求。</div>
      ) : (
        <table>
          <thead>
            <tr>
              <th>模型</th>
              <th>请求</th>
              <th>异常</th>
              <th>输出 token</th>
              {showSwap && <th>模型不符</th>}
            </tr>
          </thead>
          <tbody>
            {list.map((r) => (
              <tr key={r.name}>
                {/* 请求在拿到模型名之前就失败了，日志里记成 ? */}
                <td className={r.name === "?" ? "faint" : undefined}>{r.name === "?" ? "unknown" : r.name}</td>
                <td>{num(r.n)}</td>
                <td className={r.bad ? "tone-bad" : ""}>{num(r.bad)}</td>
                <td>{human(r.out)}</td>
                {showSwap && <td className={r.swap ? "tone-bad" : ""}>{r.swap}</td>}
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr>
              <td>合计</td>
              <td>{num(sum.n)}</td>
              <td className={sum.bad ? "tone-bad" : ""}>{num(sum.bad)}</td>
              <td>{human(sum.out)}</td>
              {showSwap && <td className={sum.swap ? "tone-bad" : ""}>{sum.swap}</td>}
            </tr>
          </tfoot>
        </table>
      )}
    </details>
  );
}

export default function Monitor() {
  const [data, setData] = useState<MonitorData | null>(null);
  const [err, setErr] = useState("");
  const [loading, setLoading] = useState(false);
  const [onlyBad, setOnlyBad] = useState(false);
  const [opened, setOpened] = useState<Set<string>>(new Set());

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setData(await getMonitor());
      setErr("");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  // 用内容做键，刷新后新请求插到最上面，已展开的那条也不会错位
  const key = (r: MonitorRow) => `${r.ts}-${r.model}-${r.ms}-${r.in}-${r.out}`;
  const toggle = (k: string) =>
    setOpened((s) => {
      const n = new Set(s);
      if (n.has(k)) n.delete(k);
      else n.add(k);
      return n;
    });

  const today = data?.today;
  const now = data ? new Date(data.now) : null;
  const rows = data?.rows ?? [];
  const badCount = rows.filter((r) => !r.ok).length;
  const maxOut = Math.max(1, ...rows.map((r) => r.out));
  const visible = rows.map((r) => ({ r, k: key(r) })).filter(({ r }) => !onlyBad || !r.ok);
  // 向下取整：出过事就不许四舍五入成 100%
  const success = today && today.count ? Math.floor(((today.count - today.bad) / today.count) * 100) : null;
  const maxReason = today?.reasons.length ? today.reasons[0].n : 1;

  let lastDay = "";
  const todayKey = now ? dayKey(now) : "";

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">监控</h1>
          <p className="page-sub">
            {now ? `数据截至 ${now.toLocaleTimeString("zh-CN", { hour12: false })}` : "加载中…"}
          </p>
        </div>
        <button className="btn btn-ghost" onClick={load} disabled={loading}>
          <Icon name="refresh" size={16} className={loading ? "spin" : undefined} />
          刷新
        </button>
      </div>

      {err && (
        <div className="banner">
          <span>无法获取监控数据：{err}</span>
          <button className="btn btn-ghost" onClick={load}>
            重试
          </button>
        </div>
      )}

      {!data && !err && <div className="skeleton" style={{ height: 320 }} />}

      {data && today && now && (
        <>
          <section className="card overview fade-in">
            <div className="ov-head">
              <div className="hero">
                <div className="hero-v num">{today.count}</div>
                <div className="hero-k">今日请求</div>
              </div>
              <div className="ov-figs">
                <div className="fig">
                  <div className="fig-v num">{success == null ? "—" : `${success}%`}</div>
                  <div className="fig-k">成功率</div>
                </div>
                <div className="fig">
                  <div className="fig-v num">{today.bad}</div>
                  <div className="fig-k">
                    <i className="key key-bad" />
                    异常
                  </div>
                </div>
              </div>
            </div>
            <HourChart hours={data.hours} nowHour={now.getHours()} />
            <div className="legend-row">
              <span>
                <i className="key key-ok" />
                正常
              </span>
              <span>
                <i className="key key-bad" />
                异常
              </span>
              <span className="faint">每小时请求数</span>
            </div>
          </section>

          <div className="kpis fade-in">
            <div className="card kpi">
              <div className="kpi-k">今日输出</div>
              <div className="kpi-v num">{human(today.out)}</div>
              {today.reply != null && today.think != null && today.out > 0 ? (
                <>
                  <div className="splitbar">
                    <i className="o-reply" style={{ flexGrow: today.reply }} />
                    <i className="o-think" style={{ flexGrow: today.think }} />
                  </div>
                  <div className="kpi-d">
                    正文 {human(today.reply)} · 思考 {human(today.think)}
                  </div>
                </>
              ) : (
                <div className="kpi-d">token</div>
              )}
            </div>
            <div className="card kpi">
              <div className="kpi-k">耗时中位数</div>
              <div className="kpi-v num">{spent(today.median_ms)}</div>
              <div className="kpi-d">半数请求快于此值</div>
            </div>
            <div className="card kpi">
              <div className="kpi-k">缓存命中率</div>
              <div className="kpi-v num">{today.cache_pct == null ? "—" : `${today.cache_pct}%`}</div>
              {today.cache_pct != null && (
                <div className="meterbar">
                  <i style={{ width: `${today.cache_pct}%` }} />
                </div>
              )}
              <div className="kpi-d">按输入 token 计</div>
            </div>
          </div>

          {today.reasons.length > 0 && (
            <section className="card reasons-card fade-in">
              <h3 className="card-title">今日异常原因</h3>
              <div className="rlist">
                {today.reasons.map((x) => {
                  const refusal = x.key === "refusal";
                  const name = refusal ? (x.category ?? "refusal") : x.key;
                  const desc = refusal ? ((x.category && REFUSAL_DESC[x.category]) ?? "拒绝") : flagDesc(x.key);
                  const w = (n: number) => `${(n / maxReason) * 100}%`;
                  return (
                    <div className="ritem" key={`${x.key}-${x.category}`}>
                      <span className="rname">
                        <span className="rname-main mono" title={name}>
                          {SHORT[name] ?? name}
                        </span>
                        <span className="rname-sub">{desc}</span>
                      </span>
                      {/* 拒绝按阶段分两段：实心是输入拦截，淡色是生成中断 */}
                      <span
                        className="rtrack"
                        title={refusal ? `${STAGE.input} ${x.input} · ${STAGE.output} ${x.output}` : undefined}
                      >
                        {refusal ? (
                          <>
                            {x.input > 0 && <i style={{ width: w(x.input) }} />}
                            {x.output > 0 && <i className="late" style={{ width: w(x.output) }} />}
                          </>
                        ) : (
                          <i style={{ width: w(x.n) }} />
                        )}
                      </span>
                      <span className="rnum num">
                        {refusal ? (
                          <>
                            {x.input}
                            <span className="rnum-late">/{x.output}</span>
                          </>
                        ) : (
                          x.n
                        )}
                      </span>
                    </div>
                  );
                })}
              </div>
              {today.reasons.some((x) => x.key === "refusal") && (
                <div className="rlegend">
                  <span>
                    <i className="key key-bad" />
                    {STAGE.input}
                  </span>
                  <span>
                    <i className="key key-late" />
                    {STAGE.output}
                  </span>
                </div>
              )}
            </section>
          )}

          {data.swapped.count > 0 && (
            <div className="banner" style={{ marginTop: 16 }}>
              <span>
                检测到 {data.swapped.count} 次模型不符。最近一次请求 {data.swapped.model}，签名为 {data.swapped.real}。
              </span>
            </div>
          )}
        </>
      )}

      {data && rows.length > 0 && (
        <>
          <section className="card log fade-in">
            <div className="log-head">
              <h3 className="card-title">请求记录</h3>
              <span className="log-note">最近 {rows.length} 条</span>
              <div className="seg" role="group">
                <button type="button" aria-pressed={!onlyBad} onClick={() => setOnlyBad(false)}>
                  全部
                </button>
                <button type="button" aria-pressed={onlyBad} onClick={() => setOnlyBad(true)}>
                  仅异常{badCount > 0 && <span className="num"> {badCount}</span>}
                </button>
              </div>
            </div>
            <div className="lcols only-d">
              <span>时间</span>
              <span>模型</span>
              <span>结果</span>
              <span>输出 token（正文 + 思考）</span>
              <span style={{ textAlign: "right" }}>耗时</span>
              <span />
            </div>
            {visible.length === 0 && <div className="empty">最近没有异常请求。</div>}
            {visible.map(({ r, k }) => {
              const d = new Date(r.ts * 1000);
              const dk = dayKey(d);
              const label =
                dk !== lastDay ? (dk === todayKey ? "今天" : `${d.getMonth() + 1} 月 ${d.getDate()} 日`) : null;
              lastDay = dk;
              return (
                <div key={k}>
                  {label && <div className="lday">{label}</div>}
                  <Row r={r} open={opened.has(k)} toggle={() => toggle(k)} maxOut={maxOut} />
                </div>
              );
            })}
          </section>

          <Tally data={data.tally} />
        </>
      )}

      {data && rows.length === 0 && <div className="card empty">暂无请求记录。</div>}
    </div>
  );
}
