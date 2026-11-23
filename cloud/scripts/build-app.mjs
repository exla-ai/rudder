// The web app's build: the files Cloud serves under /app (cloud/dist/app).
//   index.html  the shell, revalidated on every open (no-cache). Built from
//               app/index.html: its stylesheets become one, theme.js goes inline.
//   sw.js       the service worker (app/sw.js), with the build's id and the
//               files it precaches written in front, so every build changes it.
//   assets/     app.js with everything it imports as one module, lazy chunks
//               for its dynamic imports (Markdown, highlighting, ...) and one
//               stylesheet, all under content-hashed names: sent `immutable`.
//   vendor/     CodeMirror and xterm.js (cloud/editor), loaded by fixed name the
//               first time the file editor or a terminal opens (no-cache).
//   the icons and the manifest, as they are.
// `node scripts/build-app.mjs` writes cloud/dist/app with .br/.gz copies of the
// scripts and stylesheets; dev-mock.mjs builds the same files in memory.
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";

const CLOUD = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = path.join(CLOUD, "app");
export const OUT = path.join(CLOUD, "dist", "app");
// /app modules built from cloud/src: the browser half of code the server shares.
const FROM_SRC = {
  "terminal-recovery.js": path.join(CLOUD, "src", "private-access", "terminal-recovery.ts"),
  "project-file-render.js": path.join(CLOUD, "src", "project-files", "browser-render.ts"),
};
const COPIED = ["manifest.webmanifest", "icon.svg", "favicon.svg", "apple-touch-icon.png", "icon-512.png"];
const VENDOR = [path.join(CLOUD, "editor", "codemirror.js"), path.join(CLOUD, "editor", "xterm.js")];

// `/app/<file>` imports, as the browser would ask for them, resolved to the
// sources. /app/vendor/ stays a runtime import of the separately built bundles.
//
// Only third-party libraries load lazily. A dynamic import of an app module
// (`import("/app/home-update.js")`, there to break an import cycle) would make
// esbuild move the code it shares with app.js into extra chunks that every
// open fetches, so the module is bundled with app.js instead and the call
// resolves to its exports from `rudder:loaded`, which the entry fills once
// every module has run. The exports are imported by name: a namespace import
// (`import * as`) would put esbuild's helper for it in a chunk of its own.
const APP_DYNAMIC = /\bimport\("(\/app\/(?!vendor\/)[\w.-]+\.js)"\)/g;

// Each app module some app source imports dynamically, with its export names.
async function lazyAppModules() {
  const found = new Set();
  for (const name of fs.readdirSync(APP)) {
    if (!name.endsWith(".js")) continue;
    for (const m of fs.readFileSync(path.join(APP, name), "utf8").matchAll(APP_DYNAMIC)) found.add(m[1]);
  }
  const lazy = [];
  for (const spec of [...found].sort()) {
    const file = path.join(APP, spec.slice("/app/".length));
    const scan = await esbuild.build({ entryPoints: [file], bundle: false, write: false, metafile: true, format: "esm", outdir: OUT, logLevel: "silent" });
    const [output] = Object.values(scan.metafile.outputs);
    lazy.push({ spec, exports: output.exports });
  }
  return lazy;
}

function appPaths(lazy) {
  return {
    name: "app-paths",
    setup(build) {
      build.onResolve({ filter: /^rudder:(entry|loaded)$/ }, (args) => ({ path: args.path, namespace: "rudder" }));
      build.onLoad({ filter: /^rudder:entry$/, namespace: "rudder" }, () => ({
        resolveDir: APP,
        loader: "js",
        contents: [
          'import "/app/app.js";',
          'import { loaded } from "rudder:loaded";',
          ...lazy.map(({ spec, exports }, i) => [
            `import { ${exports.map((name, j) => `${name} as m${i}_${j}`).join(", ")} } from ${JSON.stringify(spec)};`,
            `loaded[${JSON.stringify(spec)}] = { ${exports.map((name, j) => `${JSON.stringify(name)}: m${i}_${j}`).join(", ")} };`,
          ].join("\n")),
        ].join("\n"),
      }));
      build.onLoad({ filter: /^rudder:loaded$/, namespace: "rudder" }, () => ({ loader: "js", contents: "export const loaded = Object.create(null);" }));
      build.onLoad({ filter: /\.js$/ }, (args) => {
        if (!args.path.startsWith(APP + path.sep)) return undefined;
        const source = fs.readFileSync(args.path, "utf8");
        if (!APP_DYNAMIC.test(source)) return undefined;
        APP_DYNAMIC.lastIndex = 0;
        const contents = 'import { loaded as __rudderLoaded } from "rudder:loaded";\n'
          + source.replace(APP_DYNAMIC, (_, spec) => `Promise.resolve().then(() => __rudderLoaded[${JSON.stringify(spec)}])`);
        return { contents, loader: "js", resolveDir: path.dirname(args.path) };
      });
      build.onResolve({ filter: /^\/app\// }, (args) => {
        const rel = args.path.slice("/app/".length);
        if (rel.startsWith("vendor/")) return { path: args.path, external: true };
        const file = FROM_SRC[rel] ?? path.join(APP, rel);
        if (!fs.existsSync(file)) {
          throw new Error(`${args.importer} imports ${args.path}, which is not in cloud/app`);
        }
        return { path: file };
      });
    },
  };
}

const STYLESHEET = /^\s*<link rel="stylesheet" href="\/app\/([\w.-]+\.css)">\n/gm;
const THEME = '<script src="/app/theme.js"></script>';
const ENTRY = '<script type="module" src="/app/app.js"></script>';

function sourceShell() {
  const html = fs.readFileSync(path.join(APP, "index.html"), "utf8");
  for (const tag of [THEME, ENTRY]) {
    if (!html.includes(tag)) throw new Error(`cloud/app/index.html has no ${tag}`);
  }
  const styles = [...html.matchAll(STYLESHEET)].map((m) => m[1]);
  if (!styles.length) throw new Error("cloud/app/index.html links no stylesheet");
  return { html, styles };
}

function relOut(file) {
  return path.relative(OUT, file).split(path.sep).join("/");
}

// A metafile output (relative to CLOUD, the build's working dir) as the browser asks for it.
const published = (output) => `/app/${relOut(path.join(CLOUD, output))}`;

// app.js and the stylesheets index.html links, bundled.
async function bundle(styles) {
  const common = {
    absWorkingDir: CLOUD, bundle: true, minify: true, write: false, metafile: true, outdir: OUT, charset: "utf8",
    entryNames: "assets/[name]-[hash]", chunkNames: "assets/[name]-[hash]", assetNames: "assets/[name]-[hash]",
    sourcemap: "linked", legalComments: "eof", logLevel: "silent", plugins: [appPaths(await lazyAppModules())],
  };
  const css = await esbuild.build({
    ...common,
    stdin: { contents: styles.map((name) => `@import "/app/${name}";`).join("\n"), loader: "css", resolveDir: APP, sourcefile: "index.html" },
    entryNames: "assets/app-[hash]",
  });
  const js = await esbuild.build({ ...common, entryPoints: [{ in: "rudder:entry", out: "app" }], format: "esm", splitting: true });
  const entry = Object.entries(js.metafile.outputs).find(([, out]) => out.entryPoint)?.[0];
  if (!entry) throw new Error("esbuild wrote no entry module for app.js");
  // The chunks the entry imports statically load with it (modulepreload), not a wave later.
  const preload = [];
  const walk = (out) => {
    for (const dep of js.metafile.outputs[out].imports) {
      if (dep.kind === "import-statement" && !dep.external && !preload.includes(dep.path)) {
        preload.push(dep.path);
        walk(dep.path);
      }
    }
  };
  walk(entry);
  const cssOut = css.outputFiles.find((f) => f.path.endsWith(".css"));
  return {
    files: [...css.outputFiles, ...js.outputFiles],
    entry: published(entry),
    preload: preload.map(published),
    css: `/app/${relOut(cssOut.path)}`,
  };
}

/**
 * The shell's files (everything but vendor/), as published path -> bytes.
 * Throws on any import or tag it cannot resolve: a broken build is never written.
 */
export async function buildShell() {
  const { html, styles } = sourceShell();
  const built = await bundle(styles);
  const files = new Map();
  for (const file of built.files) files.set(relOut(file.path), Buffer.from(file.contents));
  for (const name of COPIED) files.set(name, fs.readFileSync(path.join(APP, name)));
  const theme = fs.readFileSync(path.join(APP, "theme.js"), "utf8").trim();
  let first = true;
  const shell = html
    .replace(/<!--[\s\S]*?-->\n/, "")
    .replace(STYLESHEET, () => {
      if (!first) return "";
      first = false;
      return `  <link rel="stylesheet" href="${built.css}">\n`;
    })
    .replace(THEME, () => `<script>\n${theme}\n</script>`)
    .replace(ENTRY, () => [...built.preload.map((p) => `<link rel="modulepreload" href="${p}">`), `<script type="module" src="${built.entry}"></script>`].join("\n  "));
  files.set("index.html", Buffer.from(shell));
  // The build's id: the shell and every asset it can load. A new id is a new
  // service worker, which drops the caches of every earlier one.
  const assets = [...files.keys()].filter((name) => name.startsWith("assets/") && !name.endsWith(".map")).sort();
  const id = crypto.createHash("sha256");
  for (const name of ["index.html", ...assets, ...COPIED]) id.update(name).update(files.get(name));
  const precache = ["/app", ...assets.map((name) => `/app/${name}`), ...COPIED.map((name) => `/app/${name}`)];
  const sw = fs.readFileSync(path.join(APP, "sw.js"), "utf8");
  files.set("sw.js", Buffer.from(`const BUILD = ${JSON.stringify(id.digest("hex").slice(0, 16))};\nconst PRECACHE = ${JSON.stringify(precache)};\n${sw}`));
  return files;
}

/** CodeMirror and xterm.js (cloud/editor) with xterm's stylesheet and fonts, under vendor/. */
export async function buildVendor() {
  const result = await esbuild.build({
    absWorkingDir: CLOUD, entryPoints: VENDOR, bundle: true, minify: true, format: "esm", legalComments: "none", write: false,
    loader: { ".woff2": "file" }, assetNames: "[name]", outdir: path.join(OUT, "vendor"), logLevel: "silent",
  });
  return new Map(result.outputFiles.map((file) => [relOut(file.path), Buffer.from(file.contents)]));
}

const PRECOMPRESS = /\.(js|css)$/;

async function writeBuild() {
  const files = new Map([...(await buildShell()), ...(await buildVendor())]);
  fs.rmSync(OUT, { recursive: true, force: true });
  for (const [name, body] of files) {
    const file = path.join(OUT, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, body);
    // Sent instead of compressing on the fly (app-static.ts). Written after
    // the file, so never older than it.
    if (PRECOMPRESS.test(name)) {
      fs.writeFileSync(`${file}.br`, zlib.brotliCompressSync(body, {
        params: {
          [zlib.constants.BROTLI_PARAM_MODE]: zlib.constants.BROTLI_MODE_TEXT,
          [zlib.constants.BROTLI_PARAM_QUALITY]: zlib.constants.BROTLI_MAX_QUALITY,
          [zlib.constants.BROTLI_PARAM_SIZE_HINT]: body.length,
        },
      }));
      fs.writeFileSync(`${file}.gz`, zlib.gzipSync(body, { level: zlib.constants.Z_BEST_COMPRESSION }));
    }
  }
  const sizes = [...files].filter(([name]) => !name.endsWith(".map")).map(([name, body]) => `${name} ${body.length}`);
  console.log(`cloud/dist/app: ${files.size} files\n  ${sizes.join("\n  ")}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await writeBuild();
}
