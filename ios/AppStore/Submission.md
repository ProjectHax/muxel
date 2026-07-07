# muxel iOS — App Store submission guide

The checklist to get muxel from source to "Ready for Sale." Repo-side items are done
(privacy manifests, export key, background modes); the rest is signing + App Store
Connect setup you do once with your paid Apple Developer account.

## 1. Signing & identifiers (do this first)

The project uses **automatic signing** (`CODE_SIGN_STYLE: Automatic`). When you set
your team and archive, Xcode registers the App IDs for you. If you prefer to do it by
hand in the Developer portal, register these:

| Bundle ID | Target | Special capabilities to enable |
|---|---|---|
| `dev.muxel.ios` | app | **none** — BGAppRefreshTask and Live Activities are Info.plist-only, no App ID capability or entitlement needed yet |
| `dev.muxel.ios.widgets` | Live Activity widget extension | none |

Notes:
- **No App Group** is required — the Live Activity payload is passed in-process via
  ActivityKit `ContentState`, not a shared container. (Verified in code.)
- **No Push Notifications capability** yet — the Live Activity and notifications are
  driven locally by on-device polling. When the future APNs upgrade lands, add the
  Push Notifications capability + `aps-environment` entitlement then.
- The `Muxel.entitlements` file is intentionally empty — correct for v1.

### Team

`project.yml` sets `DEVELOPMENT_TEAM: "AT4THS99K7"` (**ProjectHax LLC**). A Team ID is
not secret — it ships inside every signed app — so committing it is fine. Contributors
without access to that team override it in Xcode (Signing & Capabilities → Team) or
locally; re-run `xcodegen generate` after any change.

## 2. Export compliance (encryption) — needs a decision + a filing

muxel bundles a full SSH client (SwiftNIO SSH via Citadel), so it uses **non-exempt
encryption**. `Info.plist` now declares `ITSAppUsesNonExemptEncryption = true`, which
is the honest answer for a bundled SSH stack (it's not limited to Apple's HTTPS/OS
crypto).

What you owe as a result — this is standard for every SSH/VPN app on the store:

1. **App Store Connect questionnaire** (asked once, or skipped because the plist key
   is set): the app uses only *standard* encryption algorithms (no proprietary
   crypto), so it qualifies for the **mass-market exemption** under ECCN 5D992 / License
   Exception ENC.
2. **Annual self-classification report to BIS.** Email the report (app name, ECCN
   5D992.c, a short description "SSH client using standard encryption") once per
   calendar year to `crypt@bis.doc.gov` **and** `enc@nsa.gov`. This is a one-time-per-
   year formality, not per-build. Apple's docs: "Complying with Encryption Export
   Regulations."
3. You do **not** need a CCATS / ERN or an `ITSEncryptionExportComplianceCode` for the
   self-classification path.

If you'd rather not take on the annual filing, the alternative is to argue the
encryption is exempt and set the key to `false` — but for an app whose whole purpose
is an encrypted SSH transport that is a weaker position. Recommend keeping `true` +
the annual report.

## 3. App Store Connect metadata (all required before you can submit)

- **Privacy Policy URL** — mandatory for every app. A hosted page works (e.g. GitHub
  Pages off this repo). Must state: SSH credentials are stored only in the on-device
  Keychain; no data is collected, tracked, or sent to the developer or third parties.
- **Support URL** — mandatory (the GitHub repo / issues page is fine).
- **App Privacy "nutrition label"** — answer **Data Not Collected**. The app has no
  analytics, no tracking, no third-party network; this matches the privacy manifests.
- **Screenshots** for every required device size (currently 6.9"/6.5" iPhone; **plus
  13" iPad if you keep iPad support — see §5**).
- **Description, keywords, category** — category: *Developer Tools* (or *Utilities*).
- **Age rating** questionnaire — a terminal that runs arbitrary remote commands often
  warrants "Unrestricted access"; answer honestly (may land 17+).
- **Notes for Review + demo host** — see `ReviewNotes.md`. **This is the top practical
  rejection risk.** Provide a live demo SSH host and paste the filled-in notes.

## 4. Already handled in the repo

- **Privacy manifests** — `Muxel/PrivacyInfo.xcprivacy` (declares no tracking, no data
  collection, and the two Required Reason APIs: UserDefaults `CA92.1`, system boot time
  `35F9.1`) and a minimal `MuxelWidgets/PrivacyInfo.xcprivacy`. After `xcodegen
  generate`, confirm each `.xcprivacy` is in its target's **Copy Bundle Resources**
  phase (XcodeGen adds it automatically; verify once).
- **Background modes** — trimmed to just `fetch` (the only mode actually used);
  the unused `processing` mode was removed (App Review 2.5.4).
- **Export key** — `ITSAppUsesNonExemptEncryption = true` (see §2).
- **App icon** — 1024×1024, RGB, no alpha (Apple's exact requirement).
- **GPLv3 ↔ App Store license conflict** — resolved via the §7 additional permission in
  `ios/LICENSE`.

## 5. Decide: keep iPad support?

`project.yml` sets `TARGETED_DEVICE_FAMILY: "1,2"` (iPhone **and** iPad). That promises
a working iPad experience *and* requires iPad screenshots, and reviewers will test it
on iPad. If the layout isn't tested/polished on iPad, either fix it or drop to
iPhone-only (`"1"`) in `project.yml` (both the `Muxel` and `MuxelWidgets` targets) to
shrink the review surface.

## 6. Build & upload

```sh
cd ios
xcodegen generate
# In Xcode: select your team, then Product → Archive → Distribute App → App Store Connect
```

Then in App Store Connect: attach the build, fill the metadata above, add the review
notes + demo host, and submit.
