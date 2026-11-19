// The one xterm.js bundle the terminal panel loads (app/vendor/xterm.js and
// its stylesheet app/vendor/xterm.css, built by `npm run build`). One file,
// so the addons share the core's instance, like the CodeMirror bundle. The
// stylesheet carries JetBrains Mono (OFL), the terminal's web font; its
// .woff2 files land beside it.
import "@xterm/xterm/css/xterm.css";
import "@fontsource-variable/jetbrains-mono/index.css";
export { Terminal } from "@xterm/xterm";
export { FitAddon } from "@xterm/addon-fit";
export { WebLinksAddon } from "@xterm/addon-web-links";
export { Unicode11Addon } from "@xterm/addon-unicode11";
export { WebglAddon } from "@xterm/addon-webgl";
