//! Sound alerts in the app: ringing the chime the status tick's `AlertGate`
//! lets through, and Settings → Sounds (on/off per alert, the user's own sound
//! files, volume). Decoding and playback live in `crate::sound`; when to ring
//! lives in `muxel_core::sound_alert`.

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

    /// Settings → Sounds "Play". A custom file is decoded first so one that
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

    pub(super) fn render_settings_sounds(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = &self.settings;
        let muted = cx.theme().muted_foreground;
        let note = |text: SharedString| div().text_xs().text_color(muted).child(text);
        let volume = i32::from(settings.sound_volume);

        v_flex()
            .gap_3()
            .max_w(px(560.0))
            .child(note(t(
                "muxel chimes when an agent finishes, or needs you, in a pane you aren't looking at. Only agents ring — never shells.",
            )))
            .child(self.setting_check(
                "snd-done",
                settings.sound_on_done,
                &t("Chime when an agent finishes"),
                cx,
                |s, on| s.sound_on_done = on,
            ))
            .child(self.alert_sound_row(AlertSound::Done, cx))
            .child(self.setting_check(
                "snd-blocked",
                settings.sound_on_blocked,
                &t("Chime when an agent needs your input"),
                cx,
                |s, on| s.sound_on_blocked = on,
            ))
            .child(self.alert_sound_row(AlertSound::Blocked, cx))
            .child(self.settings_label(&t("Volume"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("snd-vol-dec")
                            .ghost()
                            .label("−")
                            .disabled(volume == 0)
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.set_alert_volume(volume - 10, cx)
                            })),
                    )
                    .child(div().w(rems(4.0)).text_center().child(format!("{volume}%")))
                    .child(
                        Button::new("snd-vol-inc")
                            .ghost()
                            .label("+")
                            .disabled(volume == 100)
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.set_alert_volume(volume + 10, cx)
                            })),
                    )
                    .child(
                        Button::new("snd-vol-reset")
                            .ghost()
                            .small()
                            .label(t("Default"))
                            .disabled(settings.sound_volume == DEFAULT_VOLUME)
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.set_alert_volume(i32::from(DEFAULT_VOLUME), cx)
                            })),
                    ),
            )
            .child(note(t(
                "A pane chimes once, then stays quiet for that alert until you've looked at it (or five minutes pass), so an agent going back and forth between working and done doesn't keep ringing. Alerts that land together play as one chime.",
            )))
            .child(note(t(
                "Your own sound can be WAV, AIFF, MP3, OGG, FLAC or M4A. Anything longer than 10 seconds is cut short.",
            )))
            .into_any_element()
    }

    /// One alert's sound: what plays, with Play / Choose / Use built-in.
    fn alert_sound_row(&self, sound: AlertSound, cx: &mut Context<Self>) -> impl IntoElement {
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
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        v_flex()
            .pl(px(28.0))
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("snd-{id}-play")))
                            .ghost()
                            .small()
                            .icon(Icon::empty().path("icons/volume-2.svg"))
                            .tooltip(t("Play"))
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.preview_alert(sound, cx)),
                            ),
                    )
                    .child(div().max_w(px(240.0)).truncate().text_sm().child(name))
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
                    .text_color(muted)
                    .child(t("File not found — the built-in chime plays instead."))
            }))
            .children(error.map(|e| div().text_xs().text_color(danger).child(e)))
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
