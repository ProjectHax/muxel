//! Launch-local input cancellation and bounded OSC 133 shell evidence.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
pub(super) struct InputGate(AtomicU64);

impl InputGate {
    pub fn epoch(&self) -> u64 {
        self.0.load(Ordering::Acquire) >> 1
    }

    pub fn accepts(&self, epoch: u64) -> bool {
        self.0.load(Ordering::Acquire) == epoch << 1
    }

    pub fn hold(&self) {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(state.wrapping_add(2) | 1)
            })
            .expect("input gate update always succeeds");
    }

    pub fn resume(&self) {
        self.0.fetch_and(!1, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(super) struct ShellPrompt(AtomicBool);

impl ShellPrompt {
    pub fn at_prompt(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn submitted(&self, bytes: &[u8]) {
        if bytes.iter().any(|&byte| matches!(byte, b'\r' | b'\n')) {
            self.0.store(false, Ordering::Release);
        }
    }
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc {
        prefix: [u8; 6],
        len: usize,
        escaped: bool,
    },
    String {
        escaped: bool,
    },
}

/// Retains at most six bytes, regardless of the size of an OSC payload.
#[derive(Default)]
pub(super) struct ShellMarks {
    state: State,
}

impl ShellMarks {
    pub fn advance(&mut self, bytes: &[u8], prompt: &ShellPrompt) {
        for &byte in bytes {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                State::Ground => {
                    if byte == 0x1b {
                        State::Escape
                    } else {
                        State::Ground
                    }
                }
                State::Escape => Self::escape(byte),
                State::String { escaped } => {
                    if matches!(byte, 0x18 | 0x1a) || (escaped && byte == b'\\') {
                        State::Ground
                    } else {
                        State::String {
                            escaped: byte == 0x1b,
                        }
                    }
                }
                State::Osc {
                    mut prefix,
                    mut len,
                    escaped,
                } => {
                    if matches!(byte, 0x18 | 0x1a) {
                        State::Ground
                    } else if (!escaped && byte == 0x07) || (escaped && byte == b'\\') {
                        if len >= 5 && &prefix[..4] == b"133;" {
                            let valid = len == 5 || prefix[5] == b';';
                            prompt.0.store(
                                valid && matches!(prefix[4], b'A' | b'B'),
                                Ordering::Release,
                            );
                        }
                        State::Ground
                    } else if escaped {
                        Self::escape(byte)
                    } else if byte == 0x1b {
                        State::Osc {
                            prefix,
                            len,
                            escaped: true,
                        }
                    } else {
                        if len < prefix.len() {
                            prefix[len] = byte;
                            len += 1;
                        }
                        State::Osc {
                            prefix,
                            len,
                            escaped: false,
                        }
                    }
                }
            };
        }
    }

    fn escape(byte: u8) -> State {
        match byte {
            b']' => State::Osc {
                prefix: [0; 6],
                len: 0,
                escaped: false,
            },
            b'P' | b'X' | b'^' | b'_' => State::String { escaped: false },
            0x1b => State::Escape,
            // C0 controls execute without leaving VTE's Escape state.
            0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff => State::Escape,
            _ => State::Ground,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InputGate, ShellMarks, ShellPrompt};

    #[test]
    fn hold_changes_the_epoch_and_resume_only_clears_the_hold() {
        let gate = InputGate::default();
        let queued = gate.epoch();
        assert!(gate.accepts(queued));
        gate.hold();
        assert!(!gate.accepts(queued));
        assert!(!gate.accepts(gate.epoch()));
        gate.resume();
        assert!(!gate.accepts(queued));
        assert!(gate.accepts(gate.epoch()));
    }

    #[test]
    fn windows_shell_without_osc133_asks() {
        let prompt = ShellPrompt::default();
        let mut marks = ShellMarks::default();
        marks.advance(b"PS C:\\> ", &prompt);
        assert!(!prompt.at_prompt());
    }

    #[test]
    fn shell_marks_survive_every_read_split_and_both_terminators() {
        for bytes in [b"\x1b]133;A\x07".as_slice(), b"\x1b]133;B;aid=1\x1b\\"] {
            for split in 0..=bytes.len() {
                let prompt = ShellPrompt::default();
                let mut marks = ShellMarks::default();
                marks.advance(&bytes[..split], &prompt);
                marks.advance(&bytes[split..], &prompt);
                assert!(prompt.at_prompt(), "split {split}");
                prompt.submitted(b"command\r");
                assert!(!prompt.at_prompt());
                marks.advance(bytes, &prompt);
                assert!(prompt.at_prompt());
                marks.advance(b"\x1b]133;C\x07", &prompt);
                assert!(!prompt.at_prompt());
                marks.advance(bytes, &prompt);
                marks.advance(b"\x1b]133;D;0\x07", &prompt);
                assert!(!prompt.at_prompt());
            }
        }
    }

    #[test]
    fn shell_marks_reject_lookalikes_cancellation_and_nested_strings() {
        for bytes in [
            b"\x1b]133;Afoo\x07".as_slice(),
            b"\x1b]133;Z\x07",
            b"\x1b]133;A\x18\x07",
            b"\x1b]133;A\x1a\x07",
            b"\x1b]133;A\x1b\x07",
            b"\x1b]1333;A\x07",
            b"\x1b]2;133;A\x07",
            b"133;A",
            b"\x1bP\x1b]133;A\x07\x1b\\",
            b"\x1b_\x1b]133;A\x07\x1b\\",
        ] {
            let prompt = ShellPrompt::default();
            let mut marks = ShellMarks::default();
            marks.advance(bytes, &prompt);
            assert!(!prompt.at_prompt(), "{bytes:?}");
        }
    }

    #[test]
    fn shell_prompt_observation_ignores_paint_synchronization_and_bounds_payloads() {
        let prompt = ShellPrompt::default();
        let mut marks = ShellMarks::default();
        marks.advance(b"\x1b[?2026h\x1b]133;A\x07", &prompt);
        assert!(prompt.at_prompt());
        prompt.submitted(b"draft");
        assert!(prompt.at_prompt());
        marks.advance(b"\x1b]133;C;", &prompt);
        for _ in 0..100_000 {
            marks.advance(b"x", &prompt);
        }
        marks.advance(b"\x07", &prompt);
        assert!(!prompt.at_prompt());
        marks.advance(b"\x1b]133;B\x1b\\\x1b[?2026l", &prompt);
        assert!(prompt.at_prompt());
        assert!(
            !ShellPrompt::default().at_prompt(),
            "a new launch has no shell evidence"
        );
    }
}
