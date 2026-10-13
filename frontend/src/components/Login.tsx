import { useState, type FormEvent } from "react";
import { checkPassword, token } from "../api";
import { Logo, Spinner } from "./ui";

export default function Login({ onDone, notice }: { onDone: () => void; notice?: string }) {
  const [pw, setPw] = useState("");
  const [busy, setBusy] = useState(false);
  const [hint, setHint] = useState(notice || "");

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!pw.trim()) return setHint("请输入管理员密钥");
    setBusy(true);
    setHint("");
    try {
      if (await checkPassword(pw.trim())) {
        token.set(pw.trim());
        onDone();
      } else {
        setHint("密钥无效");
      }
    } catch {
      setHint("无法连接服务器");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <div className="login-box">
        <Logo size={76} className="login-mark" />
        <h1>ClewdR</h1>
        <p className="lead">管理控制台</p>
        <form onSubmit={submit}>
          <input
            className="input input-lg"
            type="password"
            autoComplete="current-password"
            autoFocus
            placeholder="管理员密钥"
            value={pw}
            onChange={(e) => setPw(e.target.value)}
            disabled={busy}
          />
          <button className="btn btn-primary btn-block" disabled={busy}>
            {busy ? <Spinner /> : "登录"}
          </button>
          <div className="hint" style={notice && hint === notice ? { color: "var(--muted)" } : undefined}>
            {hint}
          </div>
        </form>
      </div>
    </div>
  );
}
