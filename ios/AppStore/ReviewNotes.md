# App Review Notes — muxel iOS

muxel is **remote-only**: it does nothing without an SSH host that is running tmux
and has at least one muxel project. An App Review tester who opens the app with no
host configured will see an empty sidebar and reject it under **Guideline 2.1 (App
Completeness) — "we were unable to review your app."** You must give the reviewer a
live host to connect to. This is the single most likely rejection; do not skip it.

## What the reviewer needs

A **live, internet-reachable SSH host** that stays up for the entire review window
(and every resubmission), pre-seeded so the tester can see real panes/agents:

- A user account they can log into (password auth is simplest for review).
- `tmux` installed and working (avoid the stock AlmaLinux/RHEL 10 `tmux 3.3a` — it
  segfaults on `capture-pane`; use a distro build that works, e.g. Debian/Ubuntu).
- At least one muxel project directory with a `.muxel/workspace.json` and one or two
  running sessions (e.g. a shell and a long-running command) so panes, tabs, and
  status badges are populated. Easiest: run desktop muxel against that host once, or
  start a `tmux new-session -A -d -s muxel_<slug>_<uuid8> …` by hand, then add the
  project path in-app.
- Reachable directly (public IP / DNS). If it's behind a bastion, provide the jump
  host details too — but a direct host is far less likely to trip up the tester.

Keep the account low-privilege and disposable; rotate/tear it down after approval.

## Notes for Review (paste into App Store Connect → App Review Information)

> muxel is a companion app for the muxel desktop terminal multiplexer. It connects
> over SSH to a host the user already owns and attaches to their tmux sessions — it
> has no local terminal and no accounts of its own. To review it, use the demo SSH
> host below (it is pre-configured with a muxel project and live sessions).
>
> How to exercise the app:
> 1. Launch muxel and tap "Add host" in the sidebar.
> 2. Enter:  Host: <DEMO_HOST_OR_IP>   Port: 22   User: <DEMO_USER>
>    Auth: Password   Password: <DEMO_PASSWORD>
> 3. Tap "Test connection" (verifies the credential), then Save. On first connect the
>    app pins the host key (trust-on-first-use) — tap Trust.
> 4. The project "<DEMO_PROJECT>" appears. Tap it to see its panes/tabs. Tap a pane to
>    open the live terminal; type a command to confirm it's interactive.
> 5. Long-press a tab to rename / duplicate / close. Tap "+" to launch a new instance.
> 6. Status dots reflect each pane's state; background polling drives notifications and
>    the Lock Screen / Dynamic Island status bar while the app is minimized.
>
> The app stores SSH credentials only in the on-device Keychain; nothing is sent to us
> or any third party. Encryption is a standard SSH client (no proprietary crypto).
>
> Demo host:  <DEMO_HOST_OR_IP>
> User / password:  <DEMO_USER> / <DEMO_PASSWORD>
> (Jump host, if used:  <BASTION_HOST> — user <BASTION_USER>, password <BASTION_PASS>)

Fill in every `<...>` before submitting. Do not ship the file with placeholders.

## Gotchas that read as bugs to a reviewer

- **Notifications / background status are best-effort.** iOS throttles
  `BGAppRefreshTask` (often 15+ min, never under low power), so a reviewer won't see a
  push-like instant alert. That's expected and documented — don't claim instant
  background alerts in the description, or you invite a "doesn't work as advertised"
  rejection. Foreground status is immediate via the live terminal.
- **Real device recommended.** Keychain, background refresh, notifications, and the
  Metal terminal renderer don't all behave in the Simulator. The reviewer uses a real
  device, but test there yourself first.
- **Host must stay live.** If the demo host is down when the tester picks up the
  review (days later), it's an automatic reject. Keep it up until "Ready for Sale."
