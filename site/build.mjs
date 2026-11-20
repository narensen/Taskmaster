// Builds the public site into dist/: the landing page and /releases/.
//
//   node build.mjs             fetch releases from GitHub (cached for 30 minutes in .cache/)
//   node build.mjs --offline   use the cache only
//   TASKMASTER_RELEASES_FILE=list.json node build.mjs   a saved list instead (previews, tests)
//
// Releases are read here, at build time, never by visitors: a page view costs no API call.
// Set GITHUB_TOKEN to raise the API's hourly limit. Benchmark numbers come only from the
// measured results in ../bench/ (results.jsonl and evals/*.json); with none, the section
// says so instead of showing a number.

import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
export const REPO = process.env.TASKMASTER_REPO ?? "narensen/Taskmaster";
const CACHE_TTL_MS = 30 * 60 * 1000;
const GITHUB = `https://github.com/${REPO}`;

export function escape(text) {
  return String(text ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

/** What a release file is for. `os` is matched against the visitor's computer. */
export function classify(name) {
  const n = name.toLowerCase();
  if (n.endsWith(".sig") || n === "latest.json" || n.endsWith(".sha256") || n === "sha256sums.txt") return null;
  if (n.endsWith(".dmg")) return n.includes("aarch64") || n.includes("arm64") ? { os: "mac-arm", what: "macOS, Apple silicon", order: 1 } : { os: "mac-intel", what: "macOS, Intel", order: 2 };
  if (n.endsWith(".app.tar.gz")) return { os: "mac-update", what: "macOS app archive (for the updater)", order: 9 };
  if (n.endsWith(".msi")) return { os: "windows", what: "Windows installer (.msi)", order: 3 };
  if (n.endsWith("-setup.exe") || n.endsWith(".exe")) return { os: "windows", what: "Windows installer (.exe)", order: 4 };
  if (n.endsWith(".appimage")) return { os: "linux", what: "Linux, AppImage", order: 5 };
  if (n.endsWith(".deb")) return { os: "linux-deb", what: "Linux, Debian and Ubuntu (.deb)", order: 6 };
  if (n.endsWith(".rpm")) return { os: "linux-rpm", what: "Linux, Fedora (.rpm)", order: 7 };
  if (n.endsWith(".tar.gz") && n.includes("linux")) return { os: "linux-server", what: "Linux server, tm-daemon and the app (.tar.gz)", order: 8 };
  return { os: "other", what: name, order: 10 };
}

/** A small, safe Markdown subset for release notes: headings, lists, links, code, paragraphs. */
export function markdown(text) {
  const inline = (s) =>
    escape(s)
      .replace(/`([^`]+)`/g, "<code>$1</code>")
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, (_, label, url) => `<a href="${url}">${label}</a>`)
      .replace(/(^|\s)(https?:\/\/[^\s<]+)/g, (_, pre, url) => `${pre}<a href="${url}">${url.replace(/^https:\/\/github\.com\//, "")}</a>`);
  const out = [];
  let list = false;
  // A table: a `| a | b |` header row, a `|---|` rule, then rows. Cells are inline text.
  let table = null;
  const cells = (line) => line.replace(/^\s*\|/, "").replace(/\|\s*$/, "").split("|").map((c) => c.trim());
  const endTable = () => {
    if (!table) return;
    const head = `<thead><tr>${table.head.map((c) => `<th>${inline(c)}</th>`).join("")}</tr></thead>`;
    const body = table.rows.map((r) => `<tr>${r.map((c) => `<td>${inline(c)}</td>`).join("")}</tr>`).join("");
    out.push(`<table>${head}<tbody>${body}</tbody></table>`);
    table = null;
  };
  const lines = String(text ?? "").replace(/\r/g, "").split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trimEnd();
    if (/^\s*\|.*\|\s*$/.test(line)) {
      if (table) {
        table.rows.push(cells(line));
        continue;
      }
      if (/^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1] ?? "")) {
        if (list) out.push("</ul>"), (list = false);
        table = { head: cells(line), rows: [] };
        i++;
        continue;
      }
    }
    endTable();
    const item = /^\s*[-*] (.*)$/.exec(line);
    if (item) {
      if (!list) out.push("<ul>"), (list = true);
      out.push(`<li>${inline(item[1])}</li>`);
      continue;
    }
    if (list) out.push("</ul>"), (list = false);
    const heading = /^(#{1,4}) (.*)$/.exec(line);
    if (heading) out.push(`<h${Math.min(4, heading[1].length + 2)}>${inline(heading[2])}</h${Math.min(4, heading[1].length + 2)}>`);
    else if (line.trim()) out.push(`<p>${inline(line)}</p>`);
  }
  endTable();
  if (list) out.push("</ul>");
  return out.join("\n");
}

async function getJson(url) {
  const headers = { accept: "application/vnd.github+json", "user-agent": "taskmaster-site" };
  if (process.env.GITHUB_TOKEN) headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  const res = await fetch(url, { headers });
  if (!res.ok) throw new Error(`${url} answered ${res.status}`);
  return res.json();
}

/** Releases, newest first, each with its files and checksums. Cached in .cache/. */
export async function releases({ offline = false } = {}) {
  // A saved list instead of GitHub: previews and tests.
  if (process.env.TASKMASTER_RELEASES_FILE) return JSON.parse(readFileSync(process.env.TASKMASTER_RELEASES_FILE, "utf8"));
  const cache = join(here, ".cache/releases.json");
  const saved = existsSync(cache) ? JSON.parse(readFileSync(cache, "utf8")) : null;
  if (offline || (saved && Date.now() - saved.at < CACHE_TTL_MS)) return saved?.releases ?? [];
  try {
    const list = await getJson(`https://api.github.com/repos/${REPO}/releases?per_page=30`);
    const out = [];
    for (const r of list.filter((r) => !r.draft)) {
      const sumsAsset = r.assets.find((a) => a.name === "SHA256SUMS.txt");
      let sums = {};
      if (sumsAsset && out.length < 10) {
        try {
          const text = await (await fetch(sumsAsset.browser_download_url, { headers: { "user-agent": "taskmaster-site" } })).text();
          sums = parseSums(text);
        } catch {
          // The page links the file instead.
        }
      }
      out.push({
        tag: r.tag_name,
        name: r.name || r.tag_name,
        date: r.published_at,
        prerelease: r.prerelease,
        notes: r.body ?? "",
        url: r.html_url,
        sumsUrl: sumsAsset?.browser_download_url ?? null,
        files: r.assets.map((a) => ({ name: a.name, url: a.browser_download_url, size: a.size, sha256: sums[a.name] ?? null })),
      });
    }
    mkdirSync(dirname(cache), { recursive: true });
    writeFileSync(cache, JSON.stringify({ at: Date.now(), releases: out }));
    return out;
  } catch (err) {
    console.warn(`releases: ${err.message}; ${saved ? "using the saved list" : "none listed"}`);
    return saved?.releases ?? [];
  }
}

export function parseSums(text) {
  const sums = {};
  for (const line of text.split("\n")) {
    const m = /^([0-9a-f]{64})\s+\*?(.+)$/.exec(line.trim());
    if (m) sums[m[2]] = m[1];
  }
  return sums;
}

/** "Tip jar · Taskmaster, checker off (agy for every role)" from a results line. */
function rowTitle(dir, r) {
  const prompt = existsSync(join(dir, `${r.name}.txt`)) ? readFileSync(join(dir, `${r.name}.txt`), "utf8").split(/[.:\n]/)[0].trim() : r.name;
  const rolesFile = join(dir, r.name, "roles");
  const harnesses = existsSync(rolesFile) ? [...new Set(readFileSync(rolesFile, "utf8").split("\n").filter((l) => l && !l.startsWith("--")))] : [];
  if (r.label.startsWith("single agent")) return `${prompt} · ${r.label}`;
  const setting = r.label.replace(/-/g, " ").replace(/^checker /, "checker ").replace(/^code mode/, "Code Mode");
  const who = harnesses.length === 1 ? ` (${harnesses[0]} for every role)` : harnesses.length ? ` (${harnesses.join(", ")})` : "";
  return `${prompt} · Taskmaster, ${setting}${who}`;
}

/** Measured builds: one row per labelled run in ../bench/. Nothing measured, nothing shown. */
export function measurements(dir = join(root, "bench")) {
  const rows = [];
  const results = join(dir, "results.jsonl");
  if (existsSync(results)) {
    for (const line of readFileSync(results, "utf8").split("\n").filter(Boolean)) {
      const r = JSON.parse(line);
      rows.push({ title: rowTitle(dir, r), name: r.name, label: r.label, seconds: r.seconds, ok: r.ok, calls: r.calls, input: r.input, output: r.output, cache_read: r.cache_read });
    }
  }
  const evals = join(dir, "evals");
  if (existsSync(evals)) {
    for (const file of readdirSync(evals).filter((f) => f.endsWith(".json"))) {
      const e = JSON.parse(readFileSync(join(evals, file), "utf8"));
      for (const side of e.sides ?? []) rows.push(side);
    }
  }
  return rows;
}

const n = (v) => (v == null ? "—" : Number(v).toLocaleString("en-US"));
const mins = (s) => (s == null ? "—" : s >= 90 ? `${(s / 60).toFixed(1)} min` : `${Math.round(s)} s`);

function layout({ title, description, path, body, motion = false }) {
  const nav = (href, label, extra = "") => `<a href="${href}"${path === href ? ' aria-current="page"' : ""}${extra}>${label}</a>`;
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escape(title)}</title>
<meta name="description" content="${escape(description)}">
<meta name="color-scheme" content="dark light">
<link rel="icon" href="/favicon.ico" sizes="any">
<link rel="icon" type="image/png" href="/favicon-32.png">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<link rel="stylesheet" href="/site.css">
<script>try{if(localStorage.getItem("taskmaster-site-theme")==="light")document.documentElement.classList.add("light")}catch(e){}</script>
</head>
<body>
<a class="skip" href="#main">Skip to the content</a>
<header class="top"><div class="wrap">
<a class="brand" href="/"><img src="/icon.png" alt="" width="28" height="28">Taskmaster</a>
<nav aria-label="Site">
${nav("/releases/", "Download")}
${nav(GITHUB, "GitHub", ' class="hide-narrow"')}
<button class="theme" type="button" data-theme aria-pressed="false">Light</button>
</nav>
</div></header>
<main id="main">
${body}
</main>
<footer><div class="wrap">
<span>Taskmaster is MIT licensed.</span>
<a href="${GITHUB}">Source</a>
<a href="/releases/">Releases</a>
<a href="${GITHUB}/issues">Issues</a>
</div></footer>
<script src="/site.js" defer></script>
${motion ? `<script src="/vendor/gsap.min.js" defer></script>
<script src="/vendor/ScrollTrigger.min.js" defer></script>
<script src="/vendor/lenis.min.js" defer></script>
<script src="/motion.js" defer></script>` : ""}
</body>
</html>
`;
}

export function landing(rows, latest) {
  const table = rows.length
    ? `<div class="table-wrap"><table class="numbers">
<caption>Each row is one build of the same locked plan, timed from approval to done. Planning is left out so every row starts equal.</caption>
<thead><tr><th scope="col">Build</th><th scope="col">Wall time</th><th scope="col">Agent calls</th><th scope="col">Output tokens</th><th scope="col">Fresh input</th><th scope="col">Cache reads</th><th scope="col">Finished</th></tr></thead>
<tbody>
${rows.map((r) => `<tr><th scope="row">${escape(r.title ?? `${r.name}: ${r.label}`)}</th><td>${mins(r.seconds)}</td><td>${n(r.calls)}</td><td>${n(r.output)}</td><td>${n(r.input)}</td><td>${n(r.cache_read)}</td><td>${r.ok ? "yes" : "no"}</td></tr>`).join("\n")}
</tbody></table></div>
<p class="note">Measured on one machine with the harnesses named in each row; the raw results and the prompts are in <a href="${GITHUB}/tree/main/bench"><code>bench/</code></a>, and <code>scripts/bench.sh</code> runs them again.</p>`
    : `<p class="empty">No benchmark has been published yet. When one is, its raw results land in <a href="${GITHUB}/tree/main/bench"><code>bench/</code></a> and show up here, with the harness and the prompt next to every number.</p>`;
  const download = latest ? `/releases/` : `${GITHUB}#quickstart`;
  return layout({
    title: "Taskmaster: one master plans, workers build, every page is checked",
    description: "Taskmaster is a multi-model coding harness: one master model plans the product, cheaper workers build its pages in parallel, and each page is verified before the run is done.",
    path: "/",
    motion: true,
    body: `<div class="hero"><div class="wrap">
<p class="eyebrow">multi-model coding harness</p>
<h1>One master plans. Workers build. <span>Nothing is done until it is checked.</span></h1>
<p class="lede">Describe a product once. A strong model locks the plan and the contracts every page shares. Cheaper models build the pages side by side, each in its own worktree. Type checks, the project's own build and a real browser look at every page before the run says done.</p>
<div class="actions">
<a class="button primary" href="${download}">Download${latest ? ` <small>${escape(latest.tag)}</small>` : ""}</a>
<a class="button" href="${GITHUB}">Read the source on GitHub</a>
</div>
<figure class="shot">
<img class="dark-only" src="/img/run-dark.png" width="1360" height="860" alt="A finished run: its plan, the token ledger split into output, input and cache, and each agent's work.">
<img class="light-only" src="/img/run-light.png" width="1360" height="860" alt="The same run in the light theme." loading="lazy">
<figcaption>A finished run. The token ledger splits every agent's tokens into output, input, cache writes and cache reads.</figcaption>
</figure>
</div></div>

<section class="band" aria-labelledby="how"><div class="wrap">
<h2 id="how">How a run goes</h2>
<p class="lead">The run is a state machine with a log. Models do the writing; Taskmaster decides what happens next, so a run that stops resumes from its log.</p>
<ol class="steps">
<li><span class="n">01 plan</span><h3>The master locks the plan</h3><p>Pages, the files each one owns, and the shared contracts are fixed before anyone builds.</p></li>
<li><span class="n">02 build</span><h3>Workers build in parallel</h3><p>Each page gets its own agent and worktree. Only files inside its glob are merged.</p></li>
<li><span class="n">03 check</span><h3>Everything is checked for free</h3><p>Types, lint, the project's build and tests, and a headless browser on every page. No tokens spent.</p></li>
<li><span class="n">04 repair</span><h3>Failures go back to their owner</h3><p>An error returns to the page that owns the file, and climbs to a stronger model only when it has to.</p></li>
</ol>
</div></section>

<section class="band" aria-labelledby="numbers"><div class="wrap">
<h2 id="numbers">Measured, not promised</h2>
<p class="lead">Wall time and tokens for real builds, read from Taskmaster's own ledger. Output tokens lead: they are what a harness can cut by asking models for less.</p>
${table}
</div></section>

<section class="band" aria-labelledby="providers"><div class="wrap">
<h2 id="providers">Runs on the CLIs you already pay for</h2>
<p class="lead">Each role can use a different harness, so the reviewer is never the model that wrote the page. Install, update and sign in to each from inside the app.</p>
<ul class="providers">
<li><strong>Claude Code</strong><span>claude</span></li>
<li><strong>Grok Build</strong><span>grok</span></li>
<li><strong>Antigravity</strong><span>agy</span></li>
<li><strong>Codex</strong><span>codex</span></li>
<li><strong>OpenCode</strong><span>opencode</span></li>
</ul>
</div></section>

<section class="band" aria-labelledby="get"><div class="wrap closing">
<div><h2 id="get">Get Taskmaster</h2><p class="hint">macOS and Linux desktop apps, and a server build of tm-daemon for headless machines.</p></div>
<div class="actions"><a class="button primary" href="${download}">Download</a><a class="button" href="${GITHUB}">GitHub</a></div>
</div></section>`,
  });
}

function fileList(release) {
  const files = release.files
    .map((f) => ({ ...f, kind: classify(f.name) }))
    .filter((f) => f.kind && f.kind.os !== "mac-update")
    .sort((a, b) => a.kind.order - b.kind.order);
  if (!files.length) return `<p class="hint">This release has no files to download.</p>`;
  return `<ul class="downloads">
${files
  .map(
    (f) => `<li><a href="${escape(f.url)}" data-os="${f.kind.os}"><span class="what">${escape(f.kind.what)}</span><span class="file">${escape(f.name)}${f.size ? ` · ${(f.size / 1048576).toFixed(1)} MB` : ""}</span></a></li>`,
  )
  .join("\n")}
</ul>`;
}

function sumsBlock(release) {
  const lines = release.files.filter((f) => f.sha256).map((f) => `${f.sha256}  ${f.name}`);
  if (!lines.length && !release.sumsUrl) return "";
  return `<details class="sums"><summary>SHA-256 checksums</summary>
${lines.length ? `<pre>${escape(lines.join("\n"))}</pre>` : ""}
${release.sumsUrl ? `<p class="hint">Check a download with <code>sha256sum -c SHA256SUMS.txt --ignore-missing</code>; the file is <a href="${escape(release.sumsUrl)}">SHA256SUMS.txt</a>.</p>` : ""}
</details>`;
}

const day = (iso) => (iso ? new Date(iso).toISOString().slice(0, 10) : "");

export function releasesPage(list) {
  const [latest, ...older] = list;
  const body = latest
    ? `<div class="latest" data-latest>
<div class="latest-title"><h2>${escape(latest.name)}</h2><span class="hint">released <time datetime="${escape(latest.date)}">${day(latest.date)}</time>${latest.prerelease ? " · pre-release" : ""}</span></div>
${fileList(latest)}
<div class="notes">${markdown(latest.notes)}</div>
${sumsBlock(latest)}
</div>
${older.length ? `<h2>Earlier versions</h2><ul class="older">
${older.map((r) => `<li><details><summary><span class="v">${escape(r.name)}</span><span class="date">${day(r.date)}</span></summary><div class="body">${fileList(r)}<div class="notes">${markdown(r.notes)}</div>${sumsBlock(r)}<p><a href="${escape(r.url)}">On GitHub</a></p></div></details></li>`).join("\n")}
</ul>` : ""}`
    : `<p class="empty">No release is published yet. Until there is one, build Taskmaster from <a href="${GITHUB}#quickstart">the source</a>.</p>`;
  return layout({
    title: "Download Taskmaster",
    description: "Taskmaster releases: the desktop app for macOS and Linux, and tm-daemon for servers, with checksums and release notes.",
    path: "/releases/",
    body: `<div class="wrap">
<div class="releases-head"><h1>Download Taskmaster</h1><p class="hint">The file for this computer is listed first. Every file has a SHA-256 checksum below its release.</p></div>
${body}
</div>`,
  });
}

// The landing page only: Lenis for the scroll, GSAP's ScrollTrigger to bring each band in
// once as it reaches the view. Short, ease-out, no bounce; with reduced motion, nothing runs
// and the page is as it was written.
const MOTION_JS = `(function () {
  if (!window.gsap || !window.ScrollTrigger || !window.Lenis) return;
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  var gsap = window.gsap;
  gsap.registerPlugin(window.ScrollTrigger);
  var lenis = new window.Lenis({ duration: 0.9, easing: function (t) { return 1 - Math.pow(1 - t, 3); }, smoothWheel: true });
  lenis.on("scroll", window.ScrollTrigger.update);
  gsap.ticker.add(function (time) { lenis.raf(time * 1000); });
  gsap.ticker.lagSmoothing(0);
  document.querySelectorAll('a[href^="#"]').forEach(function (a) {
    a.addEventListener("click", function (e) {
      var target = document.querySelector(a.getAttribute("href"));
      if (target) { e.preventDefault(); lenis.scrollTo(target, { offset: -24 }); }
    });
  });
  // Each band rises once, a little, as it comes into view; lists follow one item at a time.
  gsap.utils.toArray(".band .wrap").forEach(function (band) {
    var parts = band.querySelectorAll("h2, .lead, .steps li, .providers li, .table-wrap, .note, .empty, .closing > *");
    if (!parts.length) return;
    gsap.from(parts, { opacity: 0, y: 14, duration: 0.3, ease: "power2.out", stagger: 0.05, clearProps: "opacity,transform", scrollTrigger: { trigger: band, start: "top 86%", once: true } });
  });
  // The screenshot settles as the hero scrolls away.
  var shot = document.querySelector(".hero .shot");
  if (shot) gsap.to(shot, { y: -20, ease: "none", scrollTrigger: { trigger: ".hero", start: "top top", end: "bottom top", scrub: true } });
})();
`;

const SITE_JS = `// Theme switch, and the download for this computer first. Nothing else runs here.
(function () {
  var root = document.documentElement;
  var button = document.querySelector("[data-theme]");
  function show() {
    var light = root.classList.contains("light");
    if (button) { button.textContent = light ? "Dark" : "Light"; button.setAttribute("aria-pressed", String(light)); button.setAttribute("aria-label", light ? "Use the dark theme" : "Use the light theme"); }
  }
  if (button) button.addEventListener("click", function () {
    root.classList.toggle("light");
    try { localStorage.setItem("taskmaster-site-theme", root.classList.contains("light") ? "light" : "dark"); } catch (e) {}
    show();
  });
  show();
  var ua = navigator.userAgent, platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  var os = /Win/i.test(platform) || /Windows/i.test(ua) ? "windows" : /Mac/i.test(platform) || /Mac OS X/i.test(ua) ? "mac" : /Linux|X11/i.test(platform + ua) && !/Android/i.test(ua) ? "linux" : null;
  if (!os) return;
  document.querySelectorAll(".downloads").forEach(function (list) {
    var links = Array.prototype.slice.call(list.querySelectorAll("a[data-os]"));
    var mine = links.filter(function (a) { var k = a.getAttribute("data-os"); return k === os || k.indexOf(os + "-") === 0 && k !== "linux-server"; });
    if (os === "mac" && mine.length > 1) {
      // Apple silicon cannot be told from Intel here for certain; Apple silicon is the default since 2020.
      mine.sort(function (a, b) { return a.getAttribute("data-os") === "mac-arm" ? -1 : 1; });
    }
    if (!mine.length) return;
    var first = mine[0];
    first.classList.add("mine");
    var tag = document.createElement("span");
    tag.className = "yours";
    tag.textContent = "For this computer";
    first.insertBefore(tag, first.firstChild);
    list.insertBefore(first.parentNode, list.firstChild);
  });
})();
`;

export async function build({ offline = false, out = join(here, "dist") } = {}) {
  const list = await releases({ offline });
  const rows = measurements();
  rmSync(out, { recursive: true, force: true });
  mkdirSync(join(out, "releases"), { recursive: true });
  mkdirSync(join(out, "img"), { recursive: true });
  writeFileSync(join(out, "index.html"), landing(rows, list[0] ?? null));
  writeFileSync(join(out, "releases/index.html"), releasesPage(list));
  writeFileSync(join(out, "releases.json"), JSON.stringify(list, null, 2));
  writeFileSync(join(out, "site.js"), SITE_JS);
  writeFileSync(join(out, "motion.js"), MOTION_JS);
  // Lenis and GSAP are served from the site itself, like everything else (see the CSP).
  mkdirSync(join(out, "vendor"), { recursive: true });
  for (const [from, to] of [["gsap/dist/gsap.min.js", "gsap.min.js"], ["gsap/dist/ScrollTrigger.min.js", "ScrollTrigger.min.js"], ["lenis/dist/lenis.min.js", "lenis.min.js"]]) {
    copyFileSync(join(here, "node_modules", from), join(out, "vendor", to));
  }
  copyFileSync(join(here, "src/site.css"), join(out, "site.css"));
  for (const f of ["icon.png", "favicon.ico", "favicon-32.png", "apple-touch-icon.png"]) copyFileSync(join(root, "assets", f), join(out, f));
  for (const [from, to] of [["33-run-tokens-dark.png", "run-dark.png"], ["33-run-tokens-light.png", "run-light.png"]]) {
    const src = join(root, "assets/screenshots", from);
    if (existsSync(src)) copyFileSync(src, join(out, "img", to));
  }
  return { releases: list.length, measurements: rows.length, out };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const done = await build({ offline: process.argv.includes("--offline") });
  console.log(`site: ${done.releases} release(s), ${done.measurements} measurement(s) -> ${done.out}`);
}
