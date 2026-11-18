// The one CodeMirror 6 bundle the app's file panel loads (app/vendor/codemirror.js,
// built by `npm run build`). Bundling keeps a single @codemirror/state instance;
// separate CDN modules can load two and break every extension check.
export { EditorView, keymap } from "@codemirror/view";
export { EditorState, Compartment } from "@codemirror/state";
export { basicSetup } from "codemirror";
export { indentWithTab } from "@codemirror/commands";
export { StreamLanguage, syntaxHighlighting } from "@codemirror/language";
export { classHighlighter } from "@lezer/highlight";
export { markdown } from "@codemirror/lang-markdown";
export { javascript } from "@codemirror/lang-javascript";
export { json } from "@codemirror/lang-json";
export { python } from "@codemirror/lang-python";
export { rust } from "@codemirror/lang-rust";
export { css } from "@codemirror/lang-css";
export { html } from "@codemirror/lang-html";
export { yaml } from "@codemirror/lang-yaml";
export { shell } from "@codemirror/legacy-modes/mode/shell";
export { toml } from "@codemirror/legacy-modes/mode/toml";
export { go } from "@codemirror/legacy-modes/mode/go";
export { swift } from "@codemirror/legacy-modes/mode/swift";
