# The public site

`site/` builds the landing page (`/`) and the releases page (`/releases/`) into
`site/dist/`: plain HTML, one stylesheet, and a few lines of script (the theme switch, and
putting the download for the visitor's computer first). It is served as static files by
the Caddy that already runs the Commons on the Oracle VM.

## Build

```bash
cd site
node build.mjs            # reads releases from the GitHub API (cached 30 minutes in .cache/)
node build.mjs --offline  # the cached list only
npm test                  # the generator's tests
```

Releases are read at build time, never by a visitor's browser, so page views spend no API
calls. Set `GITHUB_TOKEN` for a higher hourly limit (and to read a private repository). The
benchmark table is built from `bench/results.jsonl`: only measured runs appear, each with
its label; with none, the section says no benchmark is published yet.

To refresh the releases page after each release, run the build again (a cron entry or a
systemd timer on any machine with Node and SSH access works; see below for the copy).

## Deploy (not done automatically)

1. Point a DNS name at the VM, for example `taskmaster.duckdns.org`.
2. Put the site's Caddy block in place. Either run the Commons deploy with the domain:

   ```bash
   SITE_DOMAIN=taskmaster.duckdns.org deploy/commons/deploy.sh
   ```

   which appends `deploy/site/Caddyfile` (with the domain filled in) to the Caddyfile it
   installs, or add that block to `/etc/caddy/Caddyfile` yourself and run
   `sudo systemctl reload caddy`. Without `SITE_DOMAIN`, the Commons deploy is unchanged.
3. Copy the built site:

   ```bash
   deploy/site/deploy.sh --dry-run   # shows the commands
   deploy/site/deploy.sh             # builds, then rsyncs site/dist/ to /srv/taskmaster-site
   ```

The block serves `/srv/taskmaster-site` with compression, short caching for pages and a
day for assets, and a Content-Security-Policy that allows only the site's own files and its
one inline script (by hash). If you change that script in `build.mjs`, update the hash in
`deploy/site/Caddyfile`.
