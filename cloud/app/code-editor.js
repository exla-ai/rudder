// CodeMirror 6 for the file panel. The bundle (/app/vendor/codemirror.js, built
// from cloud/editor) loads on the first file opened, never with the app shell.

let bundle = null;
const load = () => (bundle ??= import("/app/vendor/codemirror.js").catch((error) => { bundle = null; throw error; }));

function languageFor(cm, path) {
  const name = path.split("/").pop().toLowerCase();
  const ext = name.includes(".") ? name.slice(name.lastIndexOf(".") + 1) : "";
  if (["md", "markdown"].includes(ext)) return cm.markdown();
  if (["js", "mjs", "cjs"].includes(ext)) return cm.javascript();
  if (ext === "jsx") return cm.javascript({ jsx: true });
  if (["ts", "mts", "cts"].includes(ext)) return cm.javascript({ typescript: true });
  if (ext === "tsx") return cm.javascript({ jsx: true, typescript: true });
  if (["json", "jsonc", "webmanifest"].includes(ext)) return cm.json();
  if (["py", "pyi"].includes(ext)) return cm.python();
  if (ext === "rs") return cm.rust();
  if (["css", "scss"].includes(ext)) return cm.css();
  if (["html", "htm", "svg", "xml"].includes(ext)) return cm.html();
  if (["yml", "yaml"].includes(ext)) return cm.yaml();
  if (["sh", "bash", "zsh"].includes(ext) || [".bashrc", ".zshrc", ".profile"].includes(name)) return cm.StreamLanguage.define(cm.shell);
  if (ext === "toml" || name === "cargo.lock") return cm.StreamLanguage.define(cm.toml);
  if (ext === "go") return cm.StreamLanguage.define(cm.go);
  if (ext === "swift") return cm.StreamLanguage.define(cm.swift);
  return [];
}

// The app's own light/dark tokens; token colors are `.tok-*` rules in files-panel.css.
const theme = (cm) => cm.EditorView.theme({
  "&": { color: "var(--ink)", backgroundColor: "transparent", height: "100%" },
  ".cm-scroller": { fontFamily: "var(--mono)", lineHeight: "1.55" },
  ".cm-content": { caretColor: "var(--ink)" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--ink)" },
  ".cm-gutters": { backgroundColor: "var(--bg-elev)", color: "var(--faint)", borderRight: "1px solid var(--line)" },
  ".cm-activeLine": { backgroundColor: "var(--fp-active-line)" },
  ".cm-activeLineGutter": { backgroundColor: "var(--fp-active-line)", color: "var(--muted)" },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": { backgroundColor: "var(--fp-selection)" },
  ".cm-selectionMatch": { backgroundColor: "var(--fp-match)" },
  "&.cm-focused .cm-matchingBracket": { backgroundColor: "var(--fp-match)", outline: "none" },
  ".cm-panels": { backgroundColor: "var(--surface)", color: "var(--ink)" },
  ".cm-panels.cm-panels-top": { borderBottom: "1px solid var(--line)" },
  ".cm-searchMatch": { backgroundColor: "var(--fp-match)", outline: "1px solid var(--line-strong)" },
  ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "var(--fp-selection)" },
  ".cm-tooltip": { backgroundColor: "var(--surface)", color: "var(--ink)", border: "1px solid var(--line-strong)" },
  ".cm-foldPlaceholder": { backgroundColor: "var(--surface-2)", color: "var(--muted)", border: "none" },
});

/** An editor in `parent`. `onChange` runs after every document edit, `onSave`
 * on Cmd/Ctrl+S. Read-only editors still select, search and copy. */
export async function createEditor({ parent, text, path, readOnly, onChange, onSave }) {
  const cm = await load();
  const editable = new cm.Compartment();
  // CodeMirror's own light or dark styling (search panel, buttons) follows Settings > Appearance.
  const dark = new cm.Compartment();
  const darkTheme = () => cm.EditorView.darkTheme.of(document.documentElement.dataset.theme === "dark");
  const editableState = (locked) => [cm.EditorState.readOnly.of(locked), cm.EditorView.editable.of(!locked)];
  const view = new cm.EditorView({
    parent,
    state: cm.EditorState.create({
      doc: text,
      extensions: [
        cm.basicSetup,
        cm.keymap.of([{ key: "Mod-s", preventDefault: true, run: () => { onSave(); return true; } }, cm.indentWithTab]),
        cm.syntaxHighlighting(cm.classHighlighter),
        cm.EditorView.lineWrapping,
        theme(cm),
        languageFor(cm, path),
        editable.of(editableState(readOnly)),
        dark.of(darkTheme()),
        cm.EditorView.updateListener.of((update) => { if (update.docChanged) onChange(); }),
      ],
    }),
  });
  const appearance = new MutationObserver(() => view.dispatch({ effects: dark.reconfigure(darkTheme()) }));
  appearance.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  return {
    view,
    text: () => view.state.doc.toString(),
    /** Replace the whole document, keeping the cursor where it still fits. */
    setText(next) {
      const head = Math.min(view.state.selection.main.head, next.length);
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: next }, selection: { anchor: head } });
    },
    setReadOnly(locked) { view.dispatch({ effects: editable.reconfigure(editableState(locked)) }); },
    /** Show `line` (1-based) in the middle, selected through `end` when it is
     * past `line`, or the cursor at `col` on it. */
    goto(line, col = 0, end = 0) {
      const doc = view.state.doc;
      const lineAt = (n) => doc.line(Math.min(Math.max(1, n), doc.lines));
      const at = lineAt(line);
      const selection = col > 0 ? { anchor: Math.min(at.from + col - 1, at.to) } : { anchor: at.from, head: (end > line ? lineAt(end) : at).to };
      view.dispatch({ selection, effects: cm.EditorView.scrollIntoView(selection.anchor, { y: "center" }) });
    },
    focus() { view.focus(); },
    destroy() { appearance.disconnect(); view.destroy(); },
  };
}
