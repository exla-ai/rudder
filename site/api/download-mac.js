// GET /download/mac (vercel.json rewrites it here): count the download in
// PostHog, then send the browser to the DMG on the latest GitHub release.
//
// Anonymous: no IP, user agent or cookie is kept, and each download gets a
// random id. The event says where the click came from (?from=, the referring
// site's domain, utm tags), the OS and chip the browser reports, and whether
// it looks like a bot or a link preview (Slack and X fetch a link to unfurl
// it), so dashboards can leave those out.
const DMG = "https://github.com/exla-ai/rudder/releases/latest/download/Rudder.dmg";
const POSTHOG = "https://us.i.posthog.com/i/v0/e/";
// The same write-only project token as site/analytics.js.
const KEY = "phc_2KfOzM4nSxjAgPDfgEikP7Kp7JVcU5aJ0kAyEQykkmn";
const CAPTURE_TIMEOUT_MS = 1500;
const BOT = /bot|crawl|spider|preview|slack|discord|telegram|whatsapp|facebookexternalhit|linkedin|embedly|curl|wget|python|go-http|node-fetch|headless/i;

function word(value) {
  const text = String(value || "").trim().toLowerCase().slice(0, 60);
  return /^[a-z0-9 ._-]+$/.test(text) ? text : null;
}

function domain(referrer) {
  try {
    return new URL(String(referrer)).hostname.replace(/^www\./, "");
  } catch {
    return null;
  }
}

function os(ua, platformHint) {
  const hint = String(platformHint || "").replace(/"/g, "").toLowerCase();
  if (hint === "macos" || /Macintosh|Mac OS X/.test(ua)) return /iPhone|iPad/.test(ua) ? "ios" : "mac";
  if (/iPhone|iPad|iPod/.test(ua)) return "ios";
  if (hint === "android" || /Android/.test(ua)) return "android";
  if (hint === "windows" || /Windows/.test(ua)) return "windows";
  if (hint === "linux" || hint === "chrome os" || /Linux|CrOS/.test(ua)) return "linux";
  return "unknown";
}

// Chromium sends its chip only after the site asked for it (Accept-CH in
// vercel.json); Safari and Firefox never do.
function arch(hint) {
  const value = String(hint || "").replace(/"/g, "").toLowerCase();
  return value === "arm" ? "arm64" : value === "x86" ? "x86_64" : "unknown";
}

export default async function handler(req, res) {
  if (req.method === "GET") {
    const url = new URL(req.url, "https://rudder.exla.ai");
    const ua = String(req.headers["user-agent"] || "");
    const referrer = domain(req.headers.referer);
    const fromSite = referrer === "rudder.exla.ai";
    const properties = {
      $process_person_profile: false,
      source: "site",
      app: "mac",
      from: word(url.searchParams.get("from")) || (fromSite ? "site" : referrer ? "link" : "direct"),
      referrer_domain: referrer || "direct",
      utm_source: word(url.searchParams.get("utm_source")),
      utm_medium: word(url.searchParams.get("utm_medium")),
      utm_campaign: word(url.searchParams.get("utm_campaign")),
      os: os(ua, req.headers["sec-ch-ua-platform"]),
      arch: arch(req.headers["sec-ch-ua-arch"]),
      bot: BOT.test(ua),
    };
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), CAPTURE_TIMEOUT_MS);
    try {
      await fetch(POSTHOG, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ api_key: KEY, event: "app_downloaded", distinct_id: `download_${crypto.randomUUID()}`, properties, timestamp: new Date().toISOString() }),
        signal: controller.signal,
      });
    } catch {
      // A slow or failed count never holds up the download.
    } finally {
      clearTimeout(timer);
    }
  }
  res.statusCode = 302;
  res.setHeader("Location", DMG);
  res.setHeader("Cache-Control", "no-store");
  res.end();
}
