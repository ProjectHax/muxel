//! Pure validation, launch selection and placement for outside pane creation.

use super::{CliCommand, Command, Error, ErrorCode, creation_refusal};
use crate::pane::{PaneNode, add_tab, set_active_tab, split};
use crate::{AgentPreset, Instance, PresetKind, Project, SplitDirection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewPlacement {
    #[default]
    Tab,
    Right,
    Down,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewOptions {
    pub cwd: Option<PathBuf>,
    pub name: Option<String>,
    pub placement: NewPlacement,
    pub background: bool,
    pub preset: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Includes PROGRAM, with every subsequent token preserved verbatim.
    pub argv: Option<Vec<String>>,
    /// A user task delivered through the existing one-shot runner path.
    pub prompt: Option<String>,
    /// Caller-supplied display label, never authenticated identity.
    pub from: Option<String>,
    /// Caller-supplied pane UUID; displayed only when it names a live pane.
    pub requesting_pane: Option<Uuid>,
    /// Client behavior only; the host always returns admission immediately.
    #[serde(skip)]
    pub no_wait: bool,
}

impl NewOptions {
    /// Also checked by the server: callers may send JSON without using the CLI.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [("model", &self.model), ("effort", &self.effort)] {
            if let Some(value) = value
                && (value.is_empty()
                    || value.len() > 64
                    || value.starts_with('-')
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._:/@+-".contains(&b)))
            {
                return Err(format!(
                    "--{name} needs 1–64 characters from [A-Za-z0-9._:/@+-], without a leading '-'."
                ));
            }
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|text| text.trim().is_empty() || text.contains('\0'))
        {
            return Err("--prompt needs non-empty text without NUL".into());
        }
        if self.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return Err("--cwd must be an absolute directory".into());
        }
        if self.preset.is_some() && self.argv.is_some() {
            return Err("--preset and -- PROGRAM are mutually exclusive".into());
        }
        if self.preset.as_ref().is_some_and(|name| name.is_empty()) {
            return Err("--preset needs a preset name or id".into());
        }
        if let Some(argv) = &self.argv {
            if argv.first().is_none_or(|program| program.is_empty()) {
                return Err("-- needs a non-empty PROGRAM".into());
            }
            if argv.iter().any(|arg| arg.contains('\0')) {
                return Err("PROGRAM and ARGS cannot contain NUL".into());
            }
        }
        Ok(())
    }

    /// `default_index` is the toolbar selection, or the destination project's
    /// saved default when that project isn't currently selected.
    pub fn instance(
        &self,
        project: &Project,
        presets: &[AgentPreset],
        default_index: usize,
    ) -> Result<Instance, Error> {
        self.validate()?;
        let mut instance = if let Some(argv) = &self.argv {
            if self.model.is_some() || self.effort.is_some() {
                return Err(Error::new(
                    ErrorCode::ParameterUnsupported,
                    "--model and --effort require a preset; configure its Model flag / Effort flag in Settings > Agents instead of using -- PROGRAM",
                ));
            }
            if self.prompt.is_some() {
                return Err(creation_refusal(ErrorCode::PromptUnsupported));
            }
            let mut instance = Instance::shell(project.id);
            instance.title = argv[0].clone();
            instance.program = Some(argv[0].clone());
            instance.args = argv[1..].to_vec();
            instance.preset.clear();
            instance.preset_id = None;
            instance.literal_argv = true;
            instance
        } else {
            let mut preset = match &self.preset {
                Some(query) => {
                    let hits: Vec<_> = presets
                        .iter()
                        .filter(|preset| preset.name == *query || preset.id.to_string() == *query)
                        .collect();
                    match hits.as_slice() {
                        [preset] => (*preset).clone(),
                        [] => return Err(format!("no preset matches '{query}'").into()),
                        _ => {
                            return Err(format!(
                                "'{query}' matches more than one preset; use its id"
                            )
                            .into());
                        }
                    }
                }
                None => presets
                    .get(default_index)
                    .cloned()
                    .unwrap_or_else(AgentPreset::shell),
            };
            for (name, value, flag, target) in [
                ("Model", &self.model, &preset.model_flag, &mut preset.model),
                (
                    "Effort",
                    &self.effort,
                    &preset.effort_flag,
                    &mut preset.effort,
                ),
            ] {
                if let Some(value) = value {
                    let flag = flag.as_deref().filter(|flag| !flag.trim().is_empty());
                    if flag.is_none_or(|flag| parameter_in_args(&preset.args, flag)) {
                        return Err(Error::new(
                            ErrorCode::ParameterUnsupported,
                            format!(
                                "preset '{}' cannot override {name}; fill in its {name} flag in Settings > Agents and remove that flag from Extra args",
                                preset.name
                            ),
                        ));
                    }
                    *target = Some(value.clone());
                }
            }
            if preset.kind != PresetKind::Terminal {
                if self.prompt.is_some() {
                    return Err(creation_refusal(ErrorCode::PromptUnsupported));
                }
                return Err(
                    "`ctl new` opens terminal panes; browser presets are not supported yet".into(),
                );
            }
            if let Some(prompt) = &self.prompt {
                if !crate::readaloud::is_agent_program(preset.program.as_deref()) {
                    return Err(creation_refusal(ErrorCode::PromptUnsupported));
                }
                let mut instance = crate::Runner {
                    id: Uuid::new_v4(),
                    name: preset.name.clone(),
                    preset_id: Some(preset.id),
                    auto_mode_presses: 0,
                    prompt: "{{input}}".into(),
                }
                .build_instance(project.id, &preset, prompt);
                instance.control_prompt = true;
                instance
            } else {
                Instance::from_preset(project.id, &preset)
            }
        };
        instance.custom_name = self.name.clone();
        instance.launch_dir = Some(
            self.cwd
                .clone()
                .unwrap_or_else(|| project.root_path.clone()),
        );
        Ok(instance)
    }
}

pub(super) fn parse(args: &[String]) -> Result<CliCommand, String> {
    let Some((project, args)) = args
        .split_first()
        .filter(|(project, _)| !project.is_empty() && !project.starts_with("--"))
    else {
        return Err("`new` needs a PROJECT (an id or name from `muxel ctl projects`)".into());
    };
    let mut options = NewOptions::default();
    let mut placement_seen = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--" => {
                options.argv = Some(it.cloned().collect());
                break;
            }
            "--cwd" => options.cwd = Some(PathBuf::from(value()?)),
            "--name" => options.name = Some(value()?),
            "--preset" => options.preset = Some(value()?),
            "--model" => options.model = Some(value()?),
            "--effort" => options.effort = Some(value()?),
            "--prompt" => options.prompt = Some(value()?),
            "--from" => options.from = Some(value()?),
            "--no-wait" => options.no_wait = true,
            "--background" => options.background = true,
            "--tab" | "--split" => {
                if placement_seen {
                    return Err("choose either --tab or --split once".into());
                }
                placement_seen = true;
                options.placement = if arg == "--tab" {
                    NewPlacement::Tab
                } else {
                    match value()?.as_str() {
                        "right" => NewPlacement::Right,
                        "down" => NewPlacement::Down,
                        _ => return Err("--split must be right or down".into()),
                    }
                };
            }
            _ => return Err(format!("unexpected argument '{arg}'")),
        }
    }
    options.validate()?;
    Ok(CliCommand::Request(Command::New {
        project: project.clone(),
        options,
    }))
}

fn parameter_in_args(args: &[String], flag: &str) -> bool {
    fn config_value(arg: &str) -> Option<&str> {
        arg.strip_prefix("--config=")
            .or_else(|| arg.strip_prefix("--config "))
            .or_else(|| arg.strip_prefix("-c"))
            .map(str::trim_start)
    }
    if let Some(key) =
        config_value(flag).and_then(|value| value.split_once('=').map(|(key, _)| key))
    {
        return args.iter().enumerate().any(|(index, arg)| {
            let value = if arg == "-c" || arg == "--config" {
                args.get(index + 1).map(String::as_str)
            } else {
                config_value(arg)
            };
            value
                .and_then(|value| value.split_once('='))
                .is_some_and(|(existing, _)| existing.trim() == key)
        });
    }
    let matches = |arg: &str| {
        if let Some((prefix, suffix)) = flag.split_once("{}") {
            (arg.starts_with(prefix) && arg.ends_with(suffix))
                || prefix.strip_suffix('=').is_some_and(|flag| arg == flag)
        } else {
            arg == flag
                || arg
                    .strip_prefix(flag)
                    .is_some_and(|tail| tail.starts_with('='))
        }
    };
    args.iter().enumerate().any(|(index, arg)| {
        matches(arg)
            || flag.contains(' ')
                && args
                    .get(index + 1)
                    .is_some_and(|next| matches(&format!("{arg} {next}")))
    })
}

/// The common placement mutation, with optional selection preservation. A missing
/// target can seed an empty layout but must never replace a non-empty one.
pub fn place_new_pane(
    layout: &mut Option<PaneNode>,
    target: Option<Uuid>,
    iid: Uuid,
    direction: Option<SplitDirection>,
    background: bool,
) -> bool {
    if layout.is_none() {
        *layout = Some(PaneNode::leaf(iid));
        return true;
    }
    let Some(target) = target else { return false };
    match direction {
        Some(direction) => split(layout, target, direction, iid),
        None => {
            let previous = layout
                .as_ref()
                .and_then(|layout| layout.leaf_containing(target))
                .and_then(|leaf| match leaf {
                    PaneNode::Leaf(leaf) => leaf.tabs.get(leaf.active).copied(),
                    _ => None,
                });
            if !add_tab(layout, target, iid) {
                return false;
            }
            if background && let Some(previous) = previous {
                set_active_tab(layout, previous);
            }
            true
        }
    }
}
