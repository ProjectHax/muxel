//! The mutation-time boundary for outside pane creation. Tests compile this same
//! method against an inert host so refusal cannot reach placement or spawning.

use super::*;

impl MuxelApp {
    pub(super) fn creation_decision(&self, instance: &Instance) -> ctl::CreationDecision {
        let class = if instance.literal_argv {
            ctl::LaunchClass::Literal
        } else if readaloud::is_agent_program(instance.program.as_deref()) {
            ctl::LaunchClass::Preset
        } else {
            ctl::LaunchClass::Shell
        };
        ctl::creation_decision(
            self.settings.control_enabled,
            self.settings.control_allow_new,
            self.settings.control_allow_shells,
            class,
        )
    }

    /// Resolve without selecting, persisting, discovering an executable or
    /// spawning anything. Used both for admission and the final mutation check.
    pub(super) fn prepare_control_new(
        &self,
        pid: Uuid,
        options: &ctl::NewOptions,
    ) -> CtlResult<Instance> {
        if !self.settings.control_enabled {
            return Err(ctl::creation_refusal(ErrorCode::ControlDisabled));
        }
        if self.settings.control_allow_new == ctl::CreationPolicy::Off {
            return Err(ctl::creation_refusal(ErrorCode::CreationDisabled));
        }
        if self.confirm_quit || self.show_quit_confirm {
            return Err("muxel is quitting".into());
        }
        if self.current_workspace.is_none()
            || self.workspace_busy.is_some()
            || self.confirm.as_ref().is_some_and(|confirm| {
                matches!(confirm.action,
                ConfirmAction::DeleteWorkspace(id) if Some(id) == self.current_workspace)
            })
        {
            return Err("the workspace is closing or being deleted".into());
        }
        if self.confirm.as_ref().is_some_and(
            |confirm| matches!(confirm.action, ConfirmAction::DeleteProject(id) if id == pid),
        ) {
            return Err("the project is being deleted".into());
        }
        let project = self
            .workspace
            .project(pid)
            .ok_or("the project has closed")?;
        if project.is_remote() {
            return Err("`new` does not support remote projects yet".into());
        }
        let secondary = self.secondary_windows.iter().find(|s| s.pid == pid);
        let default_index =
            if self.workspace.active_project == Some(pid) || secondary.is_some_and(|s| s.active) {
                self.current_preset
            } else {
                project
                    .default_preset
                    .and_then(|id| self.presets.iter().position(|p| p.id == id))
                    .or_else(|| {
                        self.presets.iter().position(|p| {
                            p.id.to_string() == self.settings.default_preset
                                || p.name == self.settings.default_preset
                        })
                    })
                    .unwrap_or(0)
            };
        let instance = options.instance(project, &self.presets, default_index)?;
        if let ctl::CreationDecision::Refuse(code) = self.creation_decision(&instance) {
            return Err(ctl::creation_refusal(code));
        }
        let cwd = instance.launch_dir.as_ref().expect("new always sets cwd");
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err("--cwd must be an existing absolute directory".into());
        }
        Ok(instance)
    }

    pub(super) fn control_new(
        &mut self,
        pid: Uuid,
        options: ctl::NewOptions,
        approved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CtlResult<Value> {
        // Current settings and lifecycle still govern an approved request.
        let mut instance = self.prepare_control_new(pid, &options)?;
        if self.creation_decision(&instance) == ctl::CreationDecision::Ask && !approved {
            return Err(
                "this terminal creation requires a click in muxel; retry the command".into(),
            );
        }
        let secondary = self.secondary_windows.iter().find(|s| s.pid == pid);
        let expected_window = secondary.map(|s| s.handle).or(self.main_window);
        if expected_window
            .is_none_or(|handle| handle.window_id() != window.window_handle().window_id())
        {
            return Err("the project's window changed; retry the command".into());
        }
        let project = self
            .workspace
            .project(pid)
            .ok_or("the project has closed")?;
        let cwd = instance.launch_dir.as_ref().expect("new always sets cwd");
        if instance.literal_argv {
            let program = instance
                .program
                .as_deref()
                .expect("literal launch has program");
            let relative = Path::new(program);
            let program =
                if relative.is_relative() && (program.contains('/') || program.contains('\\')) {
                    cwd.join(relative).to_string_lossy().into_owned()
                } else {
                    program.to_string()
                };
            instance.program = Some(
                muxel_terminal::literal_program_for_spawn(&program)
                    .map_err(|error| error.to_string())?,
            );
        }
        let target = self
            .active_instance
            .filter(|iid| {
                project
                    .layout
                    .as_ref()
                    .is_some_and(|l| l.leaf_containing(*iid).is_some())
            })
            .or_else(|| project.preferred_instance());
        let iid = instance.id;
        let result = json!({
            "id": ctl::short_id(iid), "uuid": iid,
            "project": project.name, "project_id": pid,
            "cwd": cwd,
            "program": instance.program.clone().unwrap_or_else(|| CommandSpec::shell().program),
            "args": instance.args,
        });
        let placement = match options.placement {
            ctl::NewPlacement::Tab => PlacementMode::Tab,
            ctl::NewPlacement::Right => PlacementMode::Split(SplitDirection::Horizontal),
            ctl::NewPlacement::Down => PlacementMode::Split(SplitDirection::Vertical),
        };
        // Placement may reserve or even launch immediately. Publish pending work
        // in the same UI transaction, before either path can consume the task.
        if let Some(prompt) = options.prompt.as_ref() {
            self.control_turns
                .insert(iid, ControlTurn::startup(prompt.clone()));
        }
        if options.background {
            self.place_and_spawn_with_focus(
                pid,
                instance,
                placement,
                target,
                Some(WorktreeChoice::None),
                false,
                window,
                cx,
            );
        } else {
            self.select_project(pid, window, cx);
            self.place_and_spawn(
                pid,
                instance,
                placement,
                target,
                Some(WorktreeChoice::None),
                window,
                cx,
            );
            window.activate_window();
        }
        if self.workspace.instance(iid).is_none() {
            self.control_turns.remove(&iid);
            return Err("the target pane is no longer in the project's layout".into());
        }
        Ok(result)
    }
}
