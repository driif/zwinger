import { ThemeToggle } from "@driif/motiv";
import { useEffect } from "react";

// The same key as the early theme script of Starlight. An empty or missing value means "follow the system".
const STORAGE_KEY = "starlight-theme";

export default function ThemeSwitch() {
  useEffect(() => {
    const query = matchMedia("(prefers-color-scheme: light)");
    const follow = () => {
      if (!localStorage.getItem(STORAGE_KEY)) document.documentElement.dataset.theme = query.matches ? "light" : "dark";
    };
    query.addEventListener("change", follow);
    return () => query.removeEventListener("change", follow);
  }, []);

  function toggle() {
    const theme = document.documentElement.dataset.theme === "light" ? "dark" : "light";
    document.documentElement.dataset.theme = theme;
    localStorage.setItem(STORAGE_KEY, theme);
  }

  return <ThemeToggle onToggle={toggle} />;
}
