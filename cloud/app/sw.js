// Rudder app service worker. The build (cloud/scripts/build-app.mjs) writes
// `BUILD` (this build's id) and `PRECACHE` (the shell, every hashed asset, the
// icons and the manifest) in front of this file, so each build is a new
// worker: it installs with its own cache and drops every earlier one.
//   - The shell (navigations): network first, so a deploy shows on the next
//     open; the cached shell only when offline.
//   - /app/assets/: content-hashed and immutable, so from the cache.
//   - Other /app files (vendor bundles, icons): network first, cached copy
//     when offline.
// Never touches /api/ or sign-in.
const CACHE = `rudder-app-${BUILD}`;

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE)
      .then((c) => c.addAll(PRECACHE.map((u) => new Request(u, { cache: "reload" }))))
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
  if (url.origin !== self.location.origin) return;
  if (!(url.pathname === "/app" || url.pathname.startsWith("/app/"))) return;
  if (url.pathname.startsWith("/app/login")) return;

  if (url.pathname.startsWith("/app/assets/")) {
    event.respondWith(caches.match(req).then((hit) => hit || fetch(req)));
    return;
  }

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
