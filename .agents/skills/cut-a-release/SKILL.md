---
name: cut-a-release
description: Cut a Taskmaster release. Use when tagging a version or changing the Linux tarball, install.sh, or the updater.
---

# Cut a release

Pushing a tag that starts with `v` runs `.github/workflows/release.yml`. The version in the app is the tag without the `v`.

The Linux job packs `tm-daemon`, `tm-review`, `tm-commons`, `README.md`, `config.example.json`, `deploy/`, `web/dist` as `web/`, and `scripts/install-release.sh` as `install.sh`.

`install.sh` copies the binaries to `~/.local/bin` (or `--prefix <dir>/bin`) and the app to `$XDG_DATA_HOME/taskmaster/web` or `~/.local/share/taskmaster/web`. `tm-daemon serve` finds that directory when `--web` is omitted, because `installed_web()` in `crates/tm-daemon/src/main.rs` requires `index.html` there. `scripts/install.sh` is the from-source builder. Do not replace it with the tarball script.

Before a tag, `docs/RELEASING.md` lists the signing secrets. Do not commit a private key. Do not push the tag unless the person asks.
