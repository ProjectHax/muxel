//! Sound alerts in the app: ringing the chime the status tick's `AlertGate`
//! lets through, and Settings → Notifications (on/off per alert, the user's own sound
//! files, volume). Decoding and playback live in `crate::sound`; when to ring
//! lives in `muxel_core::sound_alert`.

use super::settings_kit::{Block, Click, Form, Group, Row, Seg};
use super::*;
use crate::sound::{self, Clip};
use muxel_core::sound_alert::{DEFAULT_VOLUME, volume_gain};
use std::sync::atomic::{AtomicBool, Ordering};

impl MuxelApp {
    /// Ring `sound` with the user's file (falling back to the built-in chime)
    /// and volume.
    pub(super) fn ring_alert(&mut self, sound: AlertSound) {
        let gain = volume_gain(self.settings.sound_volume);
        if gain <= 0.0 {
            return;
        }
        let file = sound.file(&self.settings).map(Path::to_path_buf);
        sound::ring(sound, file, gain, self.next_alert_playback());
    }

    /// Stop the chime still playing, if any, and hand out the next one's stop flag.
    fn next_alert_playback(&mut self) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(previous) = self.alert_playback.replace(stop.clone()) {
            previous.store(true, Ordering::Relaxed);
        }
        stop
    }

    /// Settings → Notifications "Play". A custom file is decoded first so one that
    /// won't play says why here; a real alert would quietly use the built-in.
    fn preview_alert(&mut self, sound: AlertSound, cx: &mut Context<Self>) {
        match sound.file(&self.settings).map(Path::to_path_buf) {
            Some(path) => self.load_alert_file(sound, path, false, cx),
            None => {
                let gain = volume_gain(self.settings.sound_volume);
                sound::play(Clip::chime(sound), gain, self.next_alert_playback());
            }
        }
    }

    /// Pick an audio file for `sound` via the OS file dialog.
    fn browse_alert_sound(
        &mut self,
        sound: AlertSound,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("Choose sound")),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = receiver.await
                && let Some(path) = paths.pop()
            {
                let _ = this.update(cx, |this, cx| this.load_alert_file(sound, path, true, cx));
            }
        })
        .detach();
    }

    /// Decode `path` off the UI thread. If it plays, play it — and when `adopt`,
    /// make it `sound`'s alert. If it doesn't, say why under that alert.
    fn load_alert_file(
        &mut self,
        sound: AlertSound,
        path: PathBuf,
        adopt: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { sound::decode(&path) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match decoded {
                    Ok(clip) => {
                        this.alert_sound_error = None;
                        if adopt {
                            set_alert_file(&mut this.settings, sound, Some(path));
                            this.persist_settings();
                        }
                        let gain = volume_gain(this.settings.sound_volume);
                        sound::play(clip, gain, this.next_alert_playback());
                    }
                    Err(e) => {
                        this.alert_sound_error = Some((
                            sound,
                            tf(
                                "Couldn't play {file}: {err}",
                                &[("file", &file_label(&path)), ("err", &format!("{e:#}"))],
                            )
                            .to_string(),
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn use_builtin_alert_sound(&mut self, sound: AlertSound, cx: &mut Context<Self>) {
        set_alert_file(&mut self.settings, sound, None);
        if self
            .alert_sound_error
            .as_ref()
            .is_some_and(|(s, _)| *s == sound)
        {
            self.alert_sound_error = None;
        }
        self.persist_settings();
        cx.notify();
    }

    /// Step the alert volume by `delta` percent and play the Done chime at it.
    fn set_alert_volume(&mut self, percent: i32, cx: &mut Context<Self>) {
        self.settings.sound_volume = percent.clamp(0, 100) as u8;
        self.persist_settings();
        self.ring_alert(AlertSound::Done);
        cx.notify();
    }

    /// Settings → Notifications: desktop notifications, then the chimes.
    pub(super) fn notification_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        let settings = &self.settings;
        let volume = i32::from(settings.sound_volume);
        let always = settings.done_chime_always();
        let when_seg = |value: &'static str, label: SharedString, selected: bool| {
            Seg::new(
                label,
                selected,
                cx.listener(move |this, _e, _w, cx| {
                    this.settings.sound_done_when = value.to_string();
                    this.persist_settings();
                    cx.notify();
                }),
            )
            .disabled(!settings.sound_on_done)
        };
        let done_when = form.segmented(
            "snd-done-when",
            vec![
                when_seg(
                    muxel_core::DONE_CHIME_BACKGROUND,
                    t("Background panes"),
                    !always,
                ),
                when_seg(muxel_core::DONE_CHIME_ALWAYS, t("Every pane"), always),
            ],
        );
        let step = |percent: i32| -> Click {
            Box::new(cx.listener(move |this, _e, _w, cx| this.set_alert_volume(percent, cx)))
        };
        let volume_control = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("snd-vol-reset")
                    .ghost()
                    .small()
                    .label(t("Default"))
                    .disabled(settings.sound_volume == DEFAULT_VOLUME)
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.set_alert_volume(i32::from(DEFAULT_VOLUME), cx)
                    })),
            )
            .child(form.stepper(
                "snd-vol",
                format!("{volume}%"),
                (volume > 0).then(|| step(volume - 10)),
                (volume < 100).then(|| step(volume + 10)),
            ));

        vec![
            Block::Group(Group::new(
                t("Desktop"),
                vec![
                    Row::narrow(
                        t("Desktop notifications"),
                        form.switch(
                            "b-notif",
                            self.notifications_enabled,
                            true,
                            cx.listener(|this, on: &bool, _w, cx| {
                                this.notifications_enabled = *on;
                                this.persist_settings();
                                cx.notify();
                            }),
                        ),
                    )
                    .desc(t("When an agent finishes a turn or needs your attention.")),
                    Row::new(
                        t("Check that they arrive"),
                        Button::new("test-notif")
                            .outline()
                            .small()
                            .icon(IconName::Bell)
                            .label(t("Send a test notification"))
                            .on_click(cx.listener(|_this, _e, _w, _cx| {
                                notify(
                                    t("muxel test notification").to_string(),
                                    t("If you can see this, notifications are working.")
                                        .to_string(),
                                    None,
                                );
                            })),
                    )
                    .desc(t("Your desktop decides whether muxel's notifications are shown.")),
                ],
            )),
            Block::Group(Group::new(
                t("Chimes"),
                vec![
                    Row::narrow(
                        t("Chime when an agent finishes"),
                        self.setting_switch(form, "snd-done", settings.sound_on_done, cx, |s, on| {
                            s.sound_on_done = on
                        }),
                    )
                    .desc(t("Only agents ring, never shells."))
                    .keywords("sound alert"),
                    Row::stacked(
                        t("Finish sound"),
                        self.alert_sound_control(AlertSound::Done, form, cx),
                    ),
                    Row::new(t("Chime for"), done_when).desc(t(
                        "Background panes are any pane but the one you're focused on while muxel is the active window. Every pane also chimes when the agent you're watching finishes.",
                    )),
                    Row::narrow(
                        t("Chime when an agent needs your input"),
                        self.setting_switch(
                            form,
                            "snd-blocked",
                            settings.sound_on_blocked,
                            cx,
                            |s, on| s.sound_on_blocked = on,
                        ),
                    )
                    .keywords("sound alert blocked"),
                    Row::stacked(
                        t("Input sound"),
                        self.alert_sound_control(AlertSound::Blocked, form, cx),
                    ),
                    Row::new(t("Volume"), volume_control).desc(t(
                        "A pane chimes once, then stays quiet for that alert until you've looked at it (or five minutes pass), so an agent going back and forth between working and waiting doesn't ring over and over.",
                    )),
                ],
            )),
            Block::Note(t(
                "Your own sound can be WAV, AIFF, MP3, OGG, FLAC or M4A. Anything longer than 10 seconds is cut short.",
            )),
        ]
    }

    /// One alert's sound: what plays, with Play / Choose / Use built-in.
    fn alert_sound_control(
        &self,
        sound: AlertSound,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = match sound {
            AlertSound::Done => "done",
            AlertSound::Blocked => "blocked",
        };
        let file = sound.file(&self.settings);
        let name: SharedString = match file {
            Some(path) => file_label(path).into(),
            None => t("Built-in chime"),
        };
        let missing = file.is_some_and(|path| !path.exists());
        let error = self
            .alert_sound_error
            .as_ref()
            .filter(|(s, _)| *s == sound)
            .map(|(_, e)| e.clone());
        let pal = form.pal;
        v_flex()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("snd-{id}-play")))
                            .outline()
                            .small()
                            .icon(Icon::empty().path("icons/volume-2.svg"))
                            .tooltip(t("Play"))
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.preview_alert(sound, cx)),
                            ),
                    )
                    .child(
                        div()
                            .max_w(form.rem * 15.0)
                            .truncate()
                            .text_sm()
                            .child(name),
                    )
                    .child(
                        Button::new(SharedString::from(format!("snd-{id}-choose")))
                            .ghost()
                            .small()
                            .label(t("Choose sound…"))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.browse_alert_sound(sound, window, cx)
                            })),
                    )
                    .children(file.is_some().then(|| {
                        Button::new(SharedString::from(format!("snd-{id}-builtin")))
                            .ghost()
                            .small()
                            .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                            .label(t("Use built-in"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.use_builtin_alert_sound(sound, cx)
                            }))
                    })),
            )
            .children(missing.then(|| {
                div()
                    .text_xs()
                    .text_color(pal.dim)
                    .child(t("File not found — the built-in chime plays instead."))
            }))
            .children(error.map(|e| div().text_xs().text_color(pal.danger).child(e)))
            .into_any_element()
    }
}

fn set_alert_file(settings: &mut muxel_core::Settings, sound: AlertSound, file: Option<PathBuf>) {
    match sound {
        AlertSound::Done => settings.sound_done_file = file,
        AlertSound::Blocked => settings.sound_blocked_file = file,
    }
}

/// A sound file as the settings show it: its name, not the whole path.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
