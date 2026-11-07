//! Update the Cloud home now. A home only takes a new worker image after five
//! quiet minutes, which never come while agents work all day, so the dashboard
//! offers the update itself. home.mjs owns the machine: it writes
//! `$RUDDER_HOME_UPDATE_DIR/status.json`, and a confirmed key press here writes
//! `request` for it to act on. The status is a tiny local file read at most
//! every few seconds on the App tick; nothing here waits on the network.
use super::*;

pub(crate) const HOME_UPDATE_DIR_ENV: &str = "RUDDER_HOME_UPDATE_DIR";
/// Alt+r, a dashboard chord like Alt+h full screen. `U` opens usage and
/// Alt+u scrolls the worker.
pub(crate) const HOME_UPDATE_ALT_KEY: char = 'r';
/// What Option+r types on a US Mac layout when the terminal keeps the
/// modifier to itself. Only a character, so never taken in the task composer.
pub(crate) const HOME_UPDATE_FALLBACK: char = '\u{00ae}';
/// The key as the footer names it, next to `\u{2325}h`.
pub(crate) const HOME_UPDATE_KEY_LABEL: &str = "\u{2325}r";
const HOME_UPDATE_KEY_NAME: &str = "Alt+r";
/// Lower-case text that marks the confirmation notice (`notice_kind`).
pub(crate) const HOME_UPDATE_CONFIRM_MARKER: &str = "press alt+r again";
const STATUS_READ_EVERY: Duration = Duration::from_secs(3);
const CONFIRM_WITHIN: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeUpdateState {
    Current,
    Available,
    Requested,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HomeUpdateStatus {
    pub(crate) state: HomeUpdateState,
    pub(crate) target: Option<String>,
    pub(crate) error: Option<String>,
}

impl HomeUpdateStatus {
    /// `None` for anything but a known state: the dashboard then shows nothing.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let text = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        };
        let state = match value.get("state")?.as_str()? {
            "current" => HomeUpdateState::Current,
            "available" => HomeUpdateState::Available,
            "requested" => HomeUpdateState::Requested,
            "failed" => HomeUpdateState::Failed,
            _ => return None,
        };
        Some(Self { state, target: text("target"), error: text("error") })
    }

    /// The tag after the last ':' of the target image (`...:4.2.6` is 4.2.6).
    /// A registry port (`host:5000/image`) or a digest is not a version.
    pub(crate) fn version(&self) -> Option<&str> {
        let image = self.target.as_deref()?;
        let image = image.split('@').next().unwrap_or(image);
        let (_, tag) = image.rsplit_once(':')?;
        (!tag.is_empty() && !tag.contains('/')).then_some(tag)
    }

    fn version_label(&self) -> &str {
        self.version().unwrap_or("a new version")
    }

    pub(crate) fn offers_update(&self) -> bool {
        matches!(self.state, HomeUpdateState::Available | HomeUpdateState::Failed)
    }

    fn ready_notice(&self) -> String {
        let what = match self.version() {
            Some(version) => format!("Rudder {version} is"),
            None => "A new version of Rudder is".to_string(),
        };
        format!(
            "{what} ready on this machine. Press {HOME_UPDATE_KEY_NAME} to update; agents restart and pick up where they left off."
        )
    }

    fn failed_notice(&self) -> String {
        let cause = self
            .error
            .as_deref()
            .map(|error| format!(": {}", truncate_chars(error, 160)))
            .unwrap_or_default();
        format!(
            "The update to {} failed{cause}. Press {HOME_UPDATE_KEY_NAME} to try again.",
            self.version_label()
        )
    }

    fn updating_notice(&self) -> String {
        format!("Updating to {}. This screen reconnects by itself.", self.version_label())
    }

    fn confirm_notice(&self) -> String {
        format!(
            "Update to {} now? Agents restart and continue. Press {HOME_UPDATE_KEY_NAME} again to confirm; any other key cancels.",
            self.version_label()
        )
    }

    /// What has been announced: one notice per state and target (and error).
    fn announcement(&self) -> Option<(String, String)> {
        let target = self.target.as_deref().unwrap_or("");
        match self.state {
            HomeUpdateState::Current => None,
            HomeUpdateState::Available => Some((format!("available {target}"), self.ready_notice())),
            HomeUpdateState::Requested => Some((format!("requested {target}"), self.updating_notice())),
            HomeUpdateState::Failed => Some((
                format!("failed {target} {}", self.error.as_deref().unwrap_or("")),
                self.failed_notice(),
            )),
        }
    }
}

/// App state for the home update. Only the home runtime with the env var set
/// ever reads or writes anything.
#[derive(Debug, Default)]
pub(crate) struct HomeUpdate {
    pub(crate) dir: Option<PathBuf>,
    pub(crate) status: Option<HomeUpdateStatus>,
    last_read: Option<Instant>,
    announced: Option<String>,
    /// First press of the update key: when, and the frame count then.
    confirm: Option<(Instant, u64)>,
}

impl HomeUpdate {
    pub(crate) fn from_env() -> Self {
        Self {
            dir: env::var_os(HOME_UPDATE_DIR_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            ..Self::default()
        }
    }
}

/// Write `request` atomically: a temp file in the same directory, then rename.
pub(crate) fn write_home_update_request(dir: &Path, project: &str) -> Result<()> {
    let body = serde_json::json!({ "at": iso_utc_now(), "project": project });
    let temp = dir.join(format!(".request.{}.{}.tmp", std::process::id(), now_stamp()));
    fs::write(&temp, format!("{body}\n"))?;
    if let Err(error) = fs::rename(&temp, dir.join("request")) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

/// `2026-11-06T12:34:56.789Z`, without a timezone database.
fn iso_utc_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let (days, ms_of_day) = (millis.div_euclid(86_400_000), millis.rem_euclid(86_400_000));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms_of_day / 3_600_000,
        ms_of_day / 60_000 % 60,
        ms_of_day / 1000 % 60,
        ms_of_day % 1000
    )
}

impl App {
    fn home_update_enabled(&self) -> Option<PathBuf> {
        self.home_update.dir.clone().filter(|_| self.home_runtime)
    }

    /// True when the footer should offer the update key.
    pub(crate) fn home_update_offered(&self) -> bool {
        self.home_runtime
            && self
                .home_update
                .status
                .as_ref()
                .is_some_and(HomeUpdateStatus::offers_update)
    }

    /// On the App tick: re-read status.json at most every few seconds.
    pub(crate) fn poll_home_update(&mut self) {
        let Some(dir) = self.home_update_enabled() else {
            return;
        };
        // An unanswered question expires: take it down so a late press asks
        // again visibly instead of under a question that no longer counts.
        if self
            .home_update
            .confirm
            .is_some_and(|(at, _)| at.elapsed() > CONFIRM_WITHIN)
        {
            self.home_update.confirm = None;
            let asked = self.home_update.status.as_ref().map(HomeUpdateStatus::confirm_notice);
            if asked.is_some() && self.notice == asked {
                self.notice = None;
                self.dirty = true;
            }
        }
        if self
            .home_update
            .last_read
            .is_some_and(|at| at.elapsed() < STATUS_READ_EVERY)
        {
            return;
        }
        self.home_update.last_read = Some(Instant::now());
        let mut status = fs::read_to_string(dir.join("status.json"))
            .ok()
            .and_then(|raw| HomeUpdateStatus::parse(&raw));
        // A request home.mjs has not picked up yet is already on its way.
        if let Some(status) = status.as_mut() {
            if status.state == HomeUpdateState::Available && dir.join("request").exists() {
                status.state = HomeUpdateState::Requested;
            }
        }
        self.apply_home_update_status(status);
    }

    pub(crate) fn apply_home_update_status(&mut self, status: Option<HomeUpdateStatus>) {
        if status != self.home_update.status {
            self.dirty = true;
        }
        if let Some((key, notice)) = status.as_ref().and_then(HomeUpdateStatus::announcement) {
            if self.home_update.announced.as_deref() != Some(key.as_str()) {
                self.home_update.announced = Some(key);
                self.notice = Some(notice);
                self.dirty = true;
            }
        }
        self.home_update.status = status;
    }

    /// The update chord is claimed only where an update can be asked for, so
    /// Alt+r and '\u{00ae}' reach the pane as before everywhere else.
    pub(crate) fn home_update_claims(&self, key: &KeyEvent) -> bool {
        if self.home_update_enabled().is_none() {
            return false;
        }
        let alt_like = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META);
        match key.code {
            KeyCode::Char(HOME_UPDATE_ALT_KEY) => alt_like,
            KeyCode::Char(HOME_UPDATE_FALLBACK) => self.focus != FocusPane::Task,
            _ => false,
        }
    }

    /// The update chord. First press asks, a second within a few seconds
    /// (after the question has rendered) writes the request.
    pub(crate) fn press_home_update_key(&mut self) {
        let Some(dir) = self.home_update_enabled() else {
            return;
        };
        let Some(mut status) = self.home_update.status.clone() else {
            return;
        };
        match status.state {
            HomeUpdateState::Current => {
                self.notice = Some("Rudder is up to date".to_string());
                return;
            }
            HomeUpdateState::Requested => {
                self.notice = Some(status.updating_notice());
                return;
            }
            HomeUpdateState::Available | HomeUpdateState::Failed => {}
        }
        let now = Instant::now();
        let confirmed = self.home_update.confirm.is_some_and(|(at, frame)| {
            now.duration_since(at) <= CONFIRM_WITHIN && self.frames_drawn > frame
        });
        if !confirmed {
            self.home_update.confirm = Some((now, self.frames_drawn));
            self.notice = Some(status.confirm_notice());
            return;
        }
        self.home_update.confirm = None;
        let project = self
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match write_home_update_request(&dir, &project) {
            Ok(()) => {
                status.state = HomeUpdateState::Requested;
                self.home_update.announced = status.announcement().map(|(key, _)| key);
                self.notice = Some(status.updating_notice());
                self.home_update.status = Some(status);
                self.record_activity("home update requested".to_string());
            }
            Err(error) => {
                self.notice = Some(format!("update request failed: {error}"));
            }
        }
        self.dirty = true;
    }

    /// Any key but a second update press cancels a pending confirmation. The
    /// key is not consumed.
    pub(crate) fn cancel_home_update_confirm(&mut self, key: &KeyEvent) {
        if self.home_update.confirm.is_none() {
            return;
        }
        if !self.home_update_claims(key) {
            self.home_update.confirm = None;
            self.notice = Some("update cancelled".to_string());
        }
    }
}
