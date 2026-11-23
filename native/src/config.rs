#![allow(unused_imports)]
//! Reading and writing ~/.rudder config, model defaults, and update notices.
use super::*;

pub(crate) fn load_rudder_config() -> Option<serde_json::Value> {
    let path = rudder_config_path()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Load the config for a mutating write without ever silently discarding a
/// user's settings.
///
/// `load_rudder_config` collapses both "no file" and "file present but
/// unparseable" into `None`. If a saver treated that `None` as "start from
/// defaults", a single transiently-corrupt file (an external editor mid-save,
/// a partial write from an older build) would be overwritten on the next
/// `/model` or another dashboard setting, wiping `backends`,
/// `orchestrator.maxParallel`, etc. Here we distinguish the two cases: a
/// present-but-unparseable file is copied aside to `config.json.corrupt` so the
/// user can recover it, and only then do we fall back to defaults.
fn load_config_for_write(path: &Path) -> serde_json::Value {
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => value,
            Err(_) => {
                if !raw.trim().is_empty() {
                    let backup = path.with_extension("json.corrupt");
                    let _ = fs::write(&backup, &raw);
                }
                default_config_value()
            }
        },
        // Absent (or unreadable) file: a fresh defaults document is correct.
        Err(_) => default_config_value(),
    }
}

pub(crate) fn config_backend(config: &serde_json::Value) -> Option<Backend> {
    config
        .get("lastUsedBackend")
        .and_then(serde_json::Value::as_str)
        .and_then(provider_backend)
        .or_else(|| {
            config
                .get("defaultBackend")
                .and_then(serde_json::Value::as_str)
                .and_then(provider_backend)
        })
}

pub(crate) fn config_model(config: &serde_json::Value, backend: Backend) -> Option<String> {
    config
        .get("backends")?
        .get(backend.as_str())?
        .get("model")?
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .map(|model| match backend {
            Backend::Codex => live_codex_model(model).to_string(),
            _ => model.to_string(),
        })
}

pub(crate) fn config_effort(config: &serde_json::Value, backend: Backend) -> Option<EffortLevel> {
    let backend_config = config.get("backends")?.get(backend.as_str())?;
    let keys: &[&str] = match backend {
        Backend::Claude | Backend::Antigravity => &["effort", "reasoningEffort"],
        Backend::Codex => &["reasoningEffort", "effort"],
        // opencode has no effort flag; read the keys anyway so a config written by
        // hand is not silently ignored if opencode ever gains one.
        Backend::Opencode => &["effort", "reasoningEffort"],
    };
    keys.iter().find_map(|key| {
        backend_config
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .and_then(EffortLevel::parse)
    })
}

/// The account id of the CLI's own config dir, as the app and the steer
/// protocol name it. In memory and in run.json it is `None`.
pub(crate) const MAIN_ACCOUNT: &str = "main";

/// `backends.<backend>.account`: the account new agents on this backend start
/// on. Absent, or "main", is the CLI's own config dir (`None`).
pub(crate) fn config_account(config: &serde_json::Value, backend: Backend) -> Option<String> {
    config
        .get("backends")?
        .get(backend.as_str())?
        .get("account")?
        .as_str()
        .filter(|account| !account.is_empty() && *account != MAIN_ACCOUNT)
        .map(str::to_string)
}

/// The account a NEW agent row on `backend` starts on, read from config.json
/// at launch. A row that continues a conversation keeps its own instead.
pub(crate) fn new_agent_account(backend: Backend) -> Option<String> {
    let preferred = load_rudder_config()
        .as_ref()
        .and_then(|config| config_account(config, backend));
    match usage_account_route(backend, preferred.as_deref()) {
        Ok(Some(id)) => (id != MAIN_ACCOUNT).then_some(id),
        Ok(None) | Err(_) => preferred,
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountAvailability {
    id: String,
    ok: bool,
    #[serde(default)]
    signed_in: bool,
    usage_limit: Option<AccountUsageLimit>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountUsageLimit {
    resets_at: Option<u64>,
}

impl AccountUsageLimit {
    fn active(&self, now: u64) -> bool {
        self.resets_at.is_none_or(|reset| reset > now)
    }
}

fn select_usage_account<'a>(
    rows: &'a [AccountAvailability],
    current: &str,
    now: u64,
) -> Result<Option<&'a str>, String> {
    let exhausted = |row: &AccountAvailability| {
        row.usage_limit.as_ref().is_some_and(|limit| limit.active(now))
    };
    if !rows.iter().any(|row| row.id == current && exhausted(row)) {
        return Ok(None);
    }
    rows.iter()
        .find(|row| {
            (row.ok || row.signed_in && row.usage_limit.is_some())
                && !exhausted(row)
                && (row.id == MAIN_ACCOUNT || valid_account_id(&row.id))
        })
        .map(|row| Some(row.id.as_str()))
        .ok_or_else(|| "This account has reached its usage limit. Wait for the reset or add another account in Settings.".to_string())
}

/// The cloud supervisor owns the account snapshot. Local dashboards without
/// that supervisor keep using their configured account.
pub(crate) fn usage_account_route(backend: Backend, current: Option<&str>) -> Result<Option<String>, String> {
    if !matches!(backend, Backend::Claude | Backend::Codex) {
        return Ok(None);
    }
    let Some(file) = std::env::var_os("RUDDER_ACCOUNT_AVAILABILITY") else {
        return Ok(None);
    };
    let raw = match fs::read(&file) {
        Ok(raw) => raw,
        // The supervisor is still running its first credential check.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not read agent account availability: {error}")),
    };
    let availability: std::collections::HashMap<String, Vec<AccountAvailability>> =
        serde_json::from_slice(&raw).map_err(|error| format!("Invalid agent account availability: {error}"))?;
    let Some(rows) = availability.get(backend.as_str()) else {
        return Ok(None);
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    select_usage_account(rows, current.unwrap_or(MAIN_ACCOUNT), now).map(|replacement| replacement.map(str::to_string))
}

#[cfg(test)]
mod account_rotation_tests {
    use super::*;

    #[test]
    fn account_rotation_only_replaces_a_confirmed_exhausted_account() {
        let rows: Vec<AccountAvailability> = serde_json::from_value(serde_json::json!([
            {"id":"main", "ok":false, "signedIn":true, "usageLimit":{"resetsAt":1000}},
            {"id":"expired", "ok":false, "signedIn":false},
            {"id":"outage", "ok":false, "signedIn":true},
            {"id":"older", "ok":false, "signedIn":true, "usageLimit":{"resetsAt":900}},
            {"id":"newer", "ok":true}
        ])).unwrap();
        assert_eq!(select_usage_account(&rows, MAIN_ACCOUNT, 800).unwrap(), Some("newer"));
        assert_eq!(select_usage_account(&rows, MAIN_ACCOUNT, 900).unwrap(), Some("older"));
        assert_eq!(select_usage_account(&rows, MAIN_ACCOUNT, 1000).unwrap(), None);
        assert!(select_usage_account(&rows[..4], MAIN_ACCOUNT, 899).is_err());
        assert_eq!(select_usage_account(&rows, "outage", 800).unwrap(), None);
        assert_eq!(select_usage_account(&rows, "expired", 800).unwrap(), None);
        assert_eq!(select_usage_account(&rows, "newer", 800).unwrap(), None);
    }
}

/// An extra account's id: 1 to 32 lowercase letters and digits, never "main".
pub(crate) fn valid_account_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && id != MAIN_ACCOUNT
}

/// An extra account's config dir: `<rudder home>/accounts/<backend>/<id>`,
/// where the Rudder home is the directory that holds config.json.
pub(crate) fn account_dir(backend: Backend, id: &str) -> Option<PathBuf> {
    Some(
        rudder_config_path()?
            .parent()?
            .join("accounts")
            .join(backend.as_str())
            .join(id),
    )
}

fn gam_side_from_json(value: &serde_json::Value) -> Option<GamSide> {
    let backend = value
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .and_then(provider_backend)?;
    let model = value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let model = match backend {
        Backend::Codex => live_codex_model(&model).to_string(),
        _ => model,
    };
    let effort = value
        .get("effort")
        .and_then(serde_json::Value::as_str)
        .and_then(EffortLevel::parse);
    Some(GamSide {
        backend,
        model,
        effort,
    })
}

fn gam_side_to_json(side: &GamSide) -> serde_json::Value {
    serde_json::json!({
        "backend": side.backend.as_str(),
        "model": side.model,
        "effort": side.effort.map(EffortLevel::as_str),
    })
}

/// The last `/gam` pair, stored as `gam.last` in config.json.
pub(crate) fn config_gam_last_pair(config: &serde_json::Value) -> Option<GamLastPair> {
    let last = config.get("gam")?.get("last")?;
    Some(GamLastPair {
        generator: gam_side_from_json(last.get("generator")?)?,
        reviewer: gam_side_from_json(last.get("reviewer")?)?,
        in_main: last
            .get("inMain")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        task: last
            .get("task")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Remember the pair a `/gam` just launched, so the picker can offer it again
/// and `/gam again` can rerun it in the next session too.
pub(crate) fn save_gam_last_pair(pair: &GamLastPair) -> Result<()> {
    // Tests drive real launches; one must not hand its pair to the next.
    if crate::test_home::on_shared_test_rudder_home() {
        return Ok(());
    }
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    let gam = root
        .entry("gam".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !gam.is_object() {
        *gam = serde_json::json!({});
    }
    if let Some(gam) = gam.as_object_mut() {
        gam.insert(
            "last".to_string(),
            serde_json::json!({
                "generator": gam_side_to_json(&pair.generator),
                "reviewer": gam_side_to_json(&pair.reviewer),
                "inMain": pair.in_main,
                "task": pair.task,
            }),
        );
    }
    write_config_atomically(&path, &config)
}

/// Whether Claude Code's native fast mode is on for new agents. Stored at the
/// config root as `fastMode` (the same key Claude Code's own settings use), so it
/// survives a restart and so we can inject it into a worker's `--settings`. Fast
/// mode is Opus with an accelerated, higher-cost API config — identical model and
/// reasoning, just lower latency — NOT an effort downgrade. Claude-only.
pub(crate) fn config_fast_mode(config: &serde_json::Value) -> Option<bool> {
    config.get("fastMode").and_then(serde_json::Value::as_bool)
}

pub(crate) fn fast_mode_enabled() -> bool {
    load_rudder_config()
        .as_ref()
        .and_then(config_fast_mode)
        .unwrap_or(false)
}

/// Whether Rudder should play a local completion sound when a worker enters review.
/// Default is OFF: audio is useful for some long-running sessions, but surprising
/// as an unconditional dashboard behavior.
pub(crate) fn config_completion_sound(config: &serde_json::Value) -> Option<bool> {
    config
        .get("completionSound")
        .and_then(serde_json::Value::as_bool)
}

pub(crate) fn completion_sound_enabled() -> bool {
    load_rudder_config()
        .as_ref()
        .and_then(config_completion_sound)
        .unwrap_or(false)
}

/// Whether the dashboard emits desktop notifications through the terminal
/// emulator (OSC 777) when an unfocused tab's worker finishes, fails, or needs
/// input. Default ON: unlike the audio ping this only fires for tabs you are
/// NOT looking at, and the emulator/OS notification settings still gate it.
pub(crate) fn config_desktop_notifications(config: &serde_json::Value) -> Option<bool> {
    config
        .get("desktopNotifications")
        .and_then(serde_json::Value::as_bool)
}

pub(crate) fn desktop_notifications_enabled() -> bool {
    load_rudder_config()
        .as_ref()
        .and_then(config_desktop_notifications)
        .unwrap_or(true)
}

/// Persist the `/notify` toggle so the next session keeps the same behavior.
pub(crate) fn save_desktop_notifications(enabled: bool) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "desktopNotifications".to_string(),
        serde_json::Value::Bool(enabled),
    );
    write_config_atomically(&path, &config)
}

/// Dashboard color mode. `terminal` is the default so the native TUI leaves the
/// terminal foreground/background alone and blends with the surrounding tab bar.
/// `paper` preserves the previous hard-white canvas.
pub(crate) fn config_color_mode(config: &serde_json::Value) -> Option<ColorMode> {
    config
        .get("colorMode")
        .and_then(serde_json::Value::as_str)
        .and_then(ColorMode::parse)
}

pub(crate) fn initial_color_mode() -> ColorMode {
    std::env::var("RUDDER_COLOR_MODE")
        .ok()
        .and_then(|value| ColorMode::parse(&value))
        .or_else(|| load_rudder_config().as_ref().and_then(config_color_mode))
        .unwrap_or(ColorMode::Terminal)
}

/// Persist the `/sound` toggle so the next session keeps the same audio behavior.
pub(crate) fn save_completion_sound(enabled: bool) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "completionSound".to_string(),
        serde_json::Value::Bool(enabled),
    );
    write_config_atomically(&path, &config)
}

/// Persist the dashboard color mode so the next session keeps matching the
/// user's terminal background unless they explicitly pick the old paper canvas.
pub(crate) fn save_color_mode(mode: ColorMode) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "colorMode".to_string(),
        serde_json::Value::String(mode.as_str().to_string()),
    );
    write_config_atomically(&path, &config)
}

/// Persist the `/fast` toggle so new sessions and freshly-launched workers see it.
pub(crate) fn save_fast_mode(enabled: bool) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert("fastMode".to_string(), serde_json::Value::Bool(enabled));
    write_config_atomically(&path, &config)
}

/// Atomic temp-file + rename write shared by every config-mutating helper, so a
/// crash mid-write can never truncate ~/.rudder/config.json.
fn write_config_atomically(path: &Path, config: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    fs::write(
        &temp,
        format!("{}\n", serde_json::to_string_pretty(config)?),
    )?;
    fs::rename(&temp, path)?;
    set_private_file_mode(path);
    Ok(())
}

/// Read `orchestrator.maxParallel` from the config, clamped to a sane range.
/// Falls back to `DEFAULT_MAX_PARALLEL` when unset or out of range.
pub(crate) fn config_max_parallel(config: &serde_json::Value) -> usize {
    config
        .get("orchestrator")
        .and_then(|orchestrator| orchestrator.get("maxParallel"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| (1..=100_000).contains(value))
        .unwrap_or(DEFAULT_MAX_PARALLEL)
}

/// `swarm.maxParallel`: how many experiments of one swarm run at once. Falls back
/// to `DEFAULT_SWARM_MAX_PARALLEL` when unset or out of range.
pub(crate) fn config_swarm_max_parallel(config: &serde_json::Value) -> usize {
    config
        .get("swarm")
        .and_then(|swarm| swarm.get("maxParallel"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| (1..=1_000).contains(value))
        .unwrap_or(DEFAULT_SWARM_MAX_PARALLEL)
}

pub(crate) fn swarm_max_parallel() -> usize {
    load_rudder_config()
        .as_ref()
        .map(config_swarm_max_parallel)
        .unwrap_or(DEFAULT_SWARM_MAX_PARALLEL)
}

/// The plan-orchestration parallelism cap for this session.
pub(crate) fn max_parallel() -> usize {
    load_rudder_config()
        .as_ref()
        .map(config_max_parallel)
        .unwrap_or(DEFAULT_MAX_PARALLEL)
}

pub(crate) fn save_model_defaults(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);

    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "lastUsedBackend".to_string(),
        serde_json::Value::String(backend.as_str().to_string()),
    );
    let backends = root
        .entry("backends".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !backends.is_object() {
        *backends = serde_json::json!({});
    }
    let backends = backends
        .as_object_mut()
        .context("Rudder backends config is not an object")?;
    let backend_config = backends
        .entry(backend.as_str().to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !backend_config.is_object() {
        *backend_config = serde_json::json!({});
    }
    let backend_config = backend_config
        .as_object_mut()
        .context("Rudder backend config is not an object")?;
    if model.trim().is_empty() {
        backend_config.remove("model");
    } else {
        backend_config.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    match backend {
        Backend::Claude | Backend::Antigravity => {
            if let Some(effort) = effort {
                backend_config.insert(
                    "effort".to_string(),
                    serde_json::Value::String(effort.as_str().to_string()),
                );
            } else {
                backend_config.remove("effort");
            }
        }
        Backend::Codex => {
            if let Some(effort) = effort {
                backend_config.insert(
                    "reasoningEffort".to_string(),
                    serde_json::Value::String(effort.as_str().to_string()),
                );
            } else {
                backend_config.remove("reasoningEffort");
            }
        }
        Backend::Opencode => {
            backend_config.remove("effort");
            backend_config.remove("reasoningEffort");
        }
    }

    write_config_atomically(&path, &config)
}

pub(crate) fn ensure_config_defaults(config: &mut serde_json::Value) {
    let Some(root) = config.as_object_mut() else {
        return;
    };
    root.remove("autoMerge");
    root.entry("version".to_string())
        .or_insert(serde_json::json!(1));
    root.entry("colorMode".to_string())
        .or_insert(serde_json::json!("terminal"));
    root.entry("runPolicy".to_string()).or_insert_with(|| {
        serde_json::json!({
            "sameCheckout": "single-active",
            "concurrentPromptMode": "workspace"
        })
    });
    root.entry("acpx".to_string())
        .or_insert_with(|| serde_json::json!({ "install": "latest" }));
    root.entry("backends".to_string())
        .or_insert_with(|| serde_json::json!({}));
}

pub(crate) fn default_config_value() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "colorMode": "terminal",
        "runPolicy": {
            "sameCheckout": "single-active",
            "concurrentPromptMode": "workspace"
        },
        "acpx": { "install": "latest" },
        "backends": {
            "claude": {
                "profileId": "anthropic:claude-code",
                "model": "sonnet"
            },
            "codex": {
                "profileId": "openai-codex:default",
                "model": CODEX_DEFAULT_MODEL
            },
            "acpx": {
                "model": CODEX_DEFAULT_MODEL
            }
        }
    })
}

pub(crate) fn rudder_config_path() -> Option<PathBuf> {
    let home = if let Some(value) = std::env::var_os("RUDDER_HOME") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            value
        } else {
            user_home_dir()?.join(".rudder")
        }
    } else {
        user_home_dir()?.join(".rudder")
    };
    Some(home.join("config.json"))
}

pub(crate) fn rudder_cloud_auth_path() -> Option<PathBuf> {
    let home = if let Some(value) = std::env::var_os("RUDDER_HOME") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            value
        } else {
            user_home_dir()?.join(".rudder")
        }
    } else {
        user_home_dir()?.join(".rudder")
    };
    Some(home.join("cloud.json"))
}

pub(crate) fn rudder_cloud_authenticated() -> bool {
    read_cloud_summary().connected
}

pub(crate) fn read_update_notice() -> Option<(String, String)> {
    let latest = env::var("RUDDER_UPDATE_AVAILABLE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let current = env::var("RUDDER_UPDATE_CURRENT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "current".to_string());
    Some((current, latest))
}
