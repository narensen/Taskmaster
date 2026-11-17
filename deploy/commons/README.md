# Running the Commons

`tm-commons` is one service that every Taskmaster shares. It does three things:

- signs people in with GitHub or Google;
- serves the Commons;
- holds the weekly review.

It runs on a small Linux VPS behind Caddy, and keeps its data in Neon.

## What you need

| What | Where it comes from | Goes in |
| --- | --- | --- |
| A domain name, such as `taskmaster-commons.duckdns.org` | Your registrar: an A record pointing at the VPS | `TM_COMMONS_URL`, the Caddyfile |
| A Postgres connection string | Neon: the project's Connect dialog, the pooled string; optionally also the direct string as `DATABASE_URL_DIRECT` (used only to create the tables) | `DATABASE_URL` |
| A Google OAuth client ID and secret | Google Cloud console, see below | `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` |
| A GitHub OAuth app client ID and secret | GitHub, see below | `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET` |
| The moderators | You | `TM_COMMONS_MODERATORS` |
| A signing key and a session secret | `tm-commons keygen` | `TM_COMMONS_SIGNING_KEY`, `TM_COMMONS_SESSION_SECRET` |

One provider is enough to start. Leave the other one's values as placeholders.

**Google.** In the Google Cloud console:
1. Go to APIs & Services, then OAuth consent screen. Choose External, name the app Taskmaster, and add the scopes `openid`, `email` and `profile`.
2. Publish the app, or add yourself as a test user.
3. Go to Credentials, then Create credentials, then OAuth client ID, and choose Web application.
4. Set the authorized redirect URI to `https://taskmaster-commons.duckdns.org/auth/google/callback`.

**GitHub.** Go to Settings, then Developer settings, then OAuth Apps, then New OAuth App.
1. Set the homepage to `https://taskmaster-commons.duckdns.org`.
2. Set the authorization callback URL to `https://taskmaster-commons.duckdns.org/auth/github/callback`.
3. Generate a client secret.

The service asks GitHub only for `read:user` and `user:email`: who you are, and your verified email. It asks for nothing in anyone's repositories.

**Neon.** At runtime the service uses the pooled connection string (`DATABASE_URL`). The direct string (`DATABASE_URL_DIRECT`) is used only to create the tables at start. It creates its tables on first start.

## On the VPS

```bash
# Caddy: https://caddyserver.com/docs/install
sudo useradd --system --no-create-home --shell /usr/sbin/nologin tm-commons

# The binary: from a release, or `cargo build --release -p tm-commons` and copy it over.
sudo install -m 755 tm-commons /usr/local/bin/

sudo mkdir -p /etc/tm-commons
sudo install -m 640 -o root -g tm-commons commons.env.example /etc/tm-commons/commons.env
tm-commons keygen            # paste both lines into the file
sudoedit /etc/tm-commons/commons.env

# Check it before starting it: the settings, Neon, and the tables.
sudo -u tm-commons bash -c 'set -a; . /etc/tm-commons/commons.env; tm-commons check'

sudo install -m 644 tm-commons.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now tm-commons
sudo cp Caddyfile /etc/caddy/Caddyfile   # with your domain in it
sudo systemctl reload caddy
```

Open the address and sign in. A moderator sees Moderate in the menu.

## Deploy with the script

```bash
deploy/commons/deploy.sh --dry-run   # first: shows what it would do
deploy/commons/deploy.sh             # then for real
```

Flags: `--env-file` (the local file holding the secrets), `--commons-env` (where to write the generated `commons.env`), `--binary` (the `tm-commons` binary to ship), `--duckdns` (DuckDNS settings for the domain).

Secrets are read from the local env file and written to a `chmod 600` `commons.env`. That file is copied with `scp` and installed on the VM as `/etc/tm-commons/commons.env`, mode 600.

## Firewall

On the VM, Oracle Linux:

```bash
sudo firewall-cmd --permanent --add-service=http --add-service=https && sudo firewall-cmd --reload
```

Ubuntu images use iptables:

```bash
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 80 -j ACCEPT
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 443 -j ACCEPT
sudo netfilter-persistent save
```

In the OCI console, add ingress rules to the subnet's security list for TCP 80 and 443 from `0.0.0.0/0`. Port 8740 (and SearXNG's 8888) stay localhost-only and are never opened.

## OAuth callback URLs

- Google authorized redirect URI: `https://taskmaster-commons.duckdns.org/auth/google/callback`
- GitHub homepage: `https://taskmaster-commons.duckdns.org`
- GitHub callback: `https://taskmaster-commons.duckdns.org/auth/github/callback`

## The weekly review

`/moderate` lists three things:
- **Entries waiting for review,** oldest first, with a warning on anything that looks like a credential.
- **Open reports.**
- **Served entries that fail more than they pass** in two or more people's runs.

What you can do with them:
- **Approve** an entry, and every Taskmaster gets it on its next run.
- **Reject or withdraw** an entry, and every Taskmaster that already has it deletes it on its next pull.
- **Ban** someone. Their tokens stop working, and everything they shared is withdrawn.

## Connecting a Taskmaster

1. Sign in, and make a token on your account page.
2. The page shows the lines to paste into `.taskmaster/config.json`, or into `~/.taskmaster/config.json` for every project.

```json
{
  "commons": { "remote": "https://taskmaster-commons.duckdns.org", "token": "tmc_…", "share": true },
  "auth": { "issuer": "https://taskmaster-commons.duckdns.org", "allow": ["github:you"] }
}
```

- `share` sends the entries your runs promote for review. It is off unless you turn it on.
- `report` is on by default. It sends which Commons entry a page used and whether the page passed, and nothing else.
- `auth` lets you sign in to your own Taskmaster with the same account instead of a pairing code. Only the accounts in `allow` get in.

## Keys

- **`TM_COMMONS_SIGNING_KEY`** signs every sign-in token, so keep it secret. If it changes, every Taskmaster has to sign in again; they pick up the new public key within the hour.
- **`TM_COMMONS_SESSION_SECRET`** signs browser sessions. If it changes, every browser has to sign in again.
- **Taskmaster tokens** (`tmc_…`) are stored only as hashes. You can revoke one from the account page.

## Web search (SearXNG)

The deploy also installs a self-hosted [SearXNG](https://github.com/searxng/searxng), a free metasearch engine that needs no API keys. Skip it with `deploy.sh --no-searxng`.

- It runs as the `searxng` systemd unit (podman or docker, whichever the VM has) and listens only on `http://127.0.0.1:8888`. It is not behind Caddy and not public.
- JSON output is enabled. The engines are duckduckgo, brave, bing, google and wikipedia.
- `settings.yml` in this repo holds a placeholder. The real `secret_key` is generated on the VM with `openssl rand -hex 32` and never leaves it.
- To use it from a laptop: `ssh -L 8888:127.0.0.1:8888 opc@130.210.90.199`, then open http://127.0.0.1:8888.
- Taskmaster's default search endpoint is `http://127.0.0.1:8888/search?format=json&q={q}`.
