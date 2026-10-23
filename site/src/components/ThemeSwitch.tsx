import { ThemeToggle } from "@driif/motiv";
import { useEffect, useSyncExternalStore } from "react";

// The same key as the early theme script of Starlight. An empty or missing value means "follow the system".
const STORAGE_KEY = "starlight-theme";

function subscribe(onChange: () => void) {
  const observer = new MutationObserver(onChange);
  observer.observe(document.documentElement, { attributeFilter: ["data-theme"] });
  return () => observer.disconnect();
}

const isDark = () => document.documentElement.dataset.theme !== "light";

export default function ThemeSwitch() {
  const dark = useSyncExternalStore(subscribe, isDark, () => typeof document === "undefined" || isDark());

  useEffect(() => {
    const query = matchMedia("(prefers-color-scheme: light)");
    const follow = () => {
      if (!localStorage.getItem(STORAGE_KEY)) document.documentElement.dataset.theme = query.matches ? "light" : "dark";
    };
    query.addEventListener("change", follow);
    return () => query.removeEventListener("change", follow);
  }, []);

  function toggle() {
    const theme = dark ? "light" : "dark";
    document.documentElement.dataset.theme = theme;
    localStorage.setItem(STORAGE_KEY, theme);
  }

  return <ThemeToggle dark={dark} onToggle={toggle} />;
}
