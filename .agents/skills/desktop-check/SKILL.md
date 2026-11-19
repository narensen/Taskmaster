---
name: desktop-check
description: Check the Taskmaster desktop shell. Use when a change touches desktop/src-tauri, the updater, or the version shown in Settings.
---

# Desktop check

The shell is `desktop/src-tauri`. It is outside the Cargo workspace because it needs the system webview. There is no React Native app.

Local build, with the Tauri prerequisites installed:

```bash
cd web && npm ci && npm run build && cd ..
cargo build --release -p tm-daemon
cp target/release/tm-daemon desktop/src-tauri/binaries/tm-daemon-$(rustc -vV | sed -n 's/host: //p')
npx @tauri-apps/cli@2 icon assets/icon.png -o desktop/src-tauri/icons
cd desktop/src-tauri && npx @tauri-apps/cli@2 build
```

Settings, Updates, reads the app version from the shell and checks `latest.json` on the GitHub release. An update is installed only after its signature matches the key built into the app. The web e2e stubs `__TAURI__` for that panel. See `docs/RELEASING.md` for the signing secrets.

Windows is not a supported daemon target yet. The release workflow still runs that job and lets it fail.
