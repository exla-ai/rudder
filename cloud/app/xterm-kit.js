// xterm.js as the app draws it (/app/vendor/xterm.js, built from
// cloud/editor/xterm.js): the type, the colors and the WebGL renderer shared
// by an agent's terminal (terminal-panel.js) and the machine's shell
// (shell-panel.js).
import { openExternal, toast, PLATFORM } from "/app/core.js";
import { h, copyText } from "/app/render.js";

// Type and behavior, modeled on Ghostty's defaults. The web loads JetBrains
// Mono (free, OFL; its @font-face rides in /app/vendor/xterm.css); the Mac
// app draws with the system's SF Mono, then Menlo.
export const FONT_FAMILY = PLATFORM === "mac"
  ? `ui-monospace, "SF Mono", Menlo, monospace`
  : `"JetBrains Mono Variable", ui-monospace, "SF Mono", Menlo, monospace`;
export const FONT_SIZE = 13;
const LINE_HEIGHT = 1.15;

let lib = null;
const loadCss = () => new Promise((resolve, reject) => {
  const link = h(`<link rel="stylesheet" href="/app/vendor/xterm.css">`);
  link.addEventListener("load", resolve, { once: true });
  link.addEventListener("error", () => reject(new Error("Couldn't load the terminal's stylesheet")), { once: true });
  document.head.appendChild(link);
});
// The stylesheet declares the font, so the font is asked for once it is in:
// a terminal fitted to its panel in some other font's width would be cut off.
const loadLib = () => (lib ??= Promise.all([import("/app/vendor/xterm.js"), loadCss().then(() => document.fonts.load(`${FONT_SIZE}px ${FONT_FAMILY}`))])
  .then(([mod]) => mod).catch((error) => { lib = null; throw error; }));

/** The terminal's colors, from the app's palette (terminal-panel.css) as `el` sees it. */
export function termTheme(el) {
  const css = getComputedStyle(el);
  const color = (name) => {
    const value = css.getPropertyValue(`--term-${name}`).trim();
    if (!value) throw new Error(`terminal-panel.css has no --term-${name}`);
    return value;
  };
  const theme = {
    background: color("bg"), foreground: color("fg"), cursor: color("cursor"), cursorAccent: color("bg"),
    selectionBackground: color("selection"), selectionInactiveBackground: color("selection-inactive"),
    scrollbarSliderBackground: color("scrollbar"), scrollbarSliderHoverBackground: color("scrollbar-hover"),
    scrollbarSliderActiveBackground: color("scrollbar-hover"),
  };
  for (const name of ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]) {
    theme[name] = color(name);
    theme[`bright${name[0].toUpperCase()}${name.slice(1)}`] = color(`bright-${name}`);
  }
  return theme;
}

/** A terminal in `host`, colored for `panel`. WebGL draws it, or nothing
 * does: a browser without WebGL 2 gets an error (its `webgl` is true), not a
 * slower renderer. `onLost(error)` runs when the GPU's context goes and no
 * new one can be made; the terminal is disposed by then. Selecting copies, as
 * Ghostty and iTerm2 do by default: once a drag or a double-click ends. */
export async function newTerm(host, panel, { stdin, onLost }) {
  const { Terminal, FitAddon, WebLinksAddon, Unicode11Addon, WebglAddon } = await loadLib();
  const term = new Terminal({
    fontFamily: FONT_FAMILY, fontSize: FONT_SIZE, lineHeight: LINE_HEIGHT, letterSpacing: 0,
    cursorStyle: "block", cursorBlink: true, cursorInactiveStyle: "outline",
    scrollback: 10000, disableStdin: !stdin, allowProposedApi: true,
    macOptionIsMeta: true, macOptionClickForcesSelection: true,
    drawBoldTextInBrightColors: false, minimumContrastRatio: 1,
    theme: termTheme(panel),
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = "11";
  term.loadAddon(new WebLinksAddon((e, uri) => { e.preventDefault(); openExternal(uri); }));
  term.open(host);
  try { loadWebgl(term, WebglAddon, onLost); }
  catch (error) { term.dispose(); throw error; }
  host.addEventListener("mouseup", () => setTimeout(() => {
    const text = term.hasSelection() ? term.getSelection() : "";
    if (text) void copyText(text).then((ok) => { if (!ok) toast("Couldn't copy the selection", { error: true }); });
  }, 0));
  return { term, fit };
}

function loadWebgl(term, WebglAddon, onLost) {
  let webgl;
  try {
    webgl = new WebglAddon();
    term.loadAddon(webgl);
  } catch {
    throw Object.assign(new Error("This browser can't draw the terminal. It needs WebGL 2."), { webgl: true });
  }
  // The GPU dropped the context (a sleep, a driver reset): draw with a new one.
  // Without one, the terminal goes rather than fall back to xterm's DOM renderer.
  webgl.onContextLoss(() => {
    webgl.dispose();
    try { loadWebgl(term, WebglAddon, onLost); }
    catch (error) { term.dispose(); onLost(error); }
  });
}
