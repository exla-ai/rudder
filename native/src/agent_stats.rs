//! The selected agent's live numbers, as the app shows them beside a chat
//! (cloud/app/agent-stats.js, cloud/worker/transcripts.mjs): output speed over
//! its last responses and how much of the context window it has left.
//!
//! Both come from the backend's session log, folded by timeline.rs in the
//! timeline job, so nothing here runs on the App loop except formatting.

use super::*;

/// Responses the speed averages over, like the app's `SPEED_RESPONSES`.
pub(crate) const SPEED_RESPONSES: usize = 5;

#[derive(Debug, Clone, Copy)]
struct Response {
    start: Option<u64>,
    end: u64,
    tokens: u64,
    done: bool,
}

/// Output speed over a session log, response by response. A response starts
/// when its request goes out (after the prompt, tool result or turn start
/// before it) and ends at its last recorded block, so it includes time to
/// first token; output tokens include thinking.
#[derive(Default)]
pub(crate) struct SpeedTrack {
    responses: Vec<Response>,
    request_at: Option<u64>,
    current_id: Option<String>,
    output_seen: bool,
    start_at: Option<u64>,
}

impl SpeedTrack {
    /// Claude: a prompt, tool result or `api-request` entry; the newest since
    /// the last response is when the next one went out.
    pub(crate) fn claude_request(&mut self, ts: u64) {
        self.request_at = Some(self.request_at.map_or(ts, |at| at.max(ts)));
    }

    /// Claude: one assistant block. Blocks of one response share its id.
    pub(crate) fn claude_block(&mut self, id: Option<&str>, ts: u64, output: u64, done: bool) {
        if let (Some(id), Some(current)) = (id, self.current_id.as_deref()) {
            if id == current {
                if let Some(last) = self.responses.last_mut() {
                    last.end = ts;
                    last.tokens = last.tokens.max(output);
                    last.done |= done;
                }
                return;
            }
        }
        self.current_id = id.map(str::to_string);
        let start = self.request_at.take();
        self.push(Response { start, end: ts, tokens: output, done });
    }

    /// Codex: an input-side entry (turn start, a user message, a tool output).
    pub(crate) fn codex_input(&mut self, ts: u64) {
        self.request_at = Some(ts);
    }

    /// Codex: an output item (reasoning, an assistant message, a tool call).
    pub(crate) fn codex_output(&mut self) {
        if !self.output_seen {
            self.start_at = self.request_at;
            self.output_seen = true;
        }
    }

    /// Codex: the response's `token_usage_record`.
    pub(crate) fn codex_record(&mut self, ts: u64, output: u64) {
        let start = if self.output_seen { self.start_at } else { self.request_at };
        self.push(Response { start, end: ts, tokens: output, done: true });
        self.start_at = None;
        self.output_seen = false;
        self.request_at = Some(ts);
    }

    fn push(&mut self, response: Response) {
        self.responses.push(response);
        if self.responses.len() > 20 {
            self.responses.remove(0);
        }
    }

    /// Output over the last finished responses. None until one finished
    /// with a measurable time.
    pub(crate) fn speed(&self) -> Option<Speed> {
        let timed: Vec<&Response> = self
            .responses
            .iter()
            .filter(|r| r.done && r.tokens > 0 && r.start.is_some_and(|start| r.end > start))
            .collect();
        let timed = &timed[timed.len().saturating_sub(SPEED_RESPONSES)..];
        let last = timed.last()?;
        let tokens: u64 = timed.iter().map(|r| r.tokens).sum();
        let ms: u64 = timed.iter().map(|r| r.end - r.start.unwrap_or(r.end)).sum();
        Some(Speed { end_ms: last.end, tokens, ms })
    }
}

/// Output tokens and the time they took over the last responses, and when the
/// newest of them ended (the newest conversation's speed wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Speed {
    pub(crate) end_ms: u64,
    pub(crate) tokens: u64,
    pub(crate) ms: u64,
}

impl Speed {
    pub(crate) fn tokens_per_second(self) -> f64 {
        self.tokens as f64 * 1000.0 / self.ms.max(1) as f64
    }
}

/// The window a Claude conversation is measured against: the model's context
/// window (1M for a `[1m]` model, else the models.dev catalog `rudder` keeps),
/// or the auto-compact window set in Claude Code's settings when smaller.
/// Claude Code compacts as the context nears it. Reads files: timeline job only.
pub(crate) fn claude_window(run_model: &str, session_model: Option<&str>) -> Option<u64> {
    let catalog = anthropic_windows();
    let settings = user_home_dir()
        .and_then(|home| fs::read_to_string(home.join(".claude/settings.json")).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    claude_window_from(run_model, session_model, catalog.as_ref(), settings.as_ref())
}

/// The Anthropic models' context windows from the models.dev cache, shaped
/// like the catalog (`anthropic.models.<id>.limit.context`). The cache is
/// megabytes, so it is parsed again only when the file changes.
fn anthropic_windows() -> Option<serde_json::Value> {
    static MEMO: std::sync::Mutex<Option<(PathBuf, SystemTime, u64, serde_json::Value)>> =
        std::sync::Mutex::new(None);
    let path = models_dev_cache_path()?;
    let meta = fs::metadata(&path).ok()?;
    let stamp = (meta.modified().ok()?, meta.len());
    let mut memo = MEMO.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, modified, len, value)) = memo.as_ref() {
        if *cached == path && (*modified, *len) == stamp {
            return Some(value.clone());
        }
    }
    let data: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    let mut models = serde_json::Map::new();
    for (id, entry) in data.pointer("/anthropic/models").and_then(|m| m.as_object()).into_iter().flatten() {
        if let Some(context) = entry.pointer("/limit/context").and_then(serde_json::Value::as_u64) {
            models.insert(id.clone(), serde_json::json!({ "limit": { "context": context } }));
        }
    }
    let value = serde_json::json!({ "anthropic": { "models": models } });
    *memo = Some((path, stamp.0, stamp.1, value.clone()));
    Some(value)
}

pub(crate) fn claude_window_from(
    run_model: &str,
    session_model: Option<&str>,
    catalog: Option<&serde_json::Value>,
    settings: Option<&serde_json::Value>,
) -> Option<u64> {
    let model = session_model.unwrap_or(run_model);
    let model_window = if run_model.to_ascii_lowercase().ends_with("[1m]") {
        Some(1_000_000)
    } else {
        catalog
            .and_then(|c| c.pointer("/anthropic/models"))
            .and_then(|models| models.get(model))
            .and_then(|entry| entry.pointer("/limit/context"))
            .and_then(serde_json::Value::as_u64)
            .filter(|n| *n > 0)
    };
    // Per-model settings are keyed by the canonical id (no date, no [1m]).
    let lower = model.to_ascii_lowercase();
    let canonical = lower.trim_end_matches("[1m]");
    let canonical = match canonical.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => canonical,
    };
    let setting = settings.and_then(|s| {
        s.pointer(&format!("/modelSettings/{canonical}/autoCompactWindow"))
            .or_else(|| s.get("autoCompactWindow"))
    });
    let auto = setting.and_then(serde_json::Value::as_u64).filter(|n| *n > 0);
    match (auto, model_window) {
        (Some(auto), Some(window)) => Some(auto.min(window)),
        (auto, window) => auto.or(window),
    }
}

/// The two halves of `42 tok/s · 63% left`, each when it is known: the speed,
/// and the context (what is left of the window, or its size when the window is
/// not known).
pub(crate) fn stats_parts(summary: &crate::timeline::TimelineSummary) -> (Option<String>, Option<String>) {
    let speed = summary
        .speed
        .map(|speed| format!("{} tok/s", format_rate(speed.tokens_per_second())));
    let context = summary.context.as_ref().map(|context| match context_used_pct(summary) {
        Some(used) => format!("{}% left", (100.0 - used).round().clamp(0.0, 100.0) as u64),
        None => format!("{} context", format_token_count(context.input)),
    });
    (speed, context)
}

/// How full the context is, 0 to 100, when the window is known.
pub(crate) fn context_used_pct(summary: &crate::timeline::TimelineSummary) -> Option<f64> {
    let window = summary.window.filter(|w| *w > 0)?;
    Some(summary.context.as_ref()?.input as f64 / window as f64 * 100.0)
}

fn format_rate(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{}", rate.round() as u64)
    } else {
        format!("{rate:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_averages_the_last_five_finished_responses_and_skips_unfinished_ones() {
        let mut track = SpeedTrack::default();
        // An old slow response, then five at 100 tok/s, then one still streaming.
        track.claude_request(0);
        track.claude_block(Some("old"), 10_000, 10, true);
        for i in 0..5u64 {
            let start = 20_000 + i * 10_000;
            track.claude_request(start);
            let id = format!("m{i}");
            // Streamed blocks of one response: the last carries the full count.
            track.claude_block(Some(&id), start + 500, 20, false);
            track.claude_block(Some(&id), start + 2_000, 200, true);
        }
        track.claude_request(90_000);
        track.claude_block(Some("live"), 90_500, 5, false);
        let speed = track.speed().expect("speed");
        assert_eq!(speed.end_ms, 62_000);
        assert!((speed.tokens_per_second() - 100.0).abs() < 0.01, "{speed:?}");

        let mut codex = SpeedTrack::default();
        codex.codex_input(1_000);
        codex.codex_output();
        codex.codex_input(1_500); // a tool output mid-response does not restart it
        codex.codex_record(3_000, 400);
        assert_eq!(codex.speed().map(|s| s.tokens_per_second().round() as u64), Some(200));
    }

    #[test]
    fn the_claude_window_is_the_smaller_of_the_model_and_its_auto_compact_setting() {
        let catalog = serde_json::json!({"anthropic":{"models":{"claude-opus-4-5-20251101":{"limit":{"context":200_000}}}}});
        let none = serde_json::json!({});
        let per_model = serde_json::json!({"autoCompactWindow": 150_000,
            "modelSettings":{"claude-opus-4-5":{"autoCompactWindow":120_000}}});
        let global = serde_json::json!({"autoCompactWindow": 150_000});
        let session = Some("claude-opus-4-5-20251101");
        assert_eq!(claude_window_from("opus", session, Some(&catalog), Some(&none)), Some(200_000));
        assert_eq!(claude_window_from("opus", session, Some(&catalog), Some(&per_model)), Some(120_000));
        assert_eq!(claude_window_from("opus", session, Some(&catalog), Some(&global)), Some(150_000));
        assert_eq!(claude_window_from("opus[1m]", session, Some(&catalog), Some(&global)), Some(150_000));
        assert_eq!(claude_window_from("opus[1m]", session, None, None), Some(1_000_000));
        assert_eq!(claude_window_from("opus", None, Some(&catalog), None), None);

        let mut summary = crate::timeline::TimelineSummary::default();
        assert_eq!(stats_parts(&summary), (None, None));
        summary.speed = Some(Speed { end_ms: 1, tokens: 424, ms: 10_000 });
        summary.context = Some(crate::timeline::ContextUsage { input: 74_000, ..Default::default() });
        let part = |text: &str| Some(text.to_string());
        assert_eq!(stats_parts(&summary), (part("42 tok/s"), part("74.0k context")));
        summary.window = Some(200_000);
        assert_eq!(stats_parts(&summary), (part("42 tok/s"), part("63% left")));
        summary.speed = None;
        assert_eq!(stats_parts(&summary), (None, part("63% left")));
    }
}
