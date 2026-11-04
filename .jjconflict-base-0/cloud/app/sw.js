// Rudder app service worker: network first for the shell, cached copy when offline.
// Never touches /api/.
const CACHE = "rudder-app-v5";
const SHELL = [
  "/app", "/app/app.js", "/app/render.js", "/app/core.js", "/app/plan.js", "/app/model-picker.js",
  "/app/org.js", "/app/team.js", "/app/onboarding.js", "/app/mac-local.js", "/app/styles.css", "/app/desktop.css",
  "/app/manifest.webmanifest", "/app/icon.svg", "/app/favicon.svg", "/app/apple-touch-icon.png", "/app/icon-512.png",
];
const CDN = "https://cdn.jsdelivr.net/npm/";

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE)
      .then((c) => Promise.all(SHELL.map((u) => c.add(new Request(u, { cache: "reload" })).catch(() => null))))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);

  // Pinned CDN libraries never change: cache first.
  if (url.href.startsWith(CDN)) {
    event.respondWith(
      caches.match(req).then((hit) => hit || fetch(req).then((res) => {
        if (res.ok) { const copy = res.clone(); caches.open(CACHE).then((c) => c.put(req, copy)); }
        return res;
      })),
    );
    return;
  }

  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith("/api/")) return;
  if (!(url.pathname === "/app" || url.pathname.startsWith("/app/"))) return;
  if (url.pathname === "/app/login" || url.pathname.startsWith("/app/login")) return;

  const isNav = req.mode === "navigate";
  event.respondWith((async () => {
    try {
      const res = await fetch(req);
      if (res.ok && res.type === "basic") {
        const copy = res.clone();
        const cacheKey = isNav ? "/app" : req;
        caches.open(CACHE).then((c) => c.put(cacheKey, copy));
      }
      return res;
    } catch (err) {
      const hit = await caches.match(isNav ? "/app" : req);
      if (hit) return hit;
      throw err;
    }
  })());
});
