/* Anonymous site analytics, in the same PostHog project as the CLI and the app.
 *
 * No cookies and no storage: the id is random per page load and lives in
 * memory, so there is nothing to consent to and nothing that follows a person
 * around. Events are sent with $process_person_profile:false (anonymous, like
 * src/analytics.ts). Do Not Track and Global Privacy Control turn it off.
 *
 * What is sent: page views (path, referrer domain, utm tags, screen width),
 * clicks on the links that matter (Get started, Download for Mac, TestFlight,
 * docs, GitHub, npm), install-line copies, which sections were seen, and which
 * Slack / Linear demo tab was picked. Never query strings other than utm_*,
 * never anything typed. */
(function () {
  if (navigator.doNotTrack === "1" || window.doNotTrack === "1" || navigator.globalPrivacyControl) return;

  var KEY = "phc_2KfOzM4nSxjAgPDfgEikP7Kp7JVcU5aJ0kAyEQykkmn";
  var URL_ = "https://us.i.posthog.com/i/v0/e/";
  var id = "site-" + Math.random().toString(36).slice(2) + Date.now().toString(36);
  var path = location.pathname.replace(/\/+$/, "") || "/";

  var utm = {};
  new URLSearchParams(location.search).forEach(function (v, k) {
    if (/^utm_(source|medium|campaign|content|term)$/.test(k)) utm[k] = v.slice(0, 100);
  });

  function send(event, props) {
    var p = { $process_person_profile: false, $lib: "rudder-site", source: "site", host: location.host, path: path, $current_url: location.origin + path };
    for (var k in utm) p[k] = utm[k];
    for (var k2 in props || {}) p[k2] = props[k2];
    var body = JSON.stringify({ api_key: KEY, event: event, distinct_id: id, properties: p, timestamp: new Date().toISOString() });
    // text/plain keeps it a simple request (no CORS preflight), and a beacon
    // still goes out when the click navigates away.
    try {
      if (navigator.sendBeacon && navigator.sendBeacon(URL_, new Blob([body], { type: "text/plain" }))) return;
    } catch (e) {}
    try { fetch(URL_, { method: "POST", body: body, keepalive: true, mode: "no-cors" }); } catch (e) {}
  }
  window.rudderSiteTrack = send;

  var ref = "";
  try { if (document.referrer) { var r = new URL(document.referrer); if (r.host !== location.host) ref = r.host; } } catch (e) {}
  send("$pageview", { title: document.title.slice(0, 120), $referring_domain: ref || "$direct", screen_width: window.innerWidth });

  // Links that matter, named by what they do rather than by URL.
  function target(href) {
    var u;
    try { u = new URL(href, location.href); } catch (e) { return null; }
    var same = u.host === location.host;
    if (same && (u.pathname === "/app" || u.pathname.indexOf("/app/") === 0 || u.pathname === "/login")) return "get_started";
    if (same && u.pathname.indexOf("/download/mac") === 0) return "download_mac";
    if (same && u.pathname.indexOf("/docs") === 0) return "docs";
    if (same && u.pathname === "/setup") return "setup";
    if (u.host === "testflight.apple.com") return "testflight";
    if (u.host === "github.com") return "github";
    if (u.host === "www.npmjs.com") return "npm";
    if (u.host === "twitter.com" || u.host === "x.com") return "x";
    if (!same) return "external";
    return null;
  }
  function sectionOf(el) {
    var s = el.closest("section[id], header, footer, nav");
    return s ? (s.id || s.tagName.toLowerCase()) : "page";
  }
  document.addEventListener("click", function (e) {
    var el = e.target.closest && e.target.closest("a[href], [data-copy]");
    if (!el) return;
    if (el.hasAttribute("data-copy")) return send("site_install_copied", { section: sectionOf(el) });
    var t = target(el.getAttribute("href"));
    if (t) send("site_click", { target: t, section: sectionOf(el), label: (el.textContent || "").replace(/\s+/g, " ").trim().slice(0, 40) });
  }, true);

  // The Slack / Linear demo: which tab people pick.
  document.querySelectorAll(".intg-input").forEach(function (r) {
    r.addEventListener("change", function () { if (r.checked) send("site_demo_tab", { tab: r.id.replace(/^intg-/, "") }); });
  });

  // Sections seen, once each per page load: how far people actually read.
  if ("IntersectionObserver" in window) {
    var seen = {};
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) {
        var s = e.target.id;
        if (e.isIntersecting && !seen[s]) { seen[s] = 1; io.unobserve(e.target); send("site_section_viewed", { section: s }); }
      });
    }, { threshold: 0.35 });
    document.querySelectorAll("main section[id]").forEach(function (s) { io.observe(s); });
  }
})();
