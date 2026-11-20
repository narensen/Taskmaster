import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { classify, landing, markdown, measurements, parseSums, releasesPage } from "./build.mjs";

const release = (tag, files, notes = "") => ({
  tag,
  name: `Taskmaster ${tag}`,
  date: "2026-11-20T10:00:00Z",
  prerelease: false,
  notes,
  url: `https://github.com/narensen/Taskmaster/releases/tag/${tag}`,
  sumsUrl: "https://example.invalid/SHA256SUMS.txt",
  files: files.map((name) => ({ name, url: `https://example.invalid/${name}`, size: 2_097_152, sha256: name.endsWith(".sig") ? null : "a".repeat(64) })),
});

test("release files are sorted by platform and signatures stay out", () => {
  assert.equal(classify("Taskmaster_0.2.0_aarch64.dmg").os, "mac-arm");
  assert.equal(classify("Taskmaster_0.2.0_x64.dmg").os, "mac-intel");
  assert.equal(classify("Taskmaster_0.2.0_amd64.AppImage").os, "linux");
  assert.equal(classify("Taskmaster_0.2.0_amd64.deb").os, "linux-deb");
  assert.equal(classify("Taskmaster_0.2.0_x64-setup.exe").os, "windows");
  assert.equal(classify("taskmaster-v0.2.0-x86_64-linux.tar.gz").os, "linux-server");
  for (const skip of ["Taskmaster.AppImage.sig", "latest.json", "SHA256SUMS.txt", "x.tar.gz.sha256"]) assert.equal(classify(skip), null, skip);
});

test("release notes are rendered without letting markup through", () => {
  const html = markdown("## What changed\n- Faster plans by `tm-daemon`\n- <script>alert(1)</script>\n[docs](https://example.com/a?b=1&c=2) [bad](javascript:alert(1))");
  assert.match(html, /<h4>What changed<\/h4>/);
  assert.match(html, /<li>Faster plans by <code>tm-daemon<\/code><\/li>/);
  assert.match(html, /&lt;script&gt;/);
  assert.doesNotMatch(html, /<script>/);
  assert.match(html, /<a href="https:\/\/example.com\/a\?b=1&amp;c=2">docs<\/a>/);
  assert.doesNotMatch(html, /href="javascript:/);
});

test("checksums are read from SHA256SUMS.txt", () => {
  const sums = parseSums(`${"b".repeat(64)}  Taskmaster.dmg\n${"c".repeat(64)} *taskmaster.tar.gz\nnot a line\n`);
  assert.deepEqual(sums, { "Taskmaster.dmg": "b".repeat(64), "taskmaster.tar.gz": "c".repeat(64) });
});

test("the releases page leads with the newest release and links older ones", () => {
  const html = releasesPage([
    release("v0.3.0", ["Taskmaster_0.3.0_aarch64.dmg", "Taskmaster_0.3.0_amd64.AppImage", "Taskmaster_0.3.0_amd64.AppImage.sig", "latest.json"], "- New: skills"),
    release("v0.2.0", ["Taskmaster_0.2.0_amd64.deb"]),
  ]);
  assert.match(html, /<h2>Taskmaster v0.3.0<\/h2>/);
  assert.match(html, /data-os="mac-arm"/);
  assert.doesNotMatch(html, /AppImage\.sig/);
  assert.match(html, /<li>New: skills<\/li>/);
  assert.match(html, /SHA-256 checksums/);
  assert.match(html, new RegExp(`${"a".repeat(64)}  Taskmaster_0.3.0_aarch64.dmg`));
  assert.match(html, /Earlier versions[\s\S]*Taskmaster v0.2.0/);
  assert.match(releasesPage([]), /No release is published yet/);
});

test("only measured numbers reach the landing page", () => {
  const dir = mkdtempSync(join(tmpdir(), "tm-site-"));
  assert.equal(measurements(dir).length, 0);
  assert.match(landing([], null), /No benchmark has been published yet/);
  writeFileSync(join(dir, "results.jsonl"), JSON.stringify({ name: "tipjar", label: "checker-off", seconds: 835.2, ok: true, calls: 3, input: 310684, output: 126909, cache_read: 3088358 }) + "\n");
  mkdirSync(join(dir, "evals"));
  writeFileSync(join(dir, "evals/tipjar.json"), JSON.stringify({ sides: [{ title: "Single agent (agy)", seconds: 1200, calls: 1, output: 90000, input: 1, cache_read: 2, ok: true }] }));
  const rows = measurements(dir);
  assert.equal(rows.length, 2);
  const html = landing(rows, null);
  assert.match(html, /tipjar · Taskmaster, checker off/);
  assert.match(html, /13\.9 min/);
  assert.match(html, /126,909/);
  assert.match(html, /Single agent \(agy\)/);
  assert.doesNotMatch(html, /No benchmark has been published yet/);
});

test("the Caddy block's CSP hash matches the one inline script", async () => {
  const { createHash } = await import("node:crypto");
  const { readFileSync } = await import("node:fs");
  const page = landing([], null);
  const scripts = [...page.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
  assert.equal(scripts.length, 1, "one inline script");
  const hash = createHash("sha256").update(scripts[0]).digest("base64");
  const caddy = readFileSync(new URL("../deploy/site/Caddyfile", import.meta.url), "utf8");
  assert.ok(caddy.includes(`'sha256-${hash}'`), `deploy/site/Caddyfile should allow sha256-${hash}`);
});

test("release notes keep their tables", () => {
  const html = markdown("### Faster\n| | 0.1.4 | 0.1.5 |\n|---|---|---|\n| Frame rate | 30.5 fps | 57.8 fps |\n| <b>x</b> | `a` | b |\n\nAfter.");
  assert.match(html, /<table><thead><tr><th><\/th><th>0\.1\.4<\/th><th>0\.1\.5<\/th><\/tr><\/thead>/);
  assert.match(html, /<tr><td>Frame rate<\/td><td>30\.5 fps<\/td><td>57\.8 fps<\/td><\/tr>/);
  assert.match(html, /<td>&lt;b&gt;x&lt;\/b&gt;<\/td><td><code>a<\/code><\/td>/);
  assert.match(html, /<\/table>\n<p>After\.<\/p>/);
  assert.doesNotMatch(html, /\|/);
});
