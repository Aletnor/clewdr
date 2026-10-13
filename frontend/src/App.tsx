import { useCallback, useEffect, useState } from "react";
import { Toaster } from "react-hot-toast";
import { checkPassword, getVersion, token } from "./api";
import Accounts from "./components/Accounts";
import Login from "./components/Login";
import Monitor from "./components/Monitor";
import Settings from "./components/Settings";
import { useTheme } from "./theme";
import { Icon, Logo, Spinner } from "./components/ui";

type Tab = "accounts" | "watch" | "settings";

const TABS: { id: Tab; label: string }[] = [
  { id: "accounts", label: "账户" },
  { id: "watch", label: "监控" },
  { id: "settings", label: "设置" },
];

const readTab = (): Tab => {
  try {
    const t = localStorage.getItem("clewdr.tab");
    if (t === "accounts" || t === "watch" || t === "settings") return t;
  } catch {
    /* 隐私模式 */
  }
  return "accounts";
};

export default function App() {
  const [auth, setAuth] = useState<"checking" | "in" | "out">(token.get() ? "checking" : "out");
  const [notice, setNotice] = useState(() => {
    const p = new URLSearchParams(location.search);
    if (p.get("passwordChanged") === "true") {
      history.replaceState({}, "", location.pathname);
      return "管理员密钥已变更，请使用新密钥登录";
    }
    return "";
  });
  const [tab, setTab] = useState<Tab>(readTab);
  const [version, setVersion] = useState("");
  const [leaving, setLeaving] = useState(false);
  const { theme, toggle } = useTheme();

  useEffect(() => {
    if (!leaving) return;
    const t = setTimeout(() => setLeaving(false), 3000);
    return () => clearTimeout(t);
  }, [leaving]);

  useEffect(() => {
    getVersion()
      .then(setVersion)
      .catch(() => {});
    const t = token.get();
    if (!t) return;
    checkPassword(t)
      .then((ok) => {
        if (!ok) token.clear();
        setAuth(ok ? "in" : "out");
      })
      .catch(() => setAuth("in"));
  }, []);

  const kick = useCallback((msg?: string) => {
    token.clear();
    setNotice(msg || "会话已失效，请重新登录");
    setAuth("out");
  }, []);

  const go = (t: Tab) => {
    setTab(t);
    try {
      localStorage.setItem("clewdr.tab", t);
    } catch {
      /* 隐私模式 */
    }
    window.scrollTo({ top: 0 });
  };

  const toaster = (
    <Toaster
      position="top-center"
      toastOptions={{
        style: {
          background: "var(--float-solid)",
          color: "var(--text)",
          border: "1px solid var(--line-hi)",
          boxShadow: "var(--shadow-pop)",
          borderRadius: 999,
          fontSize: 14,
          padding: "8px 16px",
          backdropFilter: "blur(12px)",
        },
        success: { iconTheme: { primary: "var(--honey)", secondary: "var(--on-honey)" } },
        error: { iconTheme: { primary: "var(--rose)", secondary: "#fff" } },
      }}
    />
  );

  if (auth === "checking")
    return (
      <div className="login" style={{ color: "var(--honey)" }}>
        <Spinner size={28} />
      </div>
    );

  if (auth === "out")
    return (
      <>
        {toaster}
        <Login
          notice={notice}
          onDone={() => {
            setNotice("");
            setAuth("in");
          }}
        />
      </>
    );

  return (
    <>
      {toaster}
      <header className="topbar">
        <div className="topbar-inner">
          <div className="brand">
            <Logo size={28} />
            <span className="brand-name">ClewdR</span>
            {version && <span className="brand-ver">{version}</span>}
          </div>
          <nav className="nav">
            {TABS.map((t) => (
              <button key={t.id} aria-current={tab === t.id ? "page" : undefined} onClick={() => go(t.id)}>
                {t.label}
              </button>
            ))}
          </nav>
          <button
            className="icon-btn"
            title={theme === "light" ? "切换到夜间" : "切换到白天"}
            aria-label={theme === "light" ? "切换到夜间" : "切换到白天"}
            onClick={toggle}
          >
            <Icon name={theme === "light" ? "moon" : "sun"} size={18} />
          </button>
          <div className="leave">
            <button
              className={`icon-btn danger ${leaving ? "armed" : ""}`}
              title="退出登录"
              aria-label={leaving ? "再次点击以退出" : "退出登录"}
              onClick={() => (leaving ? kick("已退出登录") : setLeaving(true))}
            >
              <Icon name="logout" size={18} />
            </button>
            {leaving && <div className="leave-tip">再次点击以退出</div>}
          </div>
        </div>
      </header>

      <main key={tab}>
        {tab === "accounts" && <Accounts onAuthFail={kick} />}
        {tab === "watch" && <Monitor />}
        {tab === "settings" && <Settings onAuthFail={kick} />}
      </main>
    </>
  );
}
