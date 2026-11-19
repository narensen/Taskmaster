# Releasing Taskmaster

Push a tag that starts with `v`, and `.github/workflows/release.yml` builds and publishes
everything:

```bash
git tag v0.2.0
git push origin v0.2.0
```

| Job | What it publishes |
| --- | --- |
| `release` | Creates the GitHub release with generated notes. Edit the notes afterwards if you like: the app's Updates panel and the public Releases page show them. |
| `linux` | `taskmaster-<tag>-x86_64-linux.tar.gz` and its `.sha256`: tm-daemon, tm-review, tm-commons, the built web app, and `install.sh`. That script copies the app to `~/.local/share/taskmaster/web`, so `tm-daemon serve` finds it without `--web`. |
| `desktop` | The desktop app: `.dmg` and `.app.tar.gz` for Apple silicon and Intel Macs, `.AppImage` and `.deb` for Linux, `.msi` and `-setup.exe` for Windows, each with a `.sig` signature, and `latest.json`, the manifest the in-app updater reads. |
| `checksums` | `SHA256SUMS.txt` covering every file in the release. |

The version in the app comes from the tag (`v0.2.0` becomes `0.2.0`).

## Dry run

Run the workflow by hand (Actions > release > Run workflow) without pushing a tag. It builds
every desktop target the same way a real release does and keeps them as workflow artifacts for a
few days, but publishes nothing: no tag, no release, no `latest.json`. Choose the version and
the targets (all, mac, linux, windows). This is how a macOS or Windows fix is checked before a
tag is cut, and nothing is published until a `v*` tag is pushed.

## Secrets

Set these under Settings, Secrets and variables, Actions. None of them are ever committed.

| Secret | Needed for | Required |
| --- | --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Signing each installer for the updater | Yes |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The password of that key | Yes (empty if the key has none) |
| `TAURI_UPDATER_PUBKEY` | The matching public key, built into the app so it can check what it downloads | Yes |
| `APPLE_CERTIFICATE` | A Developer ID Application certificate, as base64 of the `.p12` | For a signed `.dmg` |
| `APPLE_CERTIFICATE_PASSWORD` | Its password | With the above |
| `APPLE_SIGNING_IDENTITY` | For example `Developer ID Application: Your Name (TEAMID)` | With the above |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | Notarization (`APPLE_PASSWORD` is an app-specific password) | For a notarized `.dmg` |
| `WINDOWS_CERTIFICATE` | A code-signing certificate, as base64 of the `.pfx` | For signed Windows installers |
| `WINDOWS_CERTIFICATE_PASSWORD` | Its password | With the above |

Without the Apple or Windows certificates the installers are still built and the updater
still checks its own signatures; macOS Gatekeeper and Windows SmartScreen will warn on
first open.

### Making the updater key, once

```bash
npx @tauri-apps/cli@2 signer generate -w ~/.tauri/taskmaster.key
```

It prints the public key and writes the private key to `~/.tauri/taskmaster.key`. Put the
file's contents in `TAURI_SIGNING_PRIVATE_KEY`, the password you chose in
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the public key in `TAURI_UPDATER_PUBKEY`. Keep a
copy of the private key somewhere safe: an app built with one public key only installs
updates signed by its private key.

## How an installed app updates

Settings, Updates, checks
`https://github.com/narensen/Taskmaster/releases/latest/download/latest.json` when it opens
and when you press Check for updates. When a newer version is listed it shows the version,
the date and the release notes. Install and restart downloads the installer for this
platform, checks its signature against the built-in public key, installs it, stops
tm-daemon and restarts.

## What is not finished

- **macOS and Windows signing.** Without the Apple or Windows certificates the installers are
  still built: macOS is ad-hoc signed (Gatekeeper warns on first open) and Windows gates the
  terminal off with a clear message instead of a ConPTY backend (SmartScreen warns until a
  certificate is added). Add `APPLE_CERTIFICATE` / `WINDOWS_CERTIFICATE` and the other secrets
  above to sign for real.
- The desktop shell (`desktop/src-tauri`) is outside the Cargo workspace, because it needs
  the system webview to compile. It is built only by this workflow. Locally, with the
  [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) installed, build it with:
- The desktop shell (`desktop/src-tauri`) is outside the Cargo workspace, because it needs
  the system webview to compile. It is built only by this workflow. Locally, with the
  [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) installed, build it with:

  ```bash
  cd web && npm ci && npm run build && cd ..
  cargo build --release -p tm-daemon
  cp target/release/tm-daemon desktop/src-tauri/binaries/tm-daemon-$(rustc -vV | sed -n 's/host: //p')
  npx @tauri-apps/cli@2 icon assets/icon.png -o desktop/src-tauri/icons
  cd desktop/src-tauri && npx @tauri-apps/cli@2 build
  ```
