import { useEffect, useState } from "react";

// 没选过就跟系统走；手动点过一次就记住，存 localStorage.clewdr.theme
export type Theme = "light" | "dark";

const KEY = "clewdr.theme";
const media = () => window.matchMedia("(prefers-color-scheme: light)");

function stored(): Theme | null {
  try {
    const t = localStorage.getItem(KEY);
    return t === "light" || t === "dark" ? t : null;
  } catch {
    return null;
  }
}

function paint(t: Theme) {
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute("content", t === "light" ? "#fcf7f7" : "#0d0b0a");
}

export function useTheme() {
  const [choice, setChoice] = useState<Theme | null>(stored);
  const [system, setSystem] = useState<Theme>(() => (media().matches ? "light" : "dark"));
  const theme = choice ?? system;

  useEffect(() => {
    const m = media();
    const on = () => setSystem(m.matches ? "light" : "dark");
    m.addEventListener("change", on);
    return () => m.removeEventListener("change", on);
  }, []);

  useEffect(() => {
    if (choice) document.documentElement.dataset.theme = choice;
    else delete document.documentElement.dataset.theme;
    paint(theme);
  }, [choice, theme]);

  const toggle = () => {
    const next: Theme = theme === "light" ? "dark" : "light";
    setChoice(next);
    try {
      localStorage.setItem(KEY, next);
    } catch {
      /* 隐私模式下记不住，本次照样生效 */
    }
  };

  return { theme, toggle };
}
