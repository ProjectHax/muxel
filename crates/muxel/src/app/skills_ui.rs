//! Settings → Skills: every agent's skills on this computer or an SSH host, for
//! the user or one project — install from a git repository or the recommended
//! list, choose which agents get each skill, update and remove.
//!
//! The page's state lives in [`SkillsPage`] on the app (not in the Settings
//! window), so it survives Settings moving between windows. All file and git
//! work runs on a thread through [`crate::skills`].

use std::collections::{BTreeMap, BTreeSet};

use muxel_core::skills::agents::Protection;
use muxel_core::skills::curated::{CATALOG, CuratedCategory, CuratedSkill};
use muxel_core::skills::inventory::SkillRow;
use muxel_core::skills::placement::LocKind;
use muxel_core::skills::{Scope, SkillAgent};

use super::settings_kit::{Form, Group, ListItem, Row, add_button, delete_button, title_button};
use super::*;
use crate::skills::{
    self as sk, ChangeReport, Fetched, Machine, Snapshot, UpdateState, fetch_error_text,
};

const SKILL_ICON: &str = "icons/sparkles.svg";

/// What the Skills page shows and is doing.
#[derive(Default)]
pub(super) struct SkillsPage {
    /// `None` = this computer, else a remote host's id.
    machine: Option<Uuid>,
    /// `None` = user skills, else a project's.
    project: Option<Uuid>,
    snapshot: Option<Snapshot>,
    scanning: bool,
    scan_error: Option<String>,
    /// Bumped when the machine or scope changes; results for an older one are
    /// dropped.
    generation: u64,
    /// The skill whose details are shown (by folder name); `None` = Add.
    selected: Option<String>,
    fetching: bool,
    fetched: Option<Arc<Fetched>>,
    fetch_error: Option<String>,
    /// Skills of `fetched` ticked for install.
    picks: BTreeSet<usize>,
    /// Agents an install targets; `None` = every detected agent.
    targets: Option<BTreeSet<SkillAgent>>,
    /// Skills (by folder name) or curated ids (`curated:<id>`) being changed.
    busy: BTreeSet<String>,
    /// The last change's messages: text, is it an error.
    notices: Vec<(String, bool)>,
    updates: BTreeMap<String, UpdateState>,
    checking: bool,
    loaded: bool,
    /// List the read-only skills (synced, or bundled with an agent) too.
    show_bundled: bool,
}

/// A finished skills job.
enum SkillsOutcome {
    Scanned(Result<Snapshot, String>),
    Fetched(Result<Fetched, String>),
    Changed {
        keys: Vec<String>,
        report: ChangeReport,
        snapshot: Result<Snapshot, String>,
    },
    Updates(BTreeMap<String, UpdateState>),
}

/// Drop a fetched clone off the UI thread (removing it touches the disk).
fn release(fetched: Option<Arc<Fetched>>) {
    if let Some(f) = fetched {
        std::thread::spawn(move || drop(f));
    }
}

fn skills_work_dir() -> Option<PathBuf> {
    muxel_store::data_dir().map(|d| d.join("skill-fetch"))
}

/// Connect, rescan, run `op`, rescan — the shape of every change.
fn change_job(
    machine: Machine,
    scope: Scope,
    keys: Vec<String>,
    op: impl FnOnce(&sk::Host, &Snapshot) -> ChangeReport,
) -> SkillsOutcome {
    let failed = |e: String, keys: &[String]| ChangeReport {
        failed: keys.iter().map(|k| (k.clone(), e.clone())).collect(),
        ..Default::default()
    };
    let host = match sk::connect(machine) {
        Ok(h) => h,
        Err(e) => {
            return SkillsOutcome::Changed {
                report: failed(e.clone(), &keys),
                keys,
                snapshot: Err(e),
            };
        }
    };
    let report = match sk::scan(&host, &scope) {
        Ok(snap) => op(&host, &snap),
        Err(e) => failed(e, &keys),
    };
    SkillsOutcome::Changed {
        keys,
        report,
        snapshot: sk::scan(&host, &scope),
    }
}

/// The agent's icon for the page.
fn agent_icon(agent: SkillAgent) -> SharedString {
    agent_icon_path(Some(agent.program()))
}

fn protection_text(p: Protection) -> String {
    match p {
        Protection::Synced => t("Synced from your claude.ai account. Manage it there.").to_string(),
        Protection::Bundled(a) => tf(
            "Comes with {agent}, which manages it.",
            &[("agent", a.label())],
        ),
    }
}

fn curated_text(c: &CuratedSkill) -> (SharedString, SharedString) {
    match c.id {
        "security-audit" => (
            t("Security audit"),
            t(
                "Cloudflare's security review: hunts vulnerabilities across web, cloud, AI and supply-chain code and validates each finding.",
            ),
        ),
        "differential-review" => (
            t("Differential review"),
            t(
                "Trail of Bits' security-focused review of a diff or PR, with git history and blast-radius analysis.",
            ),
        ),
        "supply-chain-risk-auditor" => (
            t("Supply-chain risk auditor"),
            t("Trail of Bits' review of a project's dependencies for supply-chain risk."),
        ),
        "semgrep-rule-creator" => (
            t("Semgrep rule creator"),
            t("Trail of Bits' guide to writing and testing custom Semgrep rules."),
        ),
        "superpowers" => (
            t("Superpowers (full set)"),
            t(
                "Jesse Vincent's development workflow: brainstorm, plan, test-first, debug, review and finish branches.",
            ),
        ),
        "systematic-debugging" => (
            t("Systematic debugging"),
            t("Find the root cause before fixing: a four-phase debugging method."),
        ),
        "test-driven-development" => (
            t("Test-driven development"),
            t("Red, green, refactor: write the failing test first."),
        ),
        "brainstorming" => (
            t("Brainstorming"),
            t("Turn a rough idea into a design through questions before any code."),
        ),
        "skill-creator" => (
            t("Skill creator"),
            t("Anthropic's guide to writing, testing and improving your own skills."),
        ),
        "mcp-builder" => (
            t("MCP builder"),
            t("Anthropic's guide to building high-quality MCP servers."),
        ),
        "frontend-design" => (
            t("Frontend design"),
            t("Distinctive, production-grade web interfaces instead of generic ones."),
        ),
        "webapp-testing" => (
            t("Web app testing"),
            t("Test local web apps with Playwright: screenshots, logs and interaction."),
        ),
        "pdf" => (
            t("PDF"),
            t("Read, create, merge, split and fill PDF forms."),
        ),
        "docx" => (
            t("Word documents"),
            t("Create and edit .docx files, with tracked changes."),
        ),
        "xlsx" => (
            t("Spreadsheets"),
            t("Create and edit .xlsx files with formulas and charts."),
        ),
        "pptx" => (t("Presentations"), t("Create and edit .pptx slide decks.")),
        other => (
            SharedString::from(other.to_string()),
            SharedString::default(),
        ),
    }
}

fn category_title(c: CuratedCategory) -> SharedString {
    match c {
        CuratedCategory::Security => t("Security"),
        CuratedCategory::Workflow => t("Workflow"),
        CuratedCategory::Building => t("Building"),
        CuratedCategory::Documents => t("Documents"),
    }
}

impl MuxelApp {
    // --- Where: machine and scope ---

    fn skills_machine(&self) -> Option<Machine> {
        let Some(id) = self.skills_page.machine else {
            return Some(Machine::Local);
        };
        let host = self
            .remotes
            .iter()
            .find(|h| h.id == id)?
            .effective(&self.identities);
        let password = (host.auth == SshAuth::Password)
            .then(|| self.remote_password(&host))
            .flatten();
        Some(Machine::Remote(Box::new(integrations::RemoteConn {
            host,
            remote_path: String::new(),
            control_path: Self::commands_control_path(id),
            password,
        })))
    }

    fn skills_scope(&self) -> Option<Scope> {
        let Some(pid) = self.skills_page.project else {
            return Some(Scope::User);
        };
        let p = self.workspace.project(pid)?;
        Some(Scope::Project(match &p.remote {
            Some(r) => r.remote_root.clone(),
            None => p.root_path.to_string_lossy().into_owned(),
        }))
    }

    /// The projects on the chosen machine.
    fn skills_projects(&self) -> Vec<(Uuid, String)> {
        self.workspace
            .projects
            .iter()
            .filter(|p| match (&p.remote, self.skills_page.machine) {
                (None, None) => true,
                (Some(r), Some(host)) => r.host_id == host,
                _ => false,
            })
            .map(|p| (p.id, p.name.clone()))
            .collect()
    }

    /// The page was opened: scan once.
    pub(super) fn skills_page_opened(&mut self, cx: &mut Context<Self>) {
        if !self.skills_page.loaded {
            self.skills_rescan(cx);
        }
    }

    fn skills_set_machine(&mut self, machine: Option<Uuid>, cx: &mut Context<Self>) {
        if self.skills_page.machine == machine {
            return;
        }
        self.skills_page.machine = machine;
        self.skills_page.project = None;
        self.skills_reset_view();
        self.skills_rescan(cx);
    }

    fn skills_set_project(&mut self, project: Option<Uuid>, cx: &mut Context<Self>) {
        if self.skills_page.project == project {
            return;
        }
        self.skills_page.project = project;
        self.skills_reset_view();
        self.skills_rescan(cx);
    }

    fn skills_reset_view(&mut self) {
        let page = &mut self.skills_page;
        page.generation += 1;
        page.snapshot = None;
        page.selected = None;
        page.updates.clear();
        page.busy.clear();
        page.notices.clear();
        page.scan_error = None;
        page.checking = false;
    }

    fn skills_rescan(&mut self, cx: &mut Context<Self>) {
        let (Some(machine), Some(scope)) = (self.skills_machine(), self.skills_scope()) else {
            self.skills_page.scan_error =
                Some(t("That host or project no longer exists.").to_string());
            cx.notify();
            return;
        };
        self.skills_page.loaded = true;
        self.skills_page.scanning = true;
        self.skills_page.scan_error = None;
        self.skills_spawn(cx, move || {
            SkillsOutcome::Scanned(sk::connect(machine).and_then(|h| sk::scan(&h, &scope)))
        });
    }

    /// Run `job` on its own thread; apply its outcome if the page still shows
    /// the same machine and scope.
    fn skills_spawn(
        &mut self,
        cx: &mut Context<Self>,
        job: impl FnOnce() -> SkillsOutcome + Send + 'static,
    ) {
        let generation = self.skills_page.generation;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                .unwrap_or_else(|_| {
                    SkillsOutcome::Scanned(Err(
                        t("The skills job stopped unexpectedly.").to_string()
                    ))
                });
            let _ = tx.send_blocking(outcome);
        });
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            if let Ok(outcome) = rx.recv().await {
                let _ = view.update(cx, |this, cx| {
                    if this.skills_page.generation == generation {
                        this.skills_apply(outcome, cx);
                    } else if let SkillsOutcome::Fetched(Ok(f)) = outcome {
                        release(Some(Arc::new(f)));
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn skills_apply(&mut self, outcome: SkillsOutcome, cx: &mut Context<Self>) {
        let page = &mut self.skills_page;
        match outcome {
            SkillsOutcome::Scanned(res) => {
                page.scanning = false;
                match res {
                    Ok(snap) => page.snapshot = Some(snap),
                    Err(e) => page.scan_error = Some(e),
                }
            }
            SkillsOutcome::Fetched(res) => {
                page.fetching = false;
                match res {
                    Ok(f) => {
                        // Tick everything that isn't already installed.
                        let installed = |name: &str| {
                            page.snapshot
                                .as_ref()
                                .is_some_and(|s| s.row(name).is_some())
                        };
                        page.picks = f
                            .skills
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| !installed(&c.name))
                            .map(|(i, _)| i)
                            .collect();
                        if f.skills.is_empty() {
                            page.fetch_error =
                                Some(t("No skills were found in that repository.").to_string());
                        }
                        release(page.fetched.replace(Arc::new(f)));
                    }
                    Err(e) => page.fetch_error = Some(e),
                }
            }
            SkillsOutcome::Changed {
                keys,
                report,
                snapshot,
            } => {
                for k in &keys {
                    page.busy.remove(k);
                    page.updates.remove(k);
                }
                page.notices = report
                    .failed
                    .iter()
                    .map(|(name, e)| (tf("{name}: {error}", &[("name", name), ("error", e)]), true))
                    .chain(report.lock_warning.iter().map(|w| (w.clone(), false)))
                    .collect();
                match snapshot {
                    Ok(snap) => {
                        if let Some(sel) = &page.selected
                            && snap.row(sel).is_none()
                        {
                            page.selected = None;
                        }
                        page.snapshot = Some(snap);
                    }
                    Err(e) => page.scan_error = Some(e),
                }
                // Installed skills drop out of the preview's ticks.
                if let Some(f) = &page.fetched {
                    page.picks.retain(|&i| {
                        f.skills
                            .get(i)
                            .is_some_and(|c| !report.done.contains(&c.name))
                    });
                }
            }
            SkillsOutcome::Updates(map) => {
                page.checking = false;
                let fresh = map.values().all(|s| *s == UpdateState::UpToDate);
                page.updates = map;
                page.notices = if fresh {
                    vec![(
                        t("Every skill with a known source is up to date.").to_string(),
                        false,
                    )]
                } else {
                    Vec::new()
                };
            }
        }
        cx.notify();
    }

    // --- Actions ---

    fn skills_find(&mut self, cx: &mut Context<Self>) {
        let input = self.settings_ui.skill_source.read(cx).value().to_string();
        let Some(work) = skills_work_dir() else {
            self.skills_page.fetch_error =
                Some(t("muxel has no data folder to download into.").to_string());
            cx.notify();
            return;
        };
        let env = self.git_env.clone();
        let page = &mut self.skills_page;
        page.fetching = true;
        page.fetch_error = None;
        page.picks.clear();
        release(page.fetched.take());
        self.skills_spawn(cx, move || {
            SkillsOutcome::Fetched(sk::fetch(&env, &work, &input).map_err(|e| fetch_error_text(&e)))
        });
    }

    /// The agents an install targets right now.
    fn skills_targets(&self) -> BTreeSet<SkillAgent> {
        let detected: BTreeSet<SkillAgent> = self
            .skills_page
            .snapshot
            .as_ref()
            .map(|s| s.detected.iter().copied().collect())
            .unwrap_or_default();
        match &self.skills_page.targets {
            Some(t) => t.intersection(&detected).copied().collect(),
            None => detected,
        }
    }

    fn skills_install_picks(&mut self, cx: &mut Context<Self>) {
        let Some(fetched) = self.skills_page.fetched.clone() else {
            return;
        };
        let (Some(machine), Some(scope)) = (self.skills_machine(), self.skills_scope()) else {
            return;
        };
        let picks: Vec<usize> = self.skills_page.picks.iter().copied().collect();
        let keys: Vec<String> = picks
            .iter()
            .filter_map(|&i| fetched.skills.get(i).map(|c| c.name.clone()))
            .collect();
        let agents = self.skills_targets();
        self.skills_page.busy.extend(keys.iter().cloned());
        self.skills_spawn(cx, move || {
            change_job(machine, scope, keys, |host, snap| {
                sk::install(host, snap, &fetched, &picks, &agents)
            })
        });
    }

    fn skills_install_curated(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(c) = muxel_core::skills::curated::find(id) else {
            return;
        };
        let (Some(machine), Some(scope), Some(work)) = (
            self.skills_machine(),
            self.skills_scope(),
            skills_work_dir(),
        ) else {
            return;
        };
        let env = self.git_env.clone();
        let agents = self.skills_targets();
        let busy = format!("curated:{id}");
        self.skills_page.busy.insert(busy.clone());
        self.skills_spawn(cx, move || {
            let fetched = match sk::fetch(&env, &work, c.repo) {
                Ok(f) => f,
                Err(e) => {
                    return SkillsOutcome::Changed {
                        keys: vec![busy.clone()],
                        report: ChangeReport {
                            failed: vec![(c.repo.to_string(), fetch_error_text(&e))],
                            ..Default::default()
                        },
                        snapshot: sk::connect(machine).and_then(|h| sk::scan(&h, &scope)),
                    };
                }
            };
            let picks: Vec<usize> = fetched
                .skills
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    c.skills.is_empty() || c.skills.contains(&s.found.frontmatter.name.as_str())
                })
                .map(|(i, _)| i)
                .collect();
            change_job(machine, scope, vec![busy], move |host, snap| {
                let mut report = sk::install(host, snap, &fetched, &picks, &agents);
                if picks.is_empty() {
                    report.failed.push((
                        c.repo.to_string(),
                        t("The skill is no longer in its repository.").to_string(),
                    ));
                }
                report
            })
        });
    }

    fn skills_set_agents(
        &mut self,
        key: String,
        agents: BTreeSet<SkillAgent>,
        cx: &mut Context<Self>,
    ) {
        let (Some(machine), Some(scope)) = (self.skills_machine(), self.skills_scope()) else {
            return;
        };
        self.skills_page.busy.insert(key.clone());
        self.skills_spawn(cx, move || {
            change_job(machine, scope, vec![key.clone()], move |host, snap| {
                sk::set_agents(host, snap, &key, &agents)
            })
        });
    }

    pub(super) fn skills_remove(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(machine), Some(scope)) = (self.skills_machine(), self.skills_scope()) else {
            return;
        };
        self.skills_page.busy.insert(key.clone());
        self.skills_spawn(cx, move || {
            change_job(machine, scope, vec![key.clone()], move |host, snap| {
                sk::remove(host, snap, &key)
            })
        });
    }

    fn skills_update(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(machine), Some(scope), Some(work)) = (
            self.skills_machine(),
            self.skills_scope(),
            skills_work_dir(),
        ) else {
            return;
        };
        let env = self.git_env.clone();
        self.skills_page.busy.insert(key.clone());
        self.skills_spawn(cx, move || {
            change_job(machine, scope, vec![key.clone()], move |host, snap| {
                sk::update(&env, &work, host, snap, &key)
            })
        });
    }

    fn skills_check_updates(&mut self, cx: &mut Context<Self>) {
        let (Some(snap), Some(work)) = (self.skills_page.snapshot.clone(), skills_work_dir())
        else {
            return;
        };
        let env = self.git_env.clone();
        self.skills_page.checking = true;
        self.skills_page.notices.clear();
        self.skills_spawn(cx, move || {
            SkillsOutcome::Updates(sk::check_updates(&env, &work, &snap))
        });
    }

    // --- Rendering ---

    pub(super) fn render_settings_skills(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let pal = form.pal;
        let page = &self.skills_page;
        let mut col = v_flex().w(form.w).gap(form.rem);
        col = col.child(self.render_skills_toolbar(form, cx));
        if page.scanning && page.snapshot.is_none() {
            col = col.child(form.status(t("Looking for skills…"), pal.dim));
        }
        if let Some(e) = &page.scan_error {
            col = col.child(form.status(e.clone(), pal.danger));
        }
        let Some(snap) = &page.snapshot else {
            return col.into_any_element();
        };
        if let Err(e) = &snap.lock {
            let msg = match e {
                muxel_core::skills::lock::LockError::Malformed => {
                    t("The skills CLI's lock file couldn't be read, so muxel won't change it.")
                }
                muxel_core::skills::lock::LockError::Newer(_) => t(
                    "The skills CLI's lock file is from a newer version, so muxel won't change it.",
                ),
            };
            col = col.child(form.status(msg, pal.warning));
        }
        let bundled = snap.rows.iter().filter(|r| r.read_only().is_some()).count();
        let rows: Vec<&SkillRow> = snap
            .rows
            .iter()
            .filter(|r| r.read_only().is_none())
            .chain(
                snap.rows
                    .iter()
                    .filter(|r| page.show_bundled && r.read_only().is_some()),
            )
            .collect();
        let empty = rows.is_empty();
        let items = rows
            .into_iter()
            .map(|row| self.skills_list_item(row, snap, form, cx))
            .collect();
        let detail = form.detail();
        let details = match page.selected.as_deref().and_then(|k| snap.row(k)) {
            Some(row) => self.render_skill_details(row, snap, &detail, cx),
            None => self.render_skills_add(snap, &detail, cx),
        };
        col.child(form.master_detail(
            items,
            Some(
                add_button("add-skill", t("Add skills")).on_click(cx.listener(
                    |this, _e, _w, cx| {
                        this.skills_page.selected = None;
                        cx.notify();
                    },
                )),
            ),
            if bundled > 0 && !page.show_bundled {
                Some(
                    tn(
                        "{n} skill that comes with an agent or from claude.ai is hidden.",
                        "{n} skills that come with an agent or from claude.ai are hidden.",
                        bundled,
                        &[("n", &bundled.to_string())],
                    )
                    .into(),
                )
            } else {
                empty.then(|| t("No skills are installed here yet."))
            },
            details,
        ))
        .into_any_element()
    }

    fn render_skills_toolbar(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let page = &self.skills_page;
        let app = cx.entity();
        let machine_name: SharedString = match page.machine {
            None => t("This computer"),
            Some(id) => self
                .remotes
                .iter()
                .find(|h| h.id == id)
                .map(|h| SharedString::from(h.name.clone()))
                .unwrap_or_else(|| t("This computer")),
        };
        let hosts: Vec<(Uuid, String)> = self
            .remotes
            .iter()
            .map(|h| (h.id, h.name.clone()))
            .collect();
        let current_machine = page.machine;
        let machine_menu = {
            let app = app.clone();
            DropdownButton::new("skills-machine")
                .small()
                .button(
                    Button::new("skills-machine-btn")
                        .outline()
                        .label(machine_name),
                )
                .dropdown_menu(move |mut menu, window, _cx| {
                    menu = menu.item(
                        PopupMenuItem::new(t("This computer"))
                            .checked(current_machine.is_none())
                            .on_click(window.listener_for(&app, |this, _e, _w, cx| {
                                this.skills_set_machine(None, cx)
                            })),
                    );
                    for (id, name) in &hosts {
                        let id = *id;
                        menu = menu.item(
                            PopupMenuItem::new(name.clone())
                                .checked(current_machine == Some(id))
                                .on_click(window.listener_for(&app, move |this, _e, _w, cx| {
                                    this.skills_set_machine(Some(id), cx)
                                })),
                        );
                    }
                    menu.scrollable(true)
                })
        };
        let projects = self.skills_projects();
        let scope_name: SharedString = match page.project {
            None => t("User skills"),
            Some(pid) => projects
                .iter()
                .find(|(id, _)| *id == pid)
                .map(|(_, n)| tf("Project: {name}", &[("name", n)]).into())
                .unwrap_or_else(|| t("User skills")),
        };
        let current_project = page.project;
        let scope_menu = DropdownButton::new("skills-scope")
            .small()
            .button(Button::new("skills-scope-btn").outline().label(scope_name))
            .dropdown_menu(move |mut menu, window, _cx| {
                menu = menu.item(
                    PopupMenuItem::new(t("User skills"))
                        .checked(current_project.is_none())
                        .on_click(window.listener_for(&app, |this, _e, _w, cx| {
                            this.skills_set_project(None, cx)
                        })),
                );
                for (id, name) in &projects {
                    let id = *id;
                    menu = menu.item(
                        PopupMenuItem::new(tf("Project: {name}", &[("name", name)]))
                            .checked(current_project == Some(id))
                            .on_click(window.listener_for(&app, move |this, _e, _w, cx| {
                                this.skills_set_project(Some(id), cx)
                            })),
                    );
                }
                menu.scrollable(true)
            });
        let busy = page.scanning || page.checking;
        let bundled = page
            .snapshot
            .as_ref()
            .map(|s| s.rows.iter().filter(|r| r.read_only().is_some()).count())
            .unwrap_or(0);
        let adding = page.selected.is_none();
        let add = Button::new("skills-add")
            .small()
            .icon(IconName::Plus)
            .label(t("Add skills"))
            .disabled(page.snapshot.is_none())
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.skills_page.selected = None;
                cx.notify();
            }));
        div()
            .w(form.w)
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(if adding { add.outline() } else { add.primary() })
            .child(machine_menu)
            .child(scope_menu)
            .child(div().flex_1())
            .children((bundled > 0).then(|| {
                title_button(
                    "skills-show-bundled",
                    if page.show_bundled {
                        t("Hide built-in")
                    } else {
                        t("Show built-in")
                    },
                )
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.skills_page.show_bundled = !this.skills_page.show_bundled;
                    cx.notify();
                }))
            }))
            .child(
                title_button(
                    "skills-check-updates",
                    if page.checking {
                        t("Checking…")
                    } else {
                        t("Check for updates")
                    },
                )
                .disabled(busy || page.snapshot.is_none())
                .on_click(cx.listener(|this, _e, _w, cx| this.skills_check_updates(cx))),
            )
            .child(
                title_button(
                    "skills-refresh",
                    if page.scanning {
                        t("Refreshing…")
                    } else {
                        t("Refresh")
                    },
                )
                .disabled(busy)
                .on_click(cx.listener(|this, _e, _w, cx| this.skills_rescan(cx))),
            )
            .into_any_element()
    }

    fn skills_list_item(
        &self,
        row: &SkillRow,
        snap: &Snapshot,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let pal = form.pal;
        let key = row.key.clone();
        let selected = self.skills_page.selected.as_deref() == Some(row.key.as_str());
        let read_only = row.read_only().is_some();
        let fg = form.item_fg(selected, read_only);
        let busy = self.skills_page.busy.contains(&row.key);
        let update = matches!(
            self.skills_page.updates.get(&row.key),
            Some(UpdateState::Available(_))
        );
        let trailing = if busy {
            Some(
                div()
                    .text_sm()
                    .text_color(if selected { fg } else { pal.dim })
                    .child("…")
                    .into_any_element(),
            )
        } else if update {
            Some(
                div()
                    .size(px(7.0))
                    .rounded_full()
                    .bg(pal.accent)
                    .into_any_element(),
            )
        } else {
            let seen = row.visible_to(&snap.home, &snap.scope, &snap.detected);
            let mut icons = div().flex().items_center().gap_0p5().flex_none();
            for a in seen.iter().take(4) {
                icons = icons.child(
                    svg()
                        .path(agent_icon(*a))
                        .size(px(11.0))
                        .flex_none()
                        .text_color(if selected { fg } else { pal.dim }),
                );
            }
            if seen.len() > 4 {
                icons = icons.child(
                    div()
                        .text_xs()
                        .text_color(pal.dim)
                        .child(format!("+{}", seen.len() - 4)),
                );
            }
            (!seen.is_empty()).then(|| icons.into_any_element())
        };
        ListItem {
            id: SharedString::from(format!("skill-row-{}", row.key)),
            icon: Some(
                Icon::empty()
                    .path(SKILL_ICON)
                    .small()
                    .text_color(fg)
                    .into_any_element(),
            ),
            label: row.name.clone().into(),
            sub: row.read_only().map(|p| match p {
                Protection::Synced => t("Synced from claude.ai"),
                Protection::Bundled(a) => {
                    tf("Bundled with {agent}", &[("agent", a.label())]).into()
                }
            }),
            trailing,
            selected,
            dim: read_only,
            on_click: Box::new(cx.listener(move |this, _e, _w, cx| {
                this.skills_page.selected = Some(key.clone());
                cx.notify();
            })),
        }
    }

    /// A row of agent toggles: which agents an install targets.
    fn render_skill_targets(&self, snap: &Snapshot, form: &Form, cx: &mut Context<Self>) -> Div {
        let targets = self.skills_targets();
        let all = self.skills_page.targets.is_none();
        let mut chips = div().w(form.control_w()).flex().flex_wrap().gap_1();
        chips = chips.child({
            let b = Button::new("skill-target-all")
                .small()
                .label(t("All agents"));
            let b = if all { b.primary() } else { b.outline() };
            b.on_click(cx.listener(|this, _e, _w, cx| {
                this.skills_page.targets = None;
                cx.notify();
            }))
        });
        for &a in &snap.detected {
            let on = !all && targets.contains(&a);
            let b = Button::new(SharedString::from(format!("skill-target-{}", a.program())))
                .small()
                .icon(Icon::empty().path(agent_icon(a)))
                .label(a.label());
            let b = if on { b.primary() } else { b.outline() };
            chips = chips.child(b.on_click(cx.listener(move |this, _e, _w, cx| {
                let mut set = match &this.skills_page.targets {
                    Some(s) => s.clone(),
                    None => BTreeSet::new(),
                };
                if !set.remove(&a) {
                    set.insert(a);
                }
                this.skills_page.targets = Some(set);
                cx.notify();
            })));
        }
        chips
    }

    fn render_skills_add(
        &self,
        snap: &Snapshot,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = form.pal;
        let page = &self.skills_page;
        let no_agents = snap.detected.is_empty();
        let targets = self.skills_targets();
        let mut col = v_flex().w(form.w).gap(form.rem).child(form.editor_title(
            None,
            t("Add skills"),
            Vec::new(),
        ));
        if no_agents {
            col = col.child(form.status(
                t("No agent that uses skills is installed here."),
                pal.warning,
            ));
        } else {
            col = col.child(form.group(
                Group::new(
                    "",
                    vec![
                        Row::stacked(t("Install for"), self.render_skill_targets(snap, form, cx))
                            .desc(t("Some agents also read other agents' skills folders, so a skill can reach a few more agents than you pick.")),
                    ],
                ),
                &[],
            ));
        }

        // From a repository.
        let find = Button::new("skills-find")
            .outline()
            .small()
            .label(if page.fetching {
                t("Fetching…")
            } else {
                t("Find skills")
            })
            .disabled(page.fetching)
            .on_click(cx.listener(|this, _e, _w, cx| this.skills_find(cx)));
        let input_w = form.control_w() - form.rem * 6.0;
        col = col.child(
            form.group(
                Group::new(
                    t("From GitHub or a git repository"),
                    vec![Row::stacked(
                        t("Repository"),
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(form.input_w(
                                &self.settings_ui.skill_source,
                                if input_w < px(140.0) { px(140.0) } else { input_w },
                            ))
                            .child(find),
                    )
                    .desc(t("owner/repo, owner/repo@skill, a GitHub URL (also …/tree/branch/folder), or any git URL. Uses your git login."))],
                ),
                &[],
            ),
        );
        if let Some(e) = &page.fetch_error {
            col = col.child(form.status(e.clone(), pal.danger));
        }
        if let Some(f) = &page.fetched {
            col = col.child(self.render_skill_preview(f, snap, &targets, form, cx));
        }
        for (msg, err) in &page.notices {
            col = col.child(form.status(msg.clone(), if *err { pal.danger } else { pal.dim }));
        }

        // Recommended.
        col = col.child(form.heading(t("Recommended")));
        for cat in CuratedCategory::ALL {
            let mut rows = Vec::new();
            for c in CATALOG.iter().filter(|c| c.category == cat) {
                rows.push(self.render_curated_row(c, snap, no_agents || targets.is_empty(), cx));
            }
            col = col.child(form.group(Group::new(category_title(cat), rows), &[]));
        }
        col.into_any_element()
    }

    fn render_skill_preview(
        &self,
        f: &Fetched,
        snap: &Snapshot,
        targets: &BTreeSet<SkillAgent>,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> Div {
        let pal = form.pal;
        let page = &self.skills_page;
        let mut card = form.card().p_3().gap_2();
        for (i, c) in f.skills.iter().enumerate() {
            let checked = page.picks.contains(&i);
            let fm = &c.found.frontmatter;
            let mut notes: Vec<String> = Vec::new();
            if let Some(l) = &fm.license {
                notes.push(tf("License: {license}", &[("license", l)]));
            }
            if c.found.has_scripts {
                notes.push(t("Includes scripts the agent may run").to_string());
            }
            if snap.row(&c.name).is_some() {
                notes.push(t("Replaces the installed copy").to_string());
            }
            card = card.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        Checkbox::new(SharedString::from(format!("skill-pick-{i}")))
                            .checked(checked)
                            .on_click(cx.listener(move |this, on: &bool, _w, cx| {
                                if *on {
                                    this.skills_page.picks.insert(i);
                                } else {
                                    this.skills_page.picks.remove(&i);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(pal.text)
                                    .child(c.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.dim)
                                    .child(fm.description.clone()),
                            )
                            .children((!notes.is_empty()).then(|| {
                                div()
                                    .text_xs()
                                    .text_color(pal.warning)
                                    .child(notes.join(" · "))
                            })),
                    ),
            );
        }
        let busy = f.skills.iter().any(|c| page.busy.contains(&c.name));
        let n = page.picks.len();
        v_flex()
            .w(form.w)
            .gap_2()
            .child(form.heading(tf(
                "{source} — {count}",
                &[
                    ("source", &f.source.display),
                    (
                        "count",
                        &tn(
                            "{n} skill found",
                            "{n} skills found",
                            f.skills.len(),
                            &[("n", &f.skills.len().to_string())],
                        ),
                    ),
                ],
            )))
            .child(card)
            .child(
                div().w(form.w).flex().justify_end().gap_2().child(
                    Button::new("skills-install")
                        .primary()
                        .small()
                        .label(if busy {
                            t("Installing…").to_string()
                        } else {
                            tn(
                                "Install {n} skill",
                                "Install {n} skills",
                                n,
                                &[("n", &n.to_string())],
                            )
                        })
                        .disabled(busy || n == 0 || targets.is_empty())
                        .on_click(cx.listener(|this, _e, _w, cx| this.skills_install_picks(cx))),
                ),
            )
    }

    fn render_curated_row(
        &self,
        c: &'static CuratedSkill,
        snap: &Snapshot,
        no_targets: bool,
        cx: &mut Context<Self>,
    ) -> Row {
        let (title, blurb) = curated_text(c);
        let installed = if c.skills.is_empty() {
            false
        } else {
            c.skills.iter().all(|s| snap.row(s).is_some())
        };
        let busy = self.skills_page.busy.contains(&format!("curated:{}", c.id));
        let id = c.id;
        let button = if installed {
            Button::new(SharedString::from(format!("curated-{id}")))
                .ghost()
                .small()
                .label(t("Installed"))
                .disabled(true)
        } else {
            Button::new(SharedString::from(format!("curated-{id}")))
                .outline()
                .small()
                .label(if busy {
                    t("Installing…")
                } else {
                    t("Install")
                })
                .disabled(busy || no_targets)
                .on_click(cx.listener(move |this, _e, _w, cx| this.skills_install_curated(id, cx)))
        };
        let url = c.web_url();
        let repo = Button::new(SharedString::from(format!("curated-repo-{id}")))
            .ghost()
            .xsmall()
            .label(format!("{} · {}", c.repo, c.license))
            .on_click(move |_e, _w, cx| cx.open_url(&url));
        Row::new(
            title,
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .child(repo)
                .child(button),
        )
        .desc(blurb)
        .keywords(SharedString::from(c.id))
    }

    fn render_skill_details(
        &self,
        row: &SkillRow,
        snap: &Snapshot,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = form.pal;
        let page = &self.skills_page;
        let key = row.key.clone();
        let busy = page.busy.contains(&row.key);
        let read_only = row.read_only();
        let mut actions: Vec<AnyElement> = Vec::new();
        if let Some(UpdateState::Available(_)) = page.updates.get(&row.key) {
            let k = key.clone();
            actions.push(
                Button::new("skill-update")
                    .primary()
                    .small()
                    .label(t("Update"))
                    .disabled(busy)
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.skills_update(k.clone(), cx)),
                    )
                    .into_any_element(),
            );
        }
        if read_only.is_none() {
            let k = key.clone();
            let name = row.name.clone();
            actions.push(
                delete_button("skill-remove", t("Remove…"))
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.request_confirm(
                            tf("Remove “{name}”?", &[("name", &name)]),
                            t("The skill is deleted from every agent's skills folder here. Agents that are running keep it until they restart."),
                            t("Remove"),
                            ConfirmAction::RemoveSkill(k.clone()),
                            cx,
                        )
                    }))
                    .into_any_element(),
            );
        }
        let mut col = v_flex().w(form.w).gap(form.rem).child(
            form.editor_title(
                Some(
                    Icon::empty()
                        .path(SKILL_ICON)
                        .small()
                        .text_color(pal.text)
                        .into_any_element(),
                ),
                row.name.clone(),
                actions,
            ),
        );
        if busy {
            col = col.child(form.status(t("Working…"), pal.dim));
        }
        if let Some(p) = read_only {
            col = col.child(form.status(protection_text(p), pal.dim));
        }
        match page.updates.get(&row.key) {
            Some(UpdateState::Available(_)) => {
                col = col.child(form.status(t("An update is available."), pal.accent))
            }
            Some(UpdateState::Gone) => {
                col = col
                    .child(form.status(t("The skill is no longer in its repository."), pal.warning))
            }
            Some(UpdateState::Failed(e)) => col = col.child(form.status(e.clone(), pal.danger)),
            _ => {}
        }
        for (msg, err) in &page.notices {
            col = col.child(form.status(msg.clone(), if *err { pal.danger } else { pal.dim }));
        }

        let value = |text: String| {
            div()
                .w(form.control_w())
                .text_sm()
                .text_color(pal.dim)
                .child(text)
        };
        let mut info = vec![Row::stacked(
            t("Description"),
            value(row.description.clone()),
        )];
        if let Some(rec) = &row.record {
            let src = rec.source.clone();
            let web = muxel_core::skills::source::parse(&rec.source_url)
                .ok()
                .and_then(|s| s.web_url());
            let source: AnyElement = match web {
                Some(url) => Button::new("skill-source")
                    .ghost()
                    .small()
                    .label(src)
                    .on_click(move |_e, _w, cx| cx.open_url(&url))
                    .into_any_element(),
                None => value(src).into_any_element(),
            };
            info.push(Row::new(t("Source"), source));
            if !rec.updated_at.is_empty() {
                info.push(Row::new(t("Updated"), value(short_date(&rec.updated_at))));
            }
        } else if read_only.is_none() {
            info.push(Row::new(
                t("Source"),
                value(t("Unknown — installed by hand").to_string()),
            ));
        }
        if let Some(l) = &row.license {
            info.push(Row::new(t("License"), value(l.clone())));
        }
        col = col.child(form.group(Group::new("", info), &[]));

        // Availability.
        let seen = row.visible_to(&snap.home, &snap.scope, &snap.detected);
        let mut avail = Vec::new();
        for &a in &snap.detected {
            let on = seen.contains(&a);
            let k = key.clone();
            let current = seen.clone();
            let enabled = read_only.is_none() && !busy;
            let switch = form.switch(
                SharedString::from(format!("skill-agent-{}", a.program())),
                on,
                enabled,
                cx.listener(move |this, want: &bool, _w, cx| {
                    let mut set = current.clone();
                    if *want {
                        set.insert(a);
                    } else {
                        set.remove(&a);
                    }
                    if set.is_empty() {
                        // Turning the last one off is a removal: ask.
                        this.request_confirm(
                            t("Remove the skill?"),
                            t("No agent would have it any more, so it would be deleted."),
                            t("Remove"),
                            ConfirmAction::RemoveSkill(k.clone()),
                            cx,
                        );
                        return;
                    }
                    this.skills_set_agents(k.clone(), set, cx);
                }),
            );
            avail.push(Row::narrow(
                SharedString::from(a.label()),
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(svg().path(agent_icon(a)).size(px(14.0)).text_color(pal.dim))
                    .child(switch),
            ));
        }
        if !avail.is_empty() {
            col = col
                .child(form.group(Group::new(t("Available to"), avail), &[]))
                .child(form.note(t(
                    "opencode, Amp and Grok also read Claude Code's skills folder, and most agents read ~/.agents/skills, so a switch can stay on after you turn it off.",
                )));
        }

        // Where it is.
        let mut locs = div().w(form.w).flex().flex_col().gap_1();
        let local = page.machine.is_none();
        for (i, l) in row.locations.iter().enumerate() {
            let kind = match (&l.kind, l.broken) {
                (_, true) => t("broken link").to_string(),
                (LocKind::Link { target }, _) => tf("link to {target}", &[("target", target)]),
                (LocKind::Real, _) => t("folder").to_string(),
            };
            let path = l.path.clone();
            locs = locs.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_family("monospace")
                            .text_color(pal.dim)
                            .truncate()
                            .child(format!("{}  ({kind})", l.path)),
                    )
                    .children(local.then(|| {
                        Button::new(SharedString::from(format!("skill-open-{i}")))
                            .ghost()
                            .xsmall()
                            .label(t("Show"))
                            .on_click(move |_e, _w, _cx| {
                                integrations::reveal_in_file_manager(Path::new(&path))
                            })
                    })),
            );
        }
        col = col.child(form.heading(t("Installed at"))).child(locs);
        col.into_any_element()
    }
}

/// `2026-11-12T04:16:21.494Z` → the local date and time.
fn short_date(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| iso.to_string())
}

impl MuxelApp {
    /// The count beside "Skills" in the list: changeable skills, once scanned.
    pub(super) fn skills_count(&self) -> Option<usize> {
        let snap = self.skills_page.snapshot.as_ref()?;
        Some(snap.rows.iter().filter(|r| r.read_only().is_none()).count())
    }
}
