// Appearance: Settings > Appearance picks System, Light or Dark for this
// device. A blocking script in <head>, so the first paint already has the
// right colors; styles.css keys its dark palette on html[data-theme="dark"].
// The Mac shell is told the choice too, so its own windows match.
(() => {
  const KEY = "rudder.theme";
  const PICKS = ["system", "light", "dark"];
  const systemDark = matchMedia("(prefers-color-scheme: dark)");
  let pick = "system";
  try {
    const saved = JSON.parse(localStorage.getItem(KEY));
    if (PICKS.includes(saved)) pick = saved;
  } catch { /* storage blocked: follow the system */ }

  const apply = () => {
    const theme = pick === "system" ? (systemDark.matches ? "dark" : "light") : pick;
    document.documentElement.dataset.theme = theme;
    document.querySelector('meta[name="theme-color"]').setAttribute("content", theme === "dark" ? "#111214" : "#f6f4ef");
  };
  // Outside the Mac shell there is no handler, and nothing to tell.
  const tellShell = () => {
    window.webkit?.messageHandlers?.rudder?.postMessage({ type: "appearance", value: pick });
  };

  systemDark.addEventListener("change", () => { if (pick === "system") apply(); });
  window.RudderTheme = {
    get: () => pick,
    set(next) {
      if (!PICKS.includes(next)) throw new Error(`Unknown appearance: ${next}`);
      pick = next;
      try { localStorage.setItem(KEY, JSON.stringify(next)); } catch { /* private mode: this page only */ }
      apply();
      tellShell();
    },
  };
  apply();
  tellShell();
})();
