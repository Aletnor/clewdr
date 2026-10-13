import { useCallback, useEffect, useRef, useState } from "react";
import { AuthError, checkPassword, getConfig, saveConfig, token, type Config } from "../api";
import { CopyButton, Icon, Switch } from "./ui";

type Field =
  | { key: string; kind: "switch"; title: string; desc?: string }
  | { key: string; kind: "text" | "secret"; title: string; desc?: string; placeholder?: string; nullable?: boolean }
  | { key: string; kind: "area"; title: string; desc?: string; placeholder?: string; nullable?: boolean; rows?: number }
  | { key: string; kind: "number"; title: string; desc?: string; min?: number; max?: number }
  | { key: string; kind: "choice"; title: string; desc?: string; options: { value: string; label: string }[] }
  | { key: string; kind: "info"; title: string; desc?: string };

interface Group {
  icon: string;
  title: string;
  desc?: string;
  fields: Field[];
}

const GROUPS: Group[] = [
  {
    icon: "bolt",
    title: "提示缓存",
    desc: "在上游缓存未变化的提示前缀，后续请求直接命中，降低额度消耗与首字延迟。",
    fields: [
      {
        key: "auto_cache",
        kind: "switch",
        title: "自动缓存",
        desc: "为每个请求自动设置缓存断点。频繁调整系统提示时建议关闭，避免反复写入造成额外开销。",
      },
      {
        key: "cache_ttl",
        kind: "choice",
        title: "缓存时长",
        desc: "对话间隔较长时选择 1 小时。1 小时档的缓存写入按基础价的 2 倍计费。",
        options: [
          { value: "5m", label: "5 分钟" },
          { value: "1h", label: "1 小时" },
        ],
      },
      {
        key: "cache_at_depth",
        kind: "number",
        title: "断点深度",
        desc: "自末尾向前第 N 轮开始缓存。设为 2 可兼容编辑末条消息后重新生成；-1 表示断点置于最后一条消息。",
        min: -1,
        max: 50,
      },
      {
        key: "magic_cache",
        kind: "switch",
        title: "魔法串断点",
        desc: "提示词中出现 GPROXY_MAGIC_STRING 标记时，在该位置设置断点。独立于自动缓存开关。",
      },
    ],
  },
  {
    icon: "key",
    title: "认证与网络",
    fields: [
      { key: "password", kind: "secret", title: "API 密钥", desc: "酒馆等客户端调用接口时使用。" },
      { key: "admin_password", kind: "secret", title: "管理员密钥", desc: "用于登录本控制台。修改后，当前浏览器的凭据会同步更新。" },
      {
        key: "proxy",
        kind: "text",
        title: "出站代理",
        desc: "访问上游时使用的代理，留空则直连。",
        placeholder: "http://host:port",
        nullable: true,
      },
      {
        key: "rproxy",
        kind: "text",
        title: "上游地址覆盖",
        desc: "以自定义地址替代官方端点，留空使用官方地址。",
        placeholder: "https://…",
        nullable: true,
      },
    ],
  },
  {
    icon: "send",
    title: "请求处理",
    fields: [
      { key: "max_retries", kind: "number", title: "最大重试次数", min: 0, max: 20 },
      { key: "preserve_chats", kind: "switch", title: "保留网页端会话", desc: "网页通道请求结束后，保留 claude.ai 上的对话记录。" },
      { key: "web_search", kind: "switch", title: "联网搜索", desc: "仅适用于网页通道。" },
      { key: "enable_web_count_tokens", kind: "switch", title: "网页通道 count_tokens", desc: "允许 count_tokens 请求经由网页通道处理。" },
      { key: "sanitize_messages", kind: "switch", title: "消息清理", desc: "转发前移除消息首尾的空白字符。" },
    ],
  },
  {
    icon: "filter",
    title: "账户调度",
    desc: "启用后，对应状态的账户不参与请求调度。",
    fields: [
      { key: "skip_first_warning", kind: "switch", title: "跳过一次警告账户" },
      { key: "skip_second_warning", kind: "switch", title: "跳过二次警告账户" },
      { key: "skip_restricted", kind: "switch", title: "跳过受限账户" },
      { key: "skip_non_pro", kind: "switch", title: "跳过免费账户" },
      { key: "skip_normal_pro", kind: "switch", title: "跳过正常 Pro 账户" },
      { key: "skip_rate_limit", kind: "switch", title: "跳过冷却中账户" },
    ],
  },
  {
    icon: "quote",
    title: "提示词",
    fields: [
      { key: "use_real_roles", kind: "switch", title: "使用真实角色" },
      { key: "custom_h", kind: "text", title: "Human 前缀", placeholder: "默认", nullable: true },
      { key: "custom_a", kind: "text", title: "Assistant 前缀", placeholder: "默认", nullable: true },
      {
        key: "custom_system",
        kind: "area",
        title: "前置系统提示词",
        desc: "插入至上游 system 起始处，适用于 Claude Code 与 API 通道。留空即停用。",
        nullable: true,
        rows: 4,
      },
      {
        key: "custom_prompt",
        kind: "area",
        title: "网页通道提示词",
        desc: "写入网页请求体的 prompt 字段。",
        rows: 3,
      },
    ],
  },
  {
    icon: "server",
    title: "服务",
    // 这几项点错一下就断线，改动即时生效之后不再放开关，只显示
    desc: "监听地址与端口要改配置文件并重启程序才生效。",
    fields: [
      { key: "ip", kind: "info", title: "监听地址" },
      { key: "port", kind: "info", title: "端口" },
      { key: "check_update", kind: "switch", title: "检查更新" },
      { key: "auto_update", kind: "info", title: "自动更新", desc: "启动时发现新版就自己换上，只能在配置文件里开。" },
    ],
  },
];

const same = (a: unknown, b: unknown) => (a ?? null) === (b ?? null);

function Secret({ value, onChange, onBlur }: { value: string; onChange: (v: string) => void; onBlur: () => void }) {
  const [show, setShow] = useState(false);
  return (
    <div className="input-wrap">
      <input
        className="input mono"
        type={show ? "text" : "password"}
        autoComplete="off"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onBlur={onBlur}
        onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
      />
      <button type="button" className="icon-btn" onClick={() => setShow(!show)} aria-label={show ? "隐藏" : "显示"}>
        <Icon name={show ? "eyeOff" : "eye"} size={16} />
      </button>
    </div>
  );
}

// 接到酒馆要填的两样摆在一起：地址按浏览器打开面板的这个地址推出来，密钥就是「API 密钥」那一格的值
function Connect({ apiKey }: { apiKey: string }) {
  const [show, setShow] = useState(false);
  const endpoint = `${window.location.origin}/code/v1`;
  return (
    <section className="card group fade-in">
      <div className="group-head">
        <h3>
          <Icon name="link" size={17} />
          接入酒馆
        </h3>
        <p>酒馆里 API 选 Claude，把这两项填进「反向代理」的地址和密码。</p>
      </div>
      <div className="row stack">
        <div className="row-text">
          <div className="row-title">反代地址</div>
        </div>
        <div className="input-wrap">
          <input className="input mono" readOnly value={endpoint} onFocus={(e) => e.currentTarget.select()} />
          <CopyButton text={endpoint} title="复制地址" />
        </div>
      </div>
      <div className="row stack">
        <div className="row-text">
          <div className="row-title">API 密钥</div>
        </div>
        <div className="input-wrap two">
          <input className="input mono" readOnly type={show ? "text" : "password"} value={apiKey} />
          <button type="button" className="icon-btn" onClick={() => setShow(!show)} aria-label={show ? "隐藏" : "显示"}>
            <Icon name={show ? "eyeOff" : "eye"} size={16} />
          </button>
          <CopyButton text={apiKey} title="复制密钥" />
        </div>
      </div>
    </section>
  );
}

function Stepper({
  value,
  onChange,
  min = -Infinity,
  max = Infinity,
}: {
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
}) {
  const clamp = (v: number) => Math.min(max, Math.max(min, v));
  return (
    <div className="stepper">
      <button type="button" onClick={() => onChange(clamp(value - 1))} aria-label="减少">
        <Icon name="minus" size={14} />
      </button>
      <input
        type="number"
        inputMode="numeric"
        value={Number.isFinite(value) ? value : ""}
        onChange={(e) => onChange(e.target.value === "" ? 0 : clamp(Number(e.target.value)))}
        style={{ width: String(value).length > 3 ? 64 : 48 }}
      />
      <button type="button" onClick={() => onChange(clamp(value + 1))} aria-label="增加">
        <Icon name="plus" size={14} />
      </button>
    </div>
  );
}

function Row({ f, value, rev, commit }: { f: Field; value: unknown; rev: number; commit: (v: unknown) => void }) {
  // 打字时的内容先留在这一行，离开输入框（数字是停手半秒）才提交；提交完一律按服务器回来的值显示
  const [local, setLocal] = useState(value);
  useEffect(() => setLocal(value), [value, rev]);
  const timer = useRef<number | undefined>(undefined);
  const pending = useRef<(() => void) | null>(null);
  // 半秒没到就切走页面，也把最后那个数存掉
  useEffect(
    () => () => {
      window.clearTimeout(timer.current);
      pending.current?.();
    },
    [],
  );

  const title = <div className="row-title">{f.title}</div>;
  const desc = f.desc && <div className="row-desc">{f.desc}</div>;
  const edit = (v: string) => setLocal("nullable" in f && f.nullable && v === "" ? null : v);
  const flush = () => {
    // 登录框会去掉首尾空格，密钥这里不去掉就会存进一个再也登不进去的值
    const v = f.kind === "secret" && typeof local === "string" ? local.trim() : local;
    // 密钥清空了不存，退回原值：空的存下去就没有密钥可登了
    if (f.kind === "secret" && v === "") return setLocal(value);
    if (!same(v, value)) commit(v);
    else if (v !== local) setLocal(v);
  };

  if (f.kind === "info")
    return (
      <div className="row">
        <div className="row-text">
          {title}
          {desc}
        </div>
        {typeof value === "boolean" ? (
          <span className="faint">{value ? "开启" : "关闭"}</span>
        ) : (
          <span className="mono faint">{String(value ?? "—")}</span>
        )}
      </div>
    );

  if (f.kind === "area" || f.kind === "text" || f.kind === "secret") {
    const isWide = f.kind !== "text" || f.key.startsWith("custom") || f.key.includes("proxy");
    const control =
      f.kind === "secret" ? (
        <Secret value={String(local ?? "")} onChange={edit} onBlur={flush} />
      ) : f.kind === "area" ? (
        <textarea
          className="textarea"
          rows={f.rows ?? 3}
          placeholder={f.placeholder}
          value={String(local ?? "")}
          onChange={(e) => edit(e.target.value)}
          onBlur={flush}
        />
      ) : (
        <input
          className="input"
          placeholder={f.placeholder}
          value={String(local ?? "")}
          onChange={(e) => edit(e.target.value)}
          onBlur={flush}
          onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
          style={isWide ? undefined : { maxWidth: 200 }}
        />
      );
    if (!isWide)
      return (
        <div className="row">
          <div className="row-text">
            {title}
            {desc}
          </div>
          {control}
        </div>
      );
    return (
      <div className="row stack">
        <div className="row-text">
          {title}
          {desc}
        </div>
        {control}
      </div>
    );
  }

  // 只有开关行整行可点；步进器和分段按钮放进 label 会被点标题误触
  if (f.kind === "switch")
    return (
      <label className="row" style={{ cursor: "pointer" }}>
        <div className="row-text">
          {title}
          {desc}
        </div>
        <Switch checked={!!value} onChange={commit} label={f.title} />
      </label>
    );

  const step = (v: number) => {
    setLocal(v);
    window.clearTimeout(timer.current);
    pending.current = () => {
      pending.current = null;
      commit(v);
    };
    timer.current = window.setTimeout(() => pending.current?.(), 500);
  };

  return (
    <div className="row">
      <div className="row-text">
        {title}
        {desc}
      </div>
      {f.kind === "number" && <Stepper value={Number(local ?? 0)} onChange={step} min={f.min} max={f.max} />}
      {f.kind === "choice" && (
        <div className="seg" role="group">
          {f.options.map((o) => (
            <button type="button" key={o.value} aria-pressed={value === o.value} onClick={() => commit(o.value)}>
              {o.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export default function Settings({ onAuthFail }: { onAuthFail: (notice?: string) => void }) {
  const [cfg, setCfg] = useState<Config | null>(null);
  const [revs, setRevs] = useState<Record<string, number>>({});
  const [err, setErr] = useState("");
  // 改动一条一条排队存，前一条存完再读最新配置存下一条，互不覆盖
  const queue = useRef<Promise<void>>(Promise.resolve());
  // 队伍排空了才拿服务器的值刷新页面；中途刷新会把后面几下还没存的点击顶回去，看着就是选中框乱跳
  const inflight = useRef(0);
  const touched = useRef(new Set<string>());

  const load = useCallback(async () => {
    try {
      setCfg(await getConfig());
      setErr("");
    } catch (e) {
      if (e instanceof AuthError) return onAuthFail();
      setErr(e instanceof Error ? e.message : String(e));
    }
  }, [onAuthFail]);

  useEffect(() => {
    load();
  }, [load]);

  const commit = (key: string, v: unknown) => {
    setCfg((c) => (c ? { ...c, [key]: v } : c));
    inflight.current += 1;
    touched.current.add(key);
    queue.current = queue.current.then(async () => {
      try {
        const latest = await getConfig();
        await saveConfig({ ...latest, [key]: v });
        if (key === "admin_password") {
          const pw = String(v ?? "");
          if (!(await checkPassword(pw))) return onAuthFail("管理员密钥已变更，请使用新密钥登录");
          token.set(pw);
        }
      } catch (e) {
        if (e instanceof AuthError) return onAuthFail();
      } finally {
        inflight.current -= 1;
      }
      if (inflight.current) return;
      let fresh: Config;
      try {
        fresh = await getConfig();
      } catch {
        return;
      }
      // 读的这一下里你又点了，就交给那一下排空后再刷新
      if (inflight.current) return;
      setCfg(fresh);
      const keys = [...touched.current];
      touched.current.clear();
      setRevs((r) => Object.fromEntries([...Object.entries(r), ...keys.map((k) => [k, (r[k] ?? 0) + 1])]));
    });
  };

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">设置</h1>
        </div>
      </div>

      {err && (
        <div className="banner">
          <span>无法读取配置：{err}</span>
          <button className="btn btn-ghost" onClick={load}>
            重试
          </button>
        </div>
      )}

      {!cfg && !err && (
        <div className="groups">
          {[220, 260, 300].map((h) => (
            <div key={h} className="skeleton" style={{ height: h }} />
          ))}
        </div>
      )}

      {cfg && (
        <div className="groups">
          <Connect apiKey={String(cfg.password ?? "")} />
          {GROUPS.map((g) => (
            <section key={g.title} className="card group fade-in">
              <div className="group-head">
                <h3>
                  <Icon name={g.icon} size={17} />
                  {g.title}
                </h3>
                {g.desc && <p>{g.desc}</p>}
              </div>
              {g.fields.map((f) => (
                <Row key={f.key} f={f} value={cfg[f.key]} rev={revs[f.key] ?? 0} commit={(v) => commit(f.key, v)} />
              ))}
            </section>
          ))}
        </div>
      )}
    </div>
  );
}
