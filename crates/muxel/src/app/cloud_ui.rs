//! muxel Cloud on the app side: signing in, unlocking with the Recovery Key,
//! the sync timer, and Settings → Cloud.
//!
//! One window per profile coordinates (it holds `data/cloud/sync.lock`) and
//! runs the sync cycles. Every window saves settings through a three-way merge
//! with `config.toml` ([`merge3`]) and reloads when another window bumps the
//! shared stamp, so pulled changes are never reverted by a stale window.

use super::settings_kit::{Block, Form, Group, Row, Seg};
use super::*;
use muxel_cloud::account::{self, AUK_SECRET, AccountFile, Setup, TOKEN_SECRET};
use muxel_cloud::api::{ApiError, Client, DevicePoll, DeviceStart, Http, Me};
use muxel_cloud::engine::{
    self, ApprovalView, CycleError, Engine, EngineState, Input, MemberDecision, Output, Session,
    TeamSettingWrite, TeamView,
};
use muxel_cloud::store::{CloudDir, Op, TaskHome, TaskStore, WindowPresence};
use muxel_core::cloud::activity::{self, AgentActivity, Presence, ProjectActivity};
use muxel_core::cloud::items::Collection;
use muxel_core::cloud::merge3::WindowSync;
use muxel_core::cloud::roles::Permission;
use muxel_core::cloud::{SERVER_URL_ENV, SyncMode, Tristate, WorkspaceCloud};
use std::sync::atomic::{AtomicBool, Ordering};

/// How often a window without the lock tries to take it over.
const LOCK_RETRY: Duration = Duration::from_secs(10);
/// How often each window rewrites its presence file (see
/// `muxel_cloud::store::PRESENCE_TTL_SECS`).
const PRESENCE_EVERY: Duration = Duration::from_secs(30);
/// How long project keys (git remotes) are cached.
const PROJECT_KEYS_TTL: Duration = Duration::from_secs(600);

pub(super) enum CloudPhase {
    /// Not yet checked (the keychain is read off the UI thread on first tick).
    Unknown,
    SignedOut,
    /// Waiting for the user to approve the device in the browser.
    SigningIn {
        start: DeviceStart,
        next_poll: Instant,
        deadline: Instant,
    },
    /// Talking to the server after sign-in.
    Checking,
    /// The account has keys this device can't open yet.
    NeedsRecovery(AccountFile),
    /// The keychain couldn't be read (often a locked login keyring). Nothing
    /// is signed out or deleted; Retry checks again.
    KeychainError(String),
    Ready,
}

/// What an outbox file the running cycle carried holds.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SentOp {
    /// The cycle's `team_setting_writes[i]`.
    Share(usize),
    /// A member decision. It lives only in the cycle's engine state, so it
    /// stays queued unless the cycle's results are kept.
    Trust,
    Other,
}

pub(super) struct CloudRuntime {
    dir: Option<CloudDir>,
    pub(super) phase: CloudPhase,
    account: Option<AccountFile>,
    /// Held while this window coordinates the profile's sync.
    lock: Option<std::fs::File>,
    lock_tried: Option<Instant>,
    /// A background job (setup, poll or cycle) is running.
    busy: bool,
    /// Set to stop the running sync cycle before its next request (pausing).
    cycle_cancel: Option<Arc<AtomicBool>>,
    last_cycle: Option<Instant>,
    sync_requested: bool,
    pub(super) last_ok: Option<chrono::DateTime<chrono::Local>>,
    pub(super) last_error: Option<String>,
    pub(super) me: Option<Me>,
    pub(super) teams: Vec<TeamView>,
    pub(super) tasks: TaskStore,
    state: Option<EngineState>,
    /// The synced settings fields as this window last loaded or saved them.
    window: WindowSync,
    seen_stamp: Option<String>,
    /// A merge in `persist_settings` changed appearance: reapply it on tick.
    reapply_appearance: bool,
    project_keys: Option<(Instant, Vec<(Uuid, String)>)>,
    /// A just-created account's Recovery Key, shown until dismissed.
    pub(super) new_recovery: Option<String>,
    pub(super) form_error: Option<String>,
    /// Bumped whenever `teams` is replaced.
    teams_gen: u64,
    /// What the team library section was last built from: (bound team,
    /// ready, `teams_gen`).
    team_lib_seen: Option<(Option<Uuid>, bool, u64)>,
    /// The team whose items `library_hub` shows as a drop-down section.
    team_library: Option<Uuid>,
    /// Outbox files the last cycle carried: removed when it completes, sent
    /// again by the next one when it fails.
    sent_ops: Vec<(std::path::PathBuf, SentOp)>,
    /// This window's presence file name.
    window_id: String,
    presence_at: Option<Instant>,
    /// Settings → muxel Cloud → Export: running, and how the last one went.
    pub(super) exporting: bool,
    pub(super) export_result: Option<Result<String, String>>,
}

impl CloudRuntime {
    pub(super) fn new() -> Self {
        let dir = muxel_store::data_dir().map(|d| CloudDir::new(&d));
        let tasks = dir.as_ref().map(CloudDir::load_tasks).unwrap_or_default();
        let teams = dir.as_ref().map(CloudDir::load_teams).unwrap_or_default();
        let seen_stamp = dir.as_ref().and_then(CloudDir::stamp);
        // The plan as of the last sync, so Tasks shows straight away (and in
        // windows that don't run the sync themselves).
        let me = dir.as_ref().and_then(CloudDir::load_me);
        Self {
            dir,
            phase: CloudPhase::Unknown,
            account: None,
            lock: None,
            lock_tried: None,
            cycle_cancel: None,
            busy: false,
            last_cycle: None,
            sync_requested: false,
            last_ok: None,
            last_error: None,
            me,
            teams,
            tasks,
            state: None,
            window: WindowSync::default(),
            seen_stamp,
            reapply_appearance: false,
            project_keys: None,
            new_recovery: None,
            form_error: None,
            teams_gen: 0,
            team_lib_seen: None,
            team_library: None,
            sent_ops: Vec::new(),
            window_id: Uuid::new_v4().simple().to_string(),
            presence_at: None,
            exporting: false,
            export_result: None,
        }
    }

    /// Signed in and unlocked.
    pub(super) fn ready(&self) -> bool {
        matches!(self.phase, CloudPhase::Ready)
    }

    pub(super) fn coordinating(&self) -> bool {
        self.lock.is_some()
    }

    pub(super) fn dir(&self) -> Option<&CloudDir> {
        self.dir.as_ref()
    }

    /// The signed-in, unlocked user.
    pub(super) fn user(&self) -> Option<Uuid> {
        self.ready()
            .then(|| self.account.as_ref().map(|a| a.user))
            .flatten()
    }

    pub(super) fn bump_stamp(&mut self) {
        self.bump();
    }

    pub(super) fn request_sync(&mut self) {
        self.sync_requested = true;
    }

    /// A project's portable key, if the sync has worked it out.
    pub(super) fn project_key_cached(&self, project: Uuid) -> Option<String> {
        self.project_keys
            .as_ref()?
            .1
            .iter()
            .find(|(id, _)| *id == project)
            .map(|(_, key)| key.clone())
    }

    fn has_account(&self) -> bool {
        self.dir.as_ref().is_some_and(CloudDir::has_account)
    }

    /// Stop the running sync cycle, if any, before its next request.
    fn cancel_cycle(&self) {
        if let Some(flag) = &self.cycle_cancel {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Drop this window's session state (not the files or the keychain).
    fn forget_session(&mut self) {
        self.cancel_cycle();
        self.lock = None;
        self.state = None;
        self.account = None;
        self.me = None;
        self.teams.clear();
        self.teams_gen += 1;
        self.new_recovery = None;
        self.last_ok = None;
        self.last_error = None;
    }

    fn bump(&mut self) {
        if let Some(dir) = &self.dir {
            self.seen_stamp = dir.bump_stamp().or_else(|| dir.stamp());
        }
    }
}

fn server_url(cloud: &muxel_core::cloud::CloudSettings) -> String {
    cloud.server(std::env::var(SERVER_URL_ENV).ok().as_deref())
}

fn platform() -> &'static str {
    std::env::consts::OS
}

fn device_name() -> String {
    format!("muxel on {}", crate::control::local_hostname())
}

/// `(project id, portable key)` for every project in every workspace. Runs
/// git, so call it off the UI thread.
fn collect_project_keys() -> Vec<(Uuid, String)> {
    let index = muxel_store::load_workspaces_index();
    let mut out = Vec::new();
    for meta in &index.workspaces {
        let Some(path) = muxel_store::workspace_doc_path(meta.id) else {
            continue;
        };
        let Some(ws) = muxel_store::load_workspace_from(&path) else {
            continue;
        };
        for p in &ws.projects {
            out.push((p.id, project_key_of(p)));
        }
    }
    out
}

fn project_key_of(p: &muxel_core::Project) -> String {
    use muxel_core::cloud::project_key::project_key;
    if p.remote.is_some() || !p.root_path.is_dir() {
        return project_key(None, "", &p.name);
    }
    let git = |args: &[&str]| -> Option<String> {
        let out = crate::integrations::command("git")
            .arg("-C")
            .arg(&p.root_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let remote = git(&["remote", "get-url", "origin"]);
    let sub = git(&["rev-parse", "--show-prefix"]).unwrap_or_default();
    project_key(remote.as_deref(), &sub, &p.name)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A cycle's result, carried back to the UI thread.
struct CycleDone {
    out: Result<Output, CycleFail>,
    state: EngineState,
    projects: Vec<(Uuid, String)>,
    tasks: TaskStore,
    /// The cycle started from nothing (a new device, or a reset): everything it
    /// pulls is the backlog, so it rings no new-task alert.
    first_pull: bool,
    /// Sync was paused while the cycle ran, so it stopped short.
    cancelled: bool,
}

enum CycleFail {
    Cycle(CycleError),
    /// No token in the keychain: signed out (perhaps by another window).
    NoToken,
    /// The keychain couldn't be read. Never treated as signed out.
    Keychain(String),
}

impl From<CycleError> for CycleFail {
    fn from(e: CycleError) -> Self {
        CycleFail::Cycle(e)
    }
}

impl MuxelApp {
    /// Called every second from the status timer.
    pub(super) fn tick_cloud(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = &self.cloud.dir else {
            return;
        };
        let stamp = dir.stamp();
        if !self.cloud.window.has_base() {
            let now = self.settings_base();
            self.cloud.window.saw(&now);
        }
        let team_lib = (
            self.workspace.cloud.as_ref().map(|b| b.team_id),
            self.cloud.ready(),
            self.cloud.teams_gen,
        );
        if self.cloud.team_lib_seen != Some(team_lib) {
            self.cloud.team_lib_seen = Some(team_lib);
            self.sync_team_library(cx);
        }
        if stamp != self.cloud.seen_stamp {
            self.cloud.seen_stamp = stamp;
            self.cloud_reload_from_disk(cx);
            self.cloud_follow_other_windows(cx);
        }
        if std::mem::take(&mut self.cloud.reapply_appearance) {
            self.cloud_apply_appearance(cx);
        }
        if !self.settings.cloud.enabled {
            // Paused, here or in another window: stop a running cycle before
            // its next request, and let another window take over when it's
            // switched back on.
            self.cloud.cancel_cycle();
            self.cloud.lock = None;
        }
        if self
            .cloud
            .presence_at
            .is_none_or(|at| at.elapsed() >= PRESENCE_EVERY)
        {
            self.cloud.presence_at = Some(Instant::now());
            self.publish_presence(cx);
        }
        if self.cloud.busy {
            return;
        }
        match &self.cloud.phase {
            CloudPhase::Unknown => self.cloud_restore(cx),
            CloudPhase::SigningIn {
                next_poll,
                deadline,
                ..
            } => {
                let now = Instant::now();
                if now >= *deadline {
                    self.cloud.phase = CloudPhase::SignedOut;
                    self.cloud.form_error =
                        Some(t("The sign-in code expired. Try again.").to_string());
                    cx.notify();
                } else if now >= *next_poll {
                    self.cloud_poll_sign_in(cx);
                }
            }
            CloudPhase::Ready if self.settings.cloud.enabled => {
                if self.cloud.lock.is_none()
                    && self
                        .cloud
                        .lock_tried
                        .is_none_or(|at| at.elapsed() >= LOCK_RETRY)
                {
                    self.cloud.lock_tried = Some(Instant::now());
                    self.cloud.lock = self.cloud.dir.as_ref().and_then(CloudDir::try_lock);
                    if self.cloud.lock.is_some() {
                        self.cloud.state = None;
                        self.cloud.sync_requested = true;
                    }
                }
                let interval = Duration::from_secs(u64::from(self.settings.cloud.interval()));
                // Something newly queued (a share). Ops a failed cycle carried
                // wait for the next interval rather than retrying every tick.
                let queued = self.cloud.lock.is_some()
                    && self.cloud.dir.as_ref().is_some_and(|dir| {
                        dir.outbox_files()
                            .iter()
                            .any(|f| !self.cloud.sent_ops.iter().any(|(s, _)| s == f))
                    });
                let due = self.cloud.sync_requested
                    || queued
                    || self
                        .cloud
                        .last_cycle
                        .is_none_or(|at| at.elapsed() >= interval);
                if self.cloud.lock.is_some() && due {
                    self.cloud_start_cycle(cx);
                }
            }
            _ => {}
        }
    }

    /// Another window signed in, unlocked or signed out: follow it.
    fn cloud_follow_other_windows(&mut self, cx: &mut Context<Self>) {
        let has = self.cloud.has_account();
        match (&self.cloud.phase, has) {
            (CloudPhase::SignedOut | CloudPhase::NeedsRecovery(_), true) => {
                self.cloud.phase = CloudPhase::Unknown;
            }
            (CloudPhase::Ready | CloudPhase::NeedsRecovery(_), false) => {
                self.cloud.forget_session();
                self.cloud.phase = CloudPhase::SignedOut;
            }
            _ => return,
        }
        cx.notify();
    }

    /// Whether settings saves go through the three-way merge: whenever this
    /// profile is signed in, whatever this window's own state.
    fn cloud_merging(&self) -> bool {
        self.cloud.window.has_base() && self.cloud.has_account()
    }

    /// Before saving: fold in what other windows (or the coordinator) saved
    /// since this window last read `config.toml`. Called from
    /// `persist_settings_checked`.
    pub(super) fn cloud_merge_before_save(&mut self) {
        if !self.cloud_merging() {
            return;
        }
        let Ok(Some(disk)) = muxel_store::try_load_settings() else {
            return;
        };
        let mut live = self.settings_base();
        let (changed, appearance) = self.cloud.window.merge_disk(&mut live, &disk);
        if changed {
            self.cloud_adopt_fields(&live);
        }
        if appearance {
            self.cloud.reapply_appearance = true;
        }
    }

    /// After a successful save: remember what was saved, and tell the other
    /// windows.
    pub(super) fn cloud_after_save(&mut self) {
        let saved = self.settings_base();
        self.cloud.window.saw(&saved);
        if self.cloud.has_account() {
            self.cloud.bump();
        }
    }

    /// Another window saved: pick up its changes, keeping this window's own
    /// unsaved ones.
    fn cloud_reload_from_disk(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = &self.cloud.dir {
            self.cloud.tasks = dir.load_tasks();
            self.cloud.teams = dir.load_teams();
            self.cloud.teams_gen += 1;
            if let Some(me) = dir.load_me() {
                self.cloud.me = Some(me);
            }
        }
        if self.cloud.window.has_base()
            && let Ok(Some(disk)) = muxel_store::try_load_settings()
        {
            let mut live = self.settings_base();
            let (changed, appearance) = self.cloud.window.merge_disk(&mut live, &disk);
            if changed {
                self.cloud_adopt_fields(&live);
            }
            if appearance {
                self.cloud_apply_appearance(cx);
            }
        }
        cx.notify();
    }

    /// Take the synced fields of `s` into this window's live state.
    fn cloud_adopt_fields(&mut self, s: &muxel_core::Settings) {
        self.runners = s.runners.clone();
        self.snippets = s.snippets.clone();
        self.loops = s.loops.clone();
        self.remotes = s.remotes.clone();
        self.identities = s.identities.clone();
        self.theme = s.theme.clone();
        self.theme_mode = s.theme_mode.clone();
        let appearance = muxel_core::cloud::items::Appearance::from_settings(s);
        appearance.apply_to(&mut self.settings);
        self.settings.keybindings = s.keybindings.clone();
        self.settings.terminal_passthrough_keys = s.terminal_passthrough_keys.clone();
        self.settings.cloud = s.cloud.clone();
    }

    /// Make the UI follow the current appearance settings.
    fn cloud_apply_appearance(&mut self, cx: &mut Context<Self>) {
        theme::apply_theme(&self.theme, cx);
        theme::set_ui_scale(self.settings.zoom, cx);
        theme::set_ui_font_size(self.settings.ui_font_size, cx);
        if let Some(lang) = &self.settings.language {
            crate::i18n::set_language(lang);
        }
        self.refresh_terminal_config(cx);
        cx.refresh_windows();
    }

    // --- signing in ---------------------------------------------------------

    /// First tick: is this profile signed in? Reads the keychain off the UI
    /// thread, and only if this profile ever signed in (no keychain access,
    /// and no unlock prompt, for anyone who never used Cloud).
    fn cloud_restore(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.cloud.dir.as_ref().and_then(CloudDir::load_account) else {
            self.cloud.phase = CloudPhase::SignedOut;
            return;
        };
        self.cloud.busy = true;
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let token = crate::secrets::read_cloud_secret(TOKEN_SECRET)?;
                    let auk = crate::secrets::read_cloud_secret(AUK_SECRET)?
                        .and_then(|a| account::decode_auk(&a))
                        .filter(|auk| account::session(&account, auk).is_ok());
                    anyhow::Ok((token.is_some(), auk.is_some(), account))
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.cloud.busy = false;
                match found {
                    Ok((token, auk, account)) => {
                        this.cloud.phase = match (token, auk) {
                            (false, _) => CloudPhase::SignedOut,
                            (true, true) => CloudPhase::Ready,
                            (true, false) => CloudPhase::NeedsRecovery(account.clone()),
                        };
                        this.cloud.account = Some(account);
                        this.cloud.sync_requested = true;
                    }
                    Err(e) => {
                        log::warn!("cloud: keychain: {e:#}");
                        this.cloud.phase = CloudPhase::KeychainError(format!("{e:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn cloud_sign_in_browser(&mut self, cx: &mut Context<Self>) {
        if self.cloud.busy {
            return;
        }
        self.cloud.busy = true;
        self.cloud.form_error = None;
        let url = server_url(&self.settings.cloud);
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let started = cx
                .background_executor()
                .spawn(async move {
                    Client::new(Http::new(&url), None).device_start(&device_name(), platform())
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.cloud.busy = false;
                match started {
                    Ok(start) => {
                        let page = start
                            .verification_uri_complete
                            .clone()
                            .unwrap_or_else(|| start.verification_uri.clone());
                        cx.open_url(&page);
                        let now = Instant::now();
                        this.cloud.phase = CloudPhase::SigningIn {
                            next_poll: now + Duration::from_secs(start.interval.max(2)),
                            deadline: now + Duration::from_secs(start.expires_in.max(30)),
                            start,
                        };
                    }
                    Err(e) => this.cloud.form_error = Some(cloud_error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn cloud_open_sign_in_page(&self, cx: &mut Context<Self>) {
        if let CloudPhase::SigningIn { start, .. } = &self.cloud.phase {
            let page = start
                .verification_uri_complete
                .as_deref()
                .unwrap_or(&start.verification_uri);
            cx.open_url(page);
        }
    }

    pub(super) fn cloud_cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self.cloud.phase = CloudPhase::SignedOut;
        cx.notify();
    }

    fn cloud_poll_sign_in(&mut self, cx: &mut Context<Self>) {
        let CloudPhase::SigningIn { start, .. } = &self.cloud.phase else {
            return;
        };
        let code = start.device_code.clone();
        let interval = start.interval.max(2);
        let url = server_url(&self.settings.cloud);
        self.cloud.busy = true;
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let polled = cx
                .background_executor()
                .spawn(async move { Client::new(Http::new(&url), None).device_poll(&code) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.cloud.busy = false;
                let CloudPhase::SigningIn { next_poll, .. } = &mut this.cloud.phase else {
                    return; // cancelled meanwhile
                };
                match polled {
                    Ok(DevicePoll::Pending) => {
                        *next_poll = Instant::now() + Duration::from_secs(interval);
                    }
                    Ok(DevicePoll::SlowDown) => {
                        *next_poll = Instant::now() + Duration::from_secs(interval + 5);
                    }
                    Ok(DevicePoll::Approved { token, .. }) => this.cloud_use_token(token, cx),
                    Ok(DevicePoll::Denied) => {
                        this.cloud.phase = CloudPhase::SignedOut;
                        this.cloud.form_error =
                            Some(t("Sign-in was declined in the browser.").to_string());
                    }
                    Ok(DevicePoll::Expired) => {
                        this.cloud.phase = CloudPhase::SignedOut;
                        this.cloud.form_error =
                            Some(t("The sign-in code expired. Try again.").to_string());
                    }
                    Err(e) => {
                        // A blip: keep waiting, show why.
                        *next_poll = Instant::now() + Duration::from_secs(interval * 2);
                        this.cloud.form_error = Some(cloud_error_text(&e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sign in with an API key pasted into Settings.
    pub(super) fn cloud_sign_in_with_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self
            .settings_ui
            .cloud_api_key
            .read(cx)
            .value()
            .trim()
            .to_string();
        if !(key.starts_with("mxk_") || key.starts_with("mxd_")) {
            self.cloud.form_error =
                Some(t("That isn't a muxel Cloud API key (they start with mxk_).").to_string());
            cx.notify();
            return;
        }
        self.settings_ui
            .cloud_api_key
            .update(cx, |s, cx| s.set_value(String::new(), window, cx));
        self.cloud_use_token(key, cx);
    }

    /// Store `token` and set up or unlock the account.
    fn cloud_use_token(&mut self, token: String, cx: &mut Context<Self>) {
        self.cloud.phase = CloudPhase::Checking;
        self.cloud.busy = true;
        self.cloud.form_error = None;
        let url = server_url(&self.settings.cloud);
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { sign_in_with_token(&url, token) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.cloud.busy = false;
                match result {
                    Ok(SignedIn::Ready { account, created }) => {
                        this.cloud_signed_in(account, cx);
                        this.cloud.new_recovery = created;
                    }
                    Ok(SignedIn::Locked { account, created }) => {
                        this.cloud_save_account(&account);
                        this.cloud.account = Some(account.clone());
                        this.cloud.phase = CloudPhase::NeedsRecovery(account);
                        if let Some(rk) = created {
                            // The new account's AUK couldn't be stored: the
                            // Recovery Key is now the only way in. Show it.
                            this.cloud.new_recovery = Some(rk);
                            this.cloud.form_error = Some(
                                t("muxel couldn't store your keys in this computer's keychain. Save the Recovery Key below, then enter it to unlock.")
                                    .to_string(),
                            );
                        }
                    }
                    Err(e) => {
                        this.cloud.phase = CloudPhase::SignedOut;
                        this.cloud.form_error = Some(e);
                    }
                }
                this.cloud.bump();
                cx.notify();
            });
        })
        .detach();
    }

    fn cloud_save_account(&mut self, account: &AccountFile) {
        if let Some(dir) = &self.cloud.dir
            && let Err(e) = dir.save_account(account)
        {
            log::warn!("cloud: couldn't save the account file: {e}");
        }
    }

    fn cloud_signed_in(&mut self, account: AccountFile, cx: &mut Context<Self>) {
        // A different account than before: start its sync from scratch.
        if self
            .cloud
            .account
            .as_ref()
            .is_some_and(|a| a.user != account.user)
            && let Some(dir) = &self.cloud.dir
        {
            let _ = dir.save_state(&EngineState::default());
            self.cloud.state = None;
        }
        self.cloud_save_account(&account);
        self.cloud.account = Some(account);
        self.cloud.phase = CloudPhase::Ready;
        self.cloud.sync_requested = true;
        self.cloud.last_error = None;
        if !self.settings.cloud.enabled {
            self.settings.cloud.enabled = true;
            self.persist_settings();
        }
        cx.notify();
    }

    pub(super) fn cloud_unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let CloudPhase::NeedsRecovery(account) = &self.cloud.phase else {
            return;
        };
        let account = account.clone();
        let typed = self.settings_ui.cloud_recovery.read(cx).value().to_string();
        self.cloud.busy = true;
        self.cloud.form_error = None;
        cx.spawn_in(window, async move |view: WeakEntity<Self>, cx| {
            let acct = account.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let auk = account::recover(&acct, &typed).map_err(|e| match e {
                        muxel_cloud::crypto::CryptoError::BadRecoveryKey => {
                            t("That Recovery Key doesn't open this account.").to_string()
                        }
                        _ => t("That doesn't look like a Recovery Key (MXL-…).").to_string(),
                    })?;
                    crate::secrets::set_cloud_secret(AUK_SECRET, &account::encode_auk(&auk))
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.cloud.busy = false;
                match result {
                    Ok(()) => {
                        this.settings_ui
                            .cloud_recovery
                            .update(cx, |s, cx| s.set_value(String::new(), window, cx));
                        this.cloud_signed_in(account, cx);
                    }
                    Err(e) => this.cloud.form_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Forget the account on this device. Local settings and tasks stay. The
    /// keys go too: signing in again needs the Recovery Key.
    pub(super) fn cloud_sign_out(&mut self, cx: &mut Context<Self>) {
        let _ = crate::secrets::delete_cloud_secret(TOKEN_SECRET);
        let _ = crate::secrets::delete_cloud_secret(AUK_SECRET);
        if let Some(dir) = &self.cloud.dir {
            dir.clear();
            // Team tasks belong to the account; personal ones stay.
            let mut tasks = dir.load_tasks();
            tasks.teams.clear();
            let _ = dir.save_tasks(&tasks);
            self.cloud.tasks = tasks;
        }
        self.cloud.forget_session();
        self.cloud.phase = CloudPhase::SignedOut;
        self.settings.cloud.enabled = false;
        self.persist_settings();
        self.cloud.bump();
        cx.notify();
    }

    /// The server no longer accepts this device's token (revoked, expired),
    /// or another window dropped it. Keep the keys and the sync state:
    /// signing in again unlocks straight away, without the Recovery Key.
    fn cloud_lost_token(&mut self, revoked: bool, cx: &mut Context<Self>) {
        if revoked {
            let _ = crate::secrets::delete_cloud_secret(TOKEN_SECRET);
        }
        self.cloud.lock = None;
        self.cloud.phase = CloudPhase::SignedOut;
        self.cloud.form_error = Some(
            t("This computer was signed out of muxel Cloud. Sign in again to keep syncing.")
                .to_string(),
        );
        cx.notify();
    }

    pub(super) fn cloud_retry_keychain(&mut self, cx: &mut Context<Self>) {
        self.cloud.phase = CloudPhase::Unknown;
        cx.notify();
    }

    /// "I've saved it" under a shown Recovery Key.
    pub(super) fn cloud_recovery_saved(&mut self, cx: &mut Context<Self>) {
        self.cloud.new_recovery = None;
        if let Some(mut account) = self.cloud.account.clone() {
            account.recovery_unconfirmed = false;
            self.cloud_save_account(&account);
            self.cloud.account = Some(account);
        }
        cx.notify();
    }

    /// Replace the Recovery Key and show the new one.
    pub(super) fn cloud_replace_recovery(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.cloud.account.clone() else {
            return;
        };
        if self.cloud.busy {
            return;
        }
        self.cloud.busy = true;
        self.cloud.form_error = None;
        let url = server_url(&self.settings.cloud);
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let token = crate::secrets::read_cloud_secret(TOKEN_SECRET)
                        .map_err(|e| format!("{e:#}"))?
                        .ok_or_else(|| t("Not signed in.").to_string())?;
                    let auk = crate::secrets::read_cloud_secret(AUK_SECRET)
                        .map_err(|e| format!("{e:#}"))?
                        .and_then(|a| account::decode_auk(&a))
                        .ok_or_else(|| t("This computer isn't unlocked.").to_string())?;
                    let client = Client::new(Http::new(&url), Some(token));
                    let (rk, bundle) = account::new_recovery_key(&client, &account, &auk)
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>((
                        rk.display(),
                        AccountFile {
                            bundle,
                            recovery_unconfirmed: true,
                            ..account
                        },
                    ))
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.cloud.busy = false;
                match result {
                    Ok((rk, account)) => {
                        this.cloud_save_account(&account);
                        this.cloud.account = Some(account);
                        this.cloud.new_recovery = Some(rk);
                    }
                    Err(e) => this.cloud.form_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// After a cycle that stopped early: remove the outbox files of the shares
    /// the server answered, so they aren't sent again; the rest stay queued.
    fn cloud_settle_shares(&mut self, settled: &[usize]) {
        let (done, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.cloud.sent_ops)
            .into_iter()
            .partition(|(_, k)| matches!(k, SentOp::Share(i) if settled.contains(i)));
        if let Some(dir) = &self.cloud.dir {
            dir.remove_ops(&done.into_iter().map(|(f, _)| f).collect::<Vec<_>>());
        }
        self.cloud.sent_ops = keep;
    }

    pub(super) fn cloud_sync_now(&mut self, cx: &mut Context<Self>) {
        self.cloud.sync_requested = true;
        cx.notify();
    }

    pub(super) fn set_cloud_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.cloud.enabled = on;
        if !on {
            self.cloud.cancel_cycle();
            self.cloud.lock = None;
        } else {
            self.cloud.sync_requested = true;
        }
        self.persist_settings();
        cx.notify();
    }

    // --- the sync cycle -----------------------------------------------------

    fn cloud_start_cycle(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.cloud.account.clone() else {
            return;
        };
        // Start from what every window has saved.
        self.cloud_reload_from_disk(cx);
        if !self.settings.cloud.enabled {
            // Another window paused sync since this tick checked.
            self.cloud.lock = None;
            return;
        }
        let Some(dir) = &self.cloud.dir else {
            return;
        };
        let state = self.cloud.state.take().unwrap_or_else(|| dir.load_state());
        let first_pull = state.sync.cursor == 0;
        let presence = dir.collect_presence(crate::control::local_hostname(), now_ms());
        let queued = dir.peek();
        let shares: Vec<TeamSettingWrite> = queued
            .iter()
            .filter_map(|(_, op)| match op {
                Op::Share(w) => Some(w.clone()),
                Op::Task { .. } | Op::SyncNow | Op::Trust(_) => None,
            })
            .collect();
        let decisions: Vec<MemberDecision> = queued
            .iter()
            .filter_map(|(_, op)| match op {
                Op::Trust(d) => Some(d.clone()),
                Op::Task { .. } | Op::SyncNow | Op::Share(_) => None,
            })
            .collect();
        let mut share_ix = 0;
        self.cloud.sent_ops = queued
            .into_iter()
            .map(|(f, op)| {
                let kind = match op {
                    Op::Share(_) => {
                        share_ix += 1;
                        SentOp::Share(share_ix - 1)
                    }
                    Op::Trust(_) => SentOp::Trust,
                    Op::Task { .. } | Op::SyncNow => SentOp::Other,
                };
                (f, kind)
            })
            .collect();
        let settings = self.settings_base();
        let tasks = self.cloud.tasks.clone();
        let url = server_url(&self.settings.cloud);
        let cached_keys = self
            .cloud
            .project_keys
            .as_ref()
            .filter(|(at, _)| at.elapsed() < PROJECT_KEYS_TTL)
            .map(|(_, keys)| keys.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.cloud.cycle_cancel = Some(cancel.clone());
        self.cloud.busy = true;
        self.cloud.sync_requested = false;
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    let mut state = state;
                    let projects = cached_keys.unwrap_or_else(collect_project_keys);
                    let job = CycleJob {
                        url,
                        account,
                        settings,
                        shares,
                        decisions,
                        presence,
                    };
                    let out = run_cycle(&job, &tasks, &projects, &mut state, cancel.clone());
                    CycleDone {
                        out,
                        state,
                        projects,
                        tasks,
                        first_pull,
                        cancelled: cancel.load(Ordering::SeqCst),
                    }
                })
                .await;
            let _ = view.update(cx, |this, cx| this.cloud_finish_cycle(done, cx));
        })
        .detach();
    }

    fn cloud_finish_cycle(&mut self, done: CycleDone, cx: &mut Context<Self>) {
        self.cloud.busy = false;
        self.cloud.cycle_cancel = None;
        self.cloud.last_cycle = Some(Instant::now());
        let CycleDone {
            out,
            state,
            projects,
            tasks: tasks_snapshot,
            first_pull,
            cancelled,
        } = done;
        if cancelled {
            // Paused (or signed out) mid-cycle: requests after that were never
            // sent. Shares the server already answered are done; everything
            // else stays queued. Its state is dropped, like its results: it
            // may record pulls that were never applied, so the next cycle
            // starts from the state on disk.
            self.cloud_settle_shares(&state.shares_settled);
            cx.notify();
            return;
        }
        let keys_fresh = self
            .cloud
            .project_keys
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < PROJECT_KEYS_TTL);
        if !keys_fresh {
            self.cloud.project_keys = Some((Instant::now(), projects.clone()));
        }
        // A cycle that fails partway drops its state with its results: the
        // state may record pulls whose changes were never applied, and the
        // next cycle would push the older local copy over them. It starts
        // from the last saved state instead.
        let out = match out {
            Ok(out) => out,
            Err(CycleFail::Cycle(CycleError::Api(ApiError::Unauthorized))) => {
                self.cloud_lost_token(true, cx);
                return;
            }
            Err(CycleFail::NoToken) => {
                self.cloud.state = Some(state);
                self.cloud_lost_token(false, cx);
                return;
            }
            Err(e) => {
                let text = cycle_fail_text(&e);
                log::warn!("cloud: sync failed: {text}");
                self.cloud_settle_shares(&state.shares_settled);
                self.cloud.last_error = Some(text);
                cx.notify();
                return;
            }
        };
        let sent = std::mem::take(&mut self.cloud.sent_ops);
        if !self.settings.cloud.enabled || !self.cloud.ready() {
            // Signed out or paused while the cycle ran: drop its results,
            // and with them its state. Every share was tried; member decisions
            // live in that state, so they stay queued for the next cycle.
            let (keep, done): (Vec<_>, Vec<_>) =
                sent.into_iter().partition(|(_, k)| *k == SentOp::Trust);
            if let Some(dir) = &self.cloud.dir {
                dir.remove_ops(&done.into_iter().map(|(f, _)| f).collect::<Vec<_>>());
            }
            self.cloud.sent_ops = keep;
            return;
        }
        // Sent (a share the server refused is reported, not retried).
        if let Some(dir) = &self.cloud.dir {
            dir.remove_ops(&sent.into_iter().map(|(f, _)| f).collect::<Vec<_>>());
        }
        let mut live = self.settings_base();
        let before = muxel_core::cloud::items::Appearance::from_settings(&live);
        if engine::apply_deltas(&mut live, &out.settings_changes, &projects, &state) {
            let after = muxel_core::cloud::items::Appearance::from_settings(&live);
            self.cloud_adopt_fields(&live);
            self.persist_settings();
            if serde_json::to_value(&before).ok() != serde_json::to_value(&after).ok() {
                self.cloud_apply_appearance(cx);
            }
        }
        // Other windows may have edited tasks while the cycle ran.
        if let Some(dir) = &self.cloud.dir {
            self.cloud.tasks = dir.load_tasks();
        }
        let tasks_before = self.cloud.tasks.clone();
        let dir = self.cloud.dir.as_ref();
        let merged =
            self.cloud
                .tasks
                .merge_cycle(&tasks_snapshot, out.personal_tasks, out.team_tasks);
        let settled = self.cloud.tasks.settle_deletions(&out.deletions_done);
        if (merged || settled)
            && let Some(dir) = dir
        {
            let _ = dir.save_tasks(&self.cloud.tasks);
        }
        if let (Some(me), Some(dir)) = (&out.me, dir) {
            let _ = dir.save_me(me);
        }
        if let Some(dir) = dir {
            let _ = dir.save_teams(&out.teams);
            if let Err(e) = dir.save_state(&state) {
                log::warn!("cloud: couldn't save sync state: {e}");
            }
        }
        self.cloud.teams = out.teams;
        self.cloud.teams_gen += 1;
        self.cloud.state = Some(state);
        if out.me.is_some() {
            self.cloud.me = out.me;
        }
        for err in &out.errors {
            log::warn!("cloud: {err}");
        }
        for note in out.notes {
            self.add_event(NotifKind::Success, t("muxel Cloud").to_string(), note);
        }
        if !first_pull {
            self.alert_new_tasks(&tasks_before);
        }
        self.cloud.last_error = out.errors.first().cloned();
        self.cloud.last_ok = Some(chrono::Local::now());
        self.cloud.bump();
        cx.notify();
    }

    /// Chime and notify for tasks the last cycle brought in. Only the
    /// coordinator runs cycles, so a profile alerts once however many windows
    /// it has open.
    fn alert_new_tasks(&mut self, before: &TaskStore) {
        let titles: Vec<String> = self
            .cloud
            .tasks
            .arrivals(before)
            .into_iter()
            .map(|(_, task)| task.title.clone())
            .collect();
        if titles.is_empty() {
            return;
        }
        let chime = self.settings.task_sound;
        if chime {
            self.ring_alert(AlertSound::Task);
        }
        if self.settings.task_notify {
            let summary = match titles.len() {
                1 => t("New task").to_string(),
                n => tf("{count} new tasks", &[("count", &n.to_string())]),
            };
            let mut body: Vec<String> = titles.iter().take(3).cloned().collect();
            if titles.len() > 3 {
                body.push(tf(
                    "and {count} more",
                    &[("count", &(titles.len() - 3).to_string())],
                ));
            }
            notify_with(summary, body.join("\n"), None, chime);
        }
    }

    /// Settings → muxel Cloud → Export: pick a file, then decrypt everything
    /// the account can read into it, off the UI thread.
    fn cloud_export(&mut self, cx: &mut Context<Self>) {
        if self.cloud.exporting {
            return;
        }
        let Some(account) = self.cloud.account.clone() else {
            return;
        };
        let url = server_url(&self.settings.cloud);
        let dir = home_dir()
            .map(|h| {
                let downloads = h.join("Downloads");
                if downloads.is_dir() { downloads } else { h }
            })
            .unwrap_or_default();
        let name = format!(
            "muxel-cloud-export-{}.json",
            chrono::Local::now().format("%Y-%m-%d")
        );
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            if view
                .update(cx, |this, cx| {
                    this.cloud.exporting = true;
                    this.cloud.export_result = None;
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            let shown = path.display().to_string();
            let result = cx
                .background_executor()
                .spawn(async move { run_export(&url, &account, &path) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.cloud.exporting = false;
                this.cloud.export_result = Some(match result {
                    Ok((0, _)) => Err(t(
                        "muxel Cloud returned nothing to export. If your plan has ended, the server may not support exports yet.",
                    )
                    .to_string()),
                    Ok((items, 0)) => Ok(tf(
                        "Saved {count} items to {path}.",
                        &[("count", &items.to_string()), ("path", &shown)],
                    )),
                    Ok((items, skipped)) => Ok(tf(
                        "Saved {count} items to {path}. {skipped} couldn't be read on this computer and were left out.",
                        &[
                            ("count", &items.to_string()),
                            ("path", &shown),
                            ("skipped", &skipped.to_string()),
                        ],
                    )),
                    Err(e) => Err(e),
                });
                cx.notify();
            });
        })
        .detach();
    }

    // --- Settings → Cloud ---------------------------------------------------

    pub(super) fn render_settings_cloud(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let mut blocks = vec![Block::Note(t(
            "Sync your runners, loops, snippets, hosts and appearance between your computers, and share them and tasks with your team. Item contents are encrypted on this computer before they are sent, so muxel Cloud can't read them.",
        ))];
        blocks.push(Block::Custom(self.render_cloud_account(form, cx)));
        if self.cloud.ready() {
            blocks.push(Block::Group(Group::new(
                t("Your data"),
                vec![
                    Row::new(
                        t("Export everything"),
                        Button::new("cloud-export")
                            .outline()
                            .small()
                            .icon(IconName::ArrowDown)
                            .label(if self.cloud.exporting {
                                t("Exporting…")
                            } else {
                                t("Export my data…")
                            })
                            .disabled(self.cloud.exporting)
                            .on_click(cx.listener(|this, _, _w, cx| this.cloud_export(cx))),
                    )
                    .desc(match &self.cloud.export_result {
                        Some(Ok(done)) => SharedString::from(done.clone()),
                        Some(Err(why)) => tf("The export failed: {error}", &[("error", why)]).into(),
                        None => t(
                            "Saves everything muxel Cloud holds for you (synced settings, tasks, and your teams' shared items) as one JSON file, decrypted on this computer. Works even after a plan ends. The file isn't encrypted, so keep it somewhere safe.",
                        ),
                    })
                    .keywords("export download backup data takeout"),
                ],
            )));
        }
        if self.cloud.ready() {
            blocks.push(Block::Group(Group::new(
                t("Sync"),
                vec![
                    Row::narrow(
                        t("Sync this computer"),
                        form.switch(
                            "cloud-enabled",
                            self.settings.cloud.enabled,
                            true,
                            cx.listener(|this, on: &bool, _w, cx| this.set_cloud_enabled(*on, cx)),
                        ),
                    )
                    .desc(t("Off pauses syncing; nothing is deleted.")),
                    Row::new(t("Check every"), self.cloud_interval_stepper(form, cx)),
                ],
            )));
            blocks.push(Block::Group(Group::new(
                t("What to sync"),
                self.cloud_category_rows(form, cx),
            )));
            blocks.push(Block::Note(t(
                "Host passwords and SSH keys never leave this computer. Loops that arrive from another computer start switched off here.",
            )));
            if !self.cloud.teams.is_empty() {
                blocks.push(Block::Group(Group::new(
                    t("Teams"),
                    self.cloud
                        .teams
                        .iter()
                        .map(|team| {
                            let mut row = Row::new(
                                team.name.clone(),
                                div().text_sm().child(role_label(&team.role)),
                            );
                            if team.waiting_for_keys {
                                row = row.desc(if team.approvals.iter().any(|a| !a.declined) {
                                    t("Waiting for you to approve a team admin below.")
                                } else {
                                    t("Waiting for a team admin to approve you in muxel, which then shares the team's keys.")
                                });
                            }
                            row
                        })
                        .collect(),
                )));
                let approvals = self.cloud_approval_rows(cx);
                if !approvals.is_empty() {
                    blocks.push(Block::Group(Group::new(
                        t("Team members to approve"),
                        approvals,
                    )));
                    blocks.push(Block::Note(t(
                        "muxel shares a team's keys only with members you approve, and uses keys only from admins you approve. Before you approve someone, check that the fingerprint here matches the one they see under Settings → muxel Cloud.",
                    )));
                }
            }
            if !self.cloud.teams.is_empty() || self.workspace.cloud.is_some() {
                blocks.extend(self.cloud_workspace_blocks(form, cx));
            }
        }
        form.page(blocks).into_any_element()
    }

    // --- Approving team members ----------------------------------------------

    /// One row per member waiting for this user's decision, across teams.
    fn cloud_approval_rows(&self, cx: &mut Context<Self>) -> Vec<Row> {
        let mut rows = Vec::new();
        for team in &self.cloud.teams {
            let i_manage = team.grants().has(Permission::MembersManage);
            for a in &team.approvals {
                let key = format!("{}-{}", team.id.simple(), a.user_id.simple());
                let decide = |approve: bool| {
                    let (team, approval) = (team.id, a.clone());
                    cx.listener(move |this, _, _w, cx| {
                        this.cloud_decide_member(team, &approval, approve, cx)
                    })
                };
                let mut buttons = h_flex().gap_2().child(
                    Button::new(SharedString::from(format!("cloud-approve-{key}")))
                        .small()
                        .primary()
                        .label(t("Approve"))
                        .on_click(decide(true)),
                );
                if !a.declined {
                    buttons = buttons.child(
                        Button::new(SharedString::from(format!("cloud-decline-{key}")))
                            .small()
                            .ghost()
                            .label(t("Decline"))
                            .on_click(decide(false)),
                    );
                }
                let role = role_label(&a.role);
                let args = [
                    ("email", a.email.as_str()),
                    ("role", role.as_ref()),
                    ("team", team.name.as_str()),
                    ("fp", a.fingerprint.as_str()),
                ];
                let desc = if a.declined {
                    tf(
                        "{email}, {role} in {team}. You declined them, so they get no keys from this computer. Fingerprint: {fp}",
                        &args,
                    )
                } else if i_manage {
                    tf(
                        "{email}, {role} in {team}. Approving shares the team's keys with them. Fingerprint: {fp}",
                        &args,
                    )
                } else {
                    tf(
                        "{email}, {role} in {team}. They can hand out the team's keys; approving lets muxel use keys from them. Fingerprint: {fp}",
                        &args,
                    )
                };
                rows.push(
                    Row::new(a.name.clone(), buttons)
                        .desc(desc)
                        .warn(a.keys_changed.then(|| {
                            t("Their encryption key is not the one you saw before. Keys don't change in normal use: check with them before you approve.")
                        }))
                        .keywords("approve member fingerprint keys trust team"),
                );
            }
        }
        rows
    }

    /// Queue the user's answer about a team member for the next sync, and
    /// take the row out of the list straight away.
    fn cloud_decide_member(
        &mut self,
        team: Uuid,
        approval: &ApprovalView,
        approve: bool,
        cx: &mut Context<Self>,
    ) {
        let op = Op::Trust(MemberDecision {
            team,
            user: approval.user_id,
            keys: approval.keys.clone(),
            approve,
        });
        let queued = self
            .cloud
            .dir
            .as_ref()
            .map(|dir| dir.enqueue(&op).map_err(|e| e.to_string()));
        match queued {
            Some(Ok(())) => {
                if let Some(view) = self.cloud.teams.iter_mut().find(|t| t.id == team) {
                    if approve {
                        view.approvals.retain(|a| a.user_id != approval.user_id);
                    } else if let Some(a) = view
                        .approvals
                        .iter_mut()
                        .find(|a| a.user_id == approval.user_id)
                    {
                        a.declined = true;
                    }
                }
                self.cloud.request_sync();
            }
            Some(Err(why)) => self.add_event(
                NotifKind::Error,
                t("muxel Cloud").to_string(),
                tf("Couldn't save that: {error}", &[("error", &why)]),
            ),
            None => {}
        }
        cx.notify();
    }

    // --- Team-shared runners, snippets and loops -----------------------------

    /// Show the bound team's shared runners, snippets and loops as a drop-down
    /// section (a muxel Cloud library in `library_hub`), or hide them. They
    /// get a library's safety: loops arrive off, and running a runner or
    /// turning on a loop asks first, again whenever the team changes it.
    fn sync_team_library(&mut self, cx: &mut Context<Self>) {
        let want = self
            .bound_team()
            .filter(|t| t.grants().has(Permission::SettingsRead))
            .map(|t| {
                (
                    t.id,
                    t.name.clone(),
                    muxel_core::cloud::items::team_library(&t.items),
                )
            });
        if let Some(prev) = self.cloud.team_library.take()
            && want.as_ref().is_none_or(|w| w.0 != prev)
        {
            self.library_hub.set_cloud_library(prev, "", None);
        }
        if let Some((id, name, items)) = want {
            let effects = self.library_hub.set_cloud_library(id, &name, Some(items));
            for loop_name in &effects.switched_off {
                self.add_event(
                    NotifKind::Error,
                    crate::libraries::switched_off_text(loop_name),
                    name.clone(),
                );
            }
            if !effects.switched_off.is_empty() {
                self.persist_library_state();
            }
            self.cloud.team_library = Some(id);
        }
        cx.notify();
    }

    // --- Share with team -----------------------------------------------------

    /// The team this window's workspace works for, if this user may share
    /// settings with it.
    fn share_team(&self) -> Option<&TeamView> {
        self.bound_team()
            .filter(|t| t.grants().has(Permission::SettingsWrite))
    }

    /// Buttons for a personal runner, snippet or loop's editor: share it with
    /// the workspace's team, update the shared copy, or stop sharing it.
    pub(super) fn share_actions(
        &self,
        collection: Collection,
        item: Uuid,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(team) = self.share_team() else {
            return Vec::new();
        };
        let shared = team
            .items
            .get(collection.as_str())
            .is_some_and(|v| v.iter().any(|(id, _)| *id == item));
        let label = if shared {
            t("Update team copy")
        } else {
            tf("Share with {team}", &[("team", &team.name)]).into()
        };
        let mut out = vec![
            super::settings_kit::title_button("share-with-team", label)
                .on_click(cx.listener(move |this, _e, _w, cx| {
                    this.share_with_team(collection, item, true, cx)
                }))
                .into_any_element(),
        ];
        if shared {
            out.push(
                super::settings_kit::title_button("stop-sharing", t("Stop sharing"))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.share_with_team(collection, item, false, cx)
                    }))
                    .into_any_element(),
            );
        }
        out
    }

    /// Queue a share (or, with `share` false, its removal) for the window
    /// that syncs. The team's copy is a snapshot: later local edits stay local
    /// until shared again.
    fn share_with_team(
        &mut self,
        collection: Collection,
        item: Uuid,
        share: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(team) = self.share_team().map(|t| (t.id, t.name.clone())) else {
            return;
        };
        let data = if share {
            let settings = self.settings_base();
            let carry = HashMap::new();
            let resolver = muxel_core::cloud::items::Resolver {
                presets: &self.presets,
                projects: &[],
                carry: &carry,
            };
            match muxel_core::cloud::items::extract(&settings, &resolver, &[collection])
                .into_iter()
                .find(|l| l.id == item)
            {
                Some(local) => Some(local.data),
                None => return,
            }
        } else {
            None
        };
        let op = Op::Share(TeamSettingWrite {
            team: team.0,
            collection,
            id: item,
            data,
        });
        let queued = self
            .cloud
            .dir
            .as_ref()
            .map(|dir| dir.enqueue(&op).map_err(|e| e.to_string()));
        match queued {
            Some(Ok(())) => {
                self.cloud.request_sync();
                let text = if share {
                    tf(
                        "Shared with {team}. It reaches the team on the next sync.",
                        &[("team", &team.1)],
                    )
                } else {
                    tf("No longer shared with {team}.", &[("team", &team.1)])
                };
                self.add_event(NotifKind::Success, t("muxel Cloud").to_string(), text);
            }
            Some(Err(e)) => self.add_event(NotifKind::Error, t("muxel Cloud").to_string(), e),
            None => {}
        }
        cx.notify();
    }

    // --- Presence -------------------------------------------------------------

    /// Write (or remove) this window's presence file for the syncing window.
    fn publish_presence(&mut self, cx: &mut Context<Self>) {
        let presence = self.window_presence(cx);
        let Some(dir) = &self.cloud.dir else {
            return;
        };
        match presence {
            Some(p) => {
                if let Err(e) = dir.put_presence(&self.cloud.window_id, &[p]) {
                    log::warn!("cloud: couldn't write presence: {e}");
                }
            }
            None => dir.clear_presence(&self.cloud.window_id),
        }
    }

    /// What this window's agents are doing, for its team. Only when the
    /// workspace works for a team, sharing is on (it's opt-in), and the role
    /// may publish. Never prompts or output; task titles only of team tasks.
    fn window_presence(&self, cx: &App) -> Option<WindowPresence> {
        let binding = self.workspace.cloud.as_ref().filter(|b| b.share_activity)?;
        if !self.cloud.ready() || !self.settings.cloud.enabled {
            return None;
        }
        let team = self
            .cloud
            .teams
            .iter()
            .find(|t| t.id == binding.team_id)
            .filter(|t| t.grants().has(Permission::ActivityPublish))?;
        let projects =
            self.workspace
                .projects
                .iter()
                .filter(|p| p.cloud.shares_activity(Some(binding)))
                .map(|p| ProjectActivity {
                    project_key: self
                        .cloud
                        .project_key_cached(p.id)
                        .unwrap_or_else(|| format!("name:{}", p.name)),
                    project_name: p.name.clone(),
                    agents: self
                        .workspace
                        .instances
                        .iter()
                        .filter(|i| i.project_id == p.id && i.kind == InstanceKind::Terminal)
                        .map(|i| {
                            // An agent's program, or a shell's preset name.
                            let program = i.program.clone().unwrap_or_else(|| i.preset.clone());
                            let status = match self.terminals.get(&i.id) {
                                Some(v) => match v.read(cx).status() {
                                    muxel_terminal::AgentStatus::Working => "working",
                                    muxel_terminal::AgentStatus::Blocked => "blocked",
                                    muxel_terminal::AgentStatus::Done => "done",
                                    muxel_terminal::AgentStatus::Idle => "idle",
                                },
                                None => "exited",
                            };
                            let task = self
                                .workspace
                                .task_links
                                .get(&i.id)
                                .and_then(|id| self.cloud.tasks.find(*id))
                                .filter(|(home, _)| *home == TaskHome::Team(team.id));
                            AgentActivity {
                                program,
                                status: status.to_string(),
                                branch: i.worktree_branch.clone().or_else(|| {
                                    self.project_branches.get(&p.id).cloned().flatten()
                                }),
                                task_id: task.map(|(_, t)| t.id),
                                task_title: task.map(|(_, t)| t.title.clone()),
                                prompt: None,
                            }
                        })
                        .collect(),
                })
                .collect();
        let presence = Presence {
            at_ms: now_ms(),
            device_name: String::new(),
            projects,
        };
        Some(WindowPresence {
            team: team.id,
            presence: activity::filter(presence, &self.settings.cloud.privacy),
        })
    }

    // --- Workspace binding --------------------------------------------------

    /// Make this window's workspace work for a team, or keep it private
    /// (`None`). Choosing the team it already works for keeps its switches.
    fn set_workspace_team(&mut self, team: Option<(Uuid, String)>, cx: &mut Context<Self>) {
        self.workspace.cloud = team.map(|(id, name)| match self.workspace.cloud.take() {
            Some(mut same) if same.team_id == id => {
                same.team_name = name;
                same
            }
            _ => WorkspaceCloud {
                team_id: id,
                team_name: name,
                share_activity: false,
                tasks_enabled: true,
            },
        });
        self.cloud.request_sync();
        self.persist();
        cx.notify();
    }

    fn edit_workspace_binding(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut WorkspaceCloud),
    ) {
        if let Some(binding) = self.workspace.cloud.as_mut() {
            edit(binding);
            self.cloud.request_sync();
            self.persist();
            cx.notify();
        }
    }

    fn set_project_activity(&mut self, project: Uuid, value: Tristate, cx: &mut Context<Self>) {
        if let Some(p) = self.workspace.projects.iter_mut().find(|p| p.id == project) {
            p.cloud.share_activity = value;
            self.persist();
            cx.notify();
        }
    }

    fn cloud_workspace_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        let binding = self.workspace.cloud.clone();
        let bound = binding.as_ref().map(|b| b.team_id);
        let mut segs = vec![Seg::new(
            t("Private"),
            bound.is_none(),
            cx.listener(|this, _, _w, cx| this.set_workspace_team(None, cx)),
        )];
        for team in &self.cloud.teams {
            let (id, name) = (team.id, team.name.clone());
            segs.push(Seg::new(
                name.clone(),
                bound == Some(id),
                cx.listener(move |this, _, _w, cx| {
                    this.set_workspace_team(Some((id, name.clone())), cx)
                }),
            ));
        }
        // Bound to a team this account no longer belongs to: still shown, so
        // it's clear why nothing of the team appears.
        if let Some(b) = binding
            .as_ref()
            .filter(|b| !self.cloud.teams.iter().any(|t| t.id == b.team_id))
        {
            segs.push(Seg::new(
                format!("{} ({})", b.team_name, t("not a member")),
                true,
                |_, _, _| {},
            ));
        }
        let mut rows = vec![
            form.choice_row(t("Works for"), "cloud-binding", segs).desc(t(
                "A team's shared runners, snippets, loops and tasks appear only in workspaces that work for that team. Your own settings sync in every workspace.",
            )),
        ];
        if let Some(b) = &binding {
            rows.push(
                Row::narrow(
                    t("Team tasks"),
                    form.switch(
                        "cloud-ws-tasks",
                        b.tasks_enabled,
                        true,
                        cx.listener(|this, on: &bool, _w, cx| {
                            let on = *on;
                            this.edit_workspace_binding(cx, |b| b.tasks_enabled = on);
                        }),
                    ),
                )
                .desc(t("Show the team's tasks in this workspace's Tasks panel.")),
            );
            rows.push(
                Row::narrow(
                    t("Share what I'm working on"),
                    form.switch(
                        "cloud-ws-activity",
                        b.share_activity,
                        true,
                        cx.listener(|this, on: &bool, _w, cx| {
                            let on = *on;
                            this.edit_workspace_binding(cx, |b| b.share_activity = on);
                        }),
                    ),
                )
                .desc(t(
                    "Teammates see each agent's project, branch, status and task. Prompts and terminal output stay on this computer.",
                )),
            );
        }
        let mut blocks = vec![Block::Group(Group::new(t("This workspace"), rows))];
        if binding.as_ref().is_some_and(|b| b.share_activity) && !self.workspace.projects.is_empty()
        {
            let rows = self
                .workspace
                .projects
                .iter()
                .map(|p| {
                    let (pid, current) = (p.id, p.cloud.share_activity);
                    let option = |label: SharedString, value: Tristate| {
                        Seg::new(
                            label,
                            current == value,
                            cx.listener(move |this, _, _w, cx| {
                                this.set_project_activity(pid, value, cx)
                            }),
                        )
                    };
                    form.choice_row(
                        p.name.clone(),
                        &format!("cloud-project-{pid}"),
                        vec![
                            option(t("Default"), Tristate::Inherit),
                            option(t("Share"), Tristate::On),
                            option(t("Don't share"), Tristate::Off),
                        ],
                    )
                })
                .collect();
            blocks.push(Block::Group(Group::new(t("Activity by project"), rows)));
        }
        blocks
    }

    fn render_cloud_account(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, danger, success) = (theme.muted_foreground, theme.danger, theme.success);
        let mono = theme.mono_font_family.clone();
        let busy = self.cloud.busy;
        let hint = |s: SharedString| div().text_xs().text_color(muted).child(s);
        let mut card = form.card().p_3().gap_3();
        match &self.cloud.phase {
            CloudPhase::Unknown | CloudPhase::Checking => {
                card = card.child(hint(t("Checking your muxel Cloud account…")));
            }
            CloudPhase::SignedOut => {
                card = card
                    .child(div().text_sm().child(t(
                        "Sign in with your muxel Cloud account. Your browser opens to confirm this computer.",
                    )))
                    .child(
                        h_flex().gap_2().child(
                            Button::new("cloud-sign-in")
                                .primary()
                                .small()
                                .label(t("Sign in with browser"))
                                .loading(busy)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _w, cx| {
                                    this.cloud_sign_in_browser(cx)
                                })),
                        ),
                    )
                    .child(hint(t("Or paste an API key from the muxel Cloud panel:")))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(form.input_w(&self.settings_ui.cloud_api_key, px(260.0)))
                            .child(
                                Button::new("cloud-use-key")
                                    .small()
                                    .label(t("Use key"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cloud_sign_in_with_key(window, cx)
                                    })),
                            ),
                    )
                    .child(hint(
                        tf(
                            "Server: {url}",
                            &[("url", &server_url(&self.settings.cloud))],
                        )
                        .into(),
                    ));
            }
            CloudPhase::SigningIn { start, .. } => {
                card = card
                    .child(div().text_sm().child(t(
                        "Confirm this computer in your browser. Check that the page shows this code:",
                    )))
                    .child(
                        div()
                            .text_xl()
                            .font_family(mono.clone())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(start.user_code.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("cloud-open-page")
                                    .small()
                                    .label(t("Open the page again"))
                                    .on_click(cx.listener(|this, _, _w, cx| {
                                        this.cloud_open_sign_in_page(cx)
                                    })),
                            )
                            .child(
                                Button::new("cloud-cancel-sign-in")
                                    .small()
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(cx.listener(|this, _, _w, cx| {
                                        this.cloud_cancel_sign_in(cx)
                                    })),
                            ),
                    );
            }
            CloudPhase::NeedsRecovery(account) => {
                card = card
                    .child(div().text_sm().child(tf(
                        "Signed in as {email}. Enter your Recovery Key to unlock your data on this computer.",
                        &[("email", &account.email)],
                    )))
                    .child(hint(t(
                        "You saved it when you first set up muxel Cloud. It starts with MXL-.",
                    )))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(form.input_w(&self.settings_ui.cloud_recovery, px(320.0)))
                            .child(
                                Button::new("cloud-unlock")
                                    .primary()
                                    .small()
                                    .label(t("Unlock"))
                                    .loading(busy)
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cloud_unlock(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div().child(
                            Button::new("cloud-sign-out-locked")
                                .small()
                                .ghost()
                                .label(t("Sign out"))
                                .on_click(cx.listener(|this, _, _w, cx| this.cloud_sign_out(cx))),
                        ),
                    );
            }
            CloudPhase::Ready => {
                let email = self
                    .cloud
                    .account
                    .as_ref()
                    .map(|a| a.email.clone())
                    .unwrap_or_default();
                let plan = self.cloud.me.as_ref().map(|me| {
                    let e = &me.entitlements;
                    if e.read_only {
                        t("Payment overdue: syncing is read-only until it's paid.")
                    } else if e.personal_sync {
                        t("Sync is included in your plan.")
                    } else {
                        t("No plan includes sync yet. Choose one in the muxel Cloud panel.")
                    }
                });
                let (dot, status): (Hsla, SharedString) = if !self.settings.cloud.enabled {
                    (muted, t("Paused."))
                } else if let Some(err) = &self.cloud.last_error {
                    (
                        danger,
                        tf("Last sync failed: {error}", &[("error", err)]).into(),
                    )
                } else if !self.cloud.coordinating() {
                    (
                        muted,
                        t("Another muxel window on this computer is syncing."),
                    )
                } else if busy && self.cloud.last_ok.is_none() {
                    (muted, t("Syncing…"))
                } else if let Some(at) = self.cloud.last_ok {
                    (
                        success,
                        tf(
                            "Synced at {time}.",
                            &[("time", &at.format("%H:%M").to_string())],
                        )
                        .into(),
                    )
                } else {
                    (muted, t("Waiting to sync…"))
                };
                let fingerprint = self
                    .cloud
                    .account
                    .as_ref()
                    .and_then(|a| {
                        let e = muxel_cloud::crypto::pub32(&a.bundle.enc_pub, "enc").ok()?;
                        let s = muxel_cloud::crypto::pub32(&a.bundle.sign_pub, "sign").ok()?;
                        Some(muxel_cloud::crypto::fingerprint(&e, &s))
                    })
                    .unwrap_or_default();
                card = card
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tf("Signed in as {email}", &[("email", &email)])),
                    )
                    .children(plan.map(hint))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(8.0)).flex_none().rounded_full().bg(dot))
                            .child(div().text_xs().text_color(muted).child(status)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tf("Your key fingerprint: {fp}", &[("fp", &fingerprint)])),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("cloud-sync-now")
                                    .small()
                                    .label(t("Sync now"))
                                    .disabled(!self.settings.cloud.enabled)
                                    .on_click(
                                        cx.listener(|this, _, _w, cx| this.cloud_sync_now(cx)),
                                    ),
                            )
                            .child(
                                Button::new("cloud-panel")
                                    .small()
                                    .ghost()
                                    .label(t("Open the muxel Cloud panel"))
                                    .on_click(cx.listener(|this, _, _w, cx| {
                                        cx.open_url(&server_url(&this.settings.cloud))
                                    })),
                            )
                            .child(
                                Button::new("cloud-sign-out")
                                    .small()
                                    .ghost()
                                    .label(t("Sign out"))
                                    .on_click(
                                        cx.listener(|this, _, _w, cx| this.cloud_sign_out(cx)),
                                    ),
                            ),
                    );
                let unconfirmed = self
                    .cloud
                    .account
                    .as_ref()
                    .is_some_and(|a| a.recovery_unconfirmed);
                if unconfirmed && self.cloud.new_recovery.is_none() {
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .p_3()
                            .rounded(form.radius)
                            .border_1()
                            .border_color(theme.warning)
                            .child(div().text_xs().child(t(
                                "You haven't confirmed that you saved your Recovery Key. Without it, losing this computer's keychain loses your synced data. Make a new one and save it.",
                            )))
                            .child(
                                h_flex().child(
                                    Button::new("cloud-new-recovery")
                                        .small()
                                        .primary()
                                        .label(t("Make a new Recovery Key"))
                                        .loading(busy)
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, _w, cx| {
                                            this.cloud_replace_recovery(cx)
                                        })),
                                ),
                            ),
                    );
                } else if self.cloud.new_recovery.is_none() {
                    card = card.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(hint(t(
                                "Lost your Recovery Key? Replace it; the old one stops working.",
                            )))
                            .child(
                                Button::new("cloud-replace-recovery")
                                    .xsmall()
                                    .ghost()
                                    .label(t("Replace Recovery Key"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _w, cx| {
                                        this.cloud_replace_recovery(cx)
                                    })),
                            ),
                    );
                }
                card = card.child(hint(t(
                    "Signing out removes this computer's keys. To sign in again you'll need your Recovery Key.",
                )));
            }
            CloudPhase::KeychainError(err) => {
                card = card
                    .child(div().text_sm().child(t(
                        "muxel can't read this computer's keychain, where it keeps your muxel Cloud sign-in. If your keychain is locked, unlock it, then retry.",
                    )))
                    .child(hint(err.clone().into()))
                    .child(
                        div().child(
                            Button::new("cloud-keychain-retry")
                                .small()
                                .label(t("Retry"))
                                .on_click(cx.listener(|this, _, _w, cx| {
                                    this.cloud_retry_keychain(cx)
                                })),
                        ),
                    );
            }
        }
        if let Some(rk) = &self.cloud.new_recovery {
            card = card.child(self.render_recovery_key(rk, form, cx));
        }
        if let Some(err) = &self.cloud.form_error {
            card = card.child(div().text_xs().text_color(danger).child(err.clone()));
        }
        card.into_any_element()
    }

    /// A just-made Recovery Key, shown until the user says it's saved.
    fn render_recovery_key(&self, rk: &str, form: &Form, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let rk_copy = rk.to_string();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(form.radius)
            .border_1()
            .border_color(theme.warning)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t("Save your Recovery Key")),
            )
            .child(div().text_xs().child(t(
                "You need it to unlock your data on another computer, or on this one if you lose its keychain. muxel Cloud can't recover it for you: without it, your synced data is lost. Keep it in a password manager.",
            )))
            .child(div().text_lg().font_family(mono).child(rk.to_string()))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("cloud-copy-recovery")
                            .small()
                            .label(t("Copy"))
                            .on_click(move |_, _w, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(rk_copy.clone()))
                            }),
                    )
                    .child(
                        Button::new("cloud-saved-recovery")
                            .small()
                            .primary()
                            .label(t("I've saved it"))
                            .on_click(
                                cx.listener(|this, _, _w, cx| this.cloud_recovery_saved(cx)),
                            ),
                    ),
            )
    }

    fn cloud_interval_stepper(&self, form: &Form, cx: &mut Context<Self>) -> Div {
        const STEPS: [u32; 7] = [15, 30, 60, 120, 300, 600, 1800];
        let now = self.settings.cloud.interval();
        let ix = STEPS
            .iter()
            .position(|&s| s >= now)
            .unwrap_or(STEPS.len() - 1);
        let set = |secs: u32| -> settings_kit::Click {
            Box::new(cx.listener(move |this, _, _w, cx| {
                this.settings.cloud.interval_secs = secs;
                this.persist_settings();
                cx.notify();
            }))
        };
        let label = if now < 60 {
            tf("{n} s", &[("n", &now.to_string())])
        } else {
            tf("{n} min", &[("n", &(now / 60).to_string())])
        };
        form.stepper(
            "cloud-interval",
            label,
            (ix > 0).then(|| set(STEPS[ix - 1])),
            (ix + 1 < STEPS.len()).then(|| set(STEPS[ix + 1])),
        )
    }

    fn cloud_category_rows(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Row> {
        type Field = fn(&mut muxel_core::cloud::SyncCategories) -> &mut SyncMode;
        let cats: [(&'static str, SharedString, Field); 7] = [
            ("cloud-runners", t("Runners"), |c| &mut c.runners),
            ("cloud-loops", t("Loops"), |c| &mut c.loops),
            ("cloud-snippets", t("Snippets"), |c| &mut c.snippets),
            ("cloud-hosts", t("Hosts and identities"), |c| &mut c.hosts),
            ("cloud-appearance", t("Appearance"), |c| &mut c.appearance),
            ("cloud-keys", t("Keybindings"), |c| &mut c.keybindings),
            ("cloud-tasks", t("Tasks"), |c| &mut c.tasks),
        ];
        let modes = [
            (SyncMode::Off, t("Off")),
            (SyncMode::Push, t("Upload")),
            (SyncMode::Pull, t("Download")),
            (SyncMode::TwoWay, t("Both")),
        ];
        cats.into_iter()
            .map(|(id, label, field)| {
                let mut cur = self.settings.cloud.categories;
                let current = *field(&mut cur);
                form.choice_row(
                    label,
                    id,
                    modes
                        .iter()
                        .map(|(mode, name)| {
                            let mode = *mode;
                            Seg::new(
                                name.clone(),
                                current == mode,
                                cx.listener(move |this, _, _w, cx| {
                                    *field(&mut this.settings.cloud.categories) = mode;
                                    this.cloud.sync_requested = true;
                                    this.persist_settings();
                                    cx.notify();
                                }),
                            )
                        })
                        .collect(),
                )
            })
            .collect()
    }
}

/// What a cycle sends, gathered on the UI thread.
struct CycleJob {
    url: String,
    account: AccountFile,
    settings: muxel_core::Settings,
    shares: Vec<TeamSettingWrite>,
    decisions: Vec<MemberDecision>,
    presence: Vec<(Uuid, Presence)>,
}

/// One cycle, off the UI thread.
fn run_cycle(
    job: &CycleJob,
    tasks: &TaskStore,
    projects: &[(Uuid, String)],
    state: &mut EngineState,
    cancel: Arc<AtomicBool>,
) -> Result<Output, CycleFail> {
    let CycleJob {
        url,
        account,
        settings,
        shares,
        decisions,
        presence,
    } = job;
    let (client, session) = open_session(url, account, Some(cancel))?;
    let engine = Engine {
        client: &client,
        session: &session,
    };
    let input = Input {
        cloud: &settings.cloud,
        settings,
        projects,
        personal_tasks: &tasks.personal,
        team_tasks: &tasks.teams,
        team_setting_writes: shares,
        member_decisions: decisions,
        presence,
        task_deletions: &tasks.deleted,
        now_ms: now_ms(),
    };
    Ok(engine.cycle(&input, state)?)
}

/// The signed-in client and unlocked keys, from the keychain. `cancel` stops
/// the client's requests once set.
fn open_session(
    url: &str,
    account: &AccountFile,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(Client<Http>, Session), CycleFail> {
    let keychain = |e: anyhow::Error| CycleFail::Keychain(format!("{e:#}"));
    let token = crate::secrets::read_cloud_secret(TOKEN_SECRET)
        .map_err(keychain)?
        .ok_or(CycleFail::NoToken)?;
    let auk = crate::secrets::read_cloud_secret(AUK_SECRET)
        .map_err(keychain)?
        .and_then(|a| account::decode_auk(&a))
        .ok_or_else(|| CycleFail::Keychain("the account key is missing".into()))?;
    let session = account::session(account, &auk).map_err(CycleError::from)?;
    let http = match cancel {
        Some(flag) => Http::new(url).with_cancel(flag),
        None => Http::new(url),
    };
    Ok((Client::new(http, Some(token)), session))
}

/// Decrypt everything the account can read and write it to `path` as JSON.
/// Returns (items written, items skipped).
fn run_export(url: &str, account: &AccountFile, path: &Path) -> Result<(usize, usize), String> {
    let (client, session) = open_session(url, account, None).map_err(|e| cycle_fail_text(&e))?;
    let engine = Engine {
        client: &client,
        session: &session,
    };
    let export = engine
        .export(now_ms())
        .map_err(|e| cycle_fail_text(&CycleFail::Cycle(e)))?;
    let text = serde_json::to_vec_pretty(&export.json).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok((export.items, export.skipped))
}

fn cycle_fail_text(e: &CycleFail) -> String {
    match e {
        CycleFail::Cycle(CycleError::Api(api)) => cloud_error_text(api),
        CycleFail::Cycle(CycleError::Crypto(c)) => c.to_string(),
        CycleFail::Keychain(k) => tf(
            "Can't read this computer's keychain: {error}",
            &[("error", k)],
        ),
        CycleFail::NoToken => t("Not signed in.").to_string(),
    }
}

/// What signing in with a token led to.
enum SignedIn {
    Ready {
        account: AccountFile,
        created: Option<String>,
    },
    /// Needs the Recovery Key. `created`: a new account whose AUK couldn't be
    /// stored, so its Recovery Key must be shown now.
    Locked {
        account: AccountFile,
        created: Option<String>,
    },
}

/// Off the UI thread: check the keychain works, then set up or unlock the
/// account, storing the token and the AUK.
fn sign_in_with_token(url: &str, token: String) -> Result<SignedIn, String> {
    crate::secrets::probe_cloud_keychain().map_err(|e| {
        tf(
            "muxel can't use this computer's keychain, so it can't keep you signed in: {error}",
            &[("error", &format!("{e:#}"))],
        )
    })?;
    let client = Client::new(Http::new(url), Some(token.clone()));
    let auk = crate::secrets::read_cloud_secret(AUK_SECRET)
        .ok()
        .flatten()
        .and_then(|a| account::decode_auk(&a));
    let setup = account::setup(&client, auk).map_err(|e| match e {
        CycleError::Api(api) => cloud_error_text(&api),
        other => other.to_string(),
    })?;
    crate::secrets::set_cloud_secret(TOKEN_SECRET, &token).map_err(|e| format!("{e:#}"))?;
    Ok(match setup {
        Setup::Ready {
            account,
            auk,
            created,
        } => {
            let created = created.map(|rk| rk.display());
            match crate::secrets::set_cloud_secret(AUK_SECRET, &account::encode_auk(&auk)) {
                Ok(()) => SignedIn::Ready { account, created },
                Err(e) => {
                    log::warn!("cloud: couldn't store the AUK: {e:#}");
                    SignedIn::Locked { account, created }
                }
            }
        }
        Setup::NeedsRecovery { account } => SignedIn::Locked {
            account,
            created: None,
        },
    })
}

fn cloud_error_text(e: &ApiError) -> String {
    match e {
        ApiError::Network(_) => t("Can't reach muxel Cloud. Check your connection.").to_string(),
        ApiError::Unauthorized => t("Not signed in.").to_string(),
        ApiError::Forbidden => t("Your role doesn't allow that.").to_string(),
        ApiError::PaymentRequired => {
            t("Your plan doesn't include sync. Choose one in the muxel Cloud panel.").to_string()
        }
        ApiError::ReadOnly => {
            t("Payment overdue: syncing is read-only until it's paid.").to_string()
        }
        other => other.to_string(),
    }
}

fn role_label(role: &str) -> SharedString {
    match role {
        "owner" => t("Owner"),
        "admin" => t("Admin"),
        "manager" => t("Manager"),
        "member" => t("Member"),
        "assignee" => t("Assignee"),
        "settings_only" => t("Settings sync only"),
        "observer" => t("Observer"),
        "billing" => t("Billing"),
        other => SharedString::from(other.to_string()),
    }
}
