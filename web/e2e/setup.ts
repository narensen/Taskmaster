// Start tm-daemon on a fresh git project and pair once; tests read the address and token.
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, existsSync, mkdirSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, delimiter } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

export const STATE = join(tmpdir(), "taskmaster-e2e.json");

export default async function setup() {
  const root = resolve(here, "../..");
  const daemon = process.env.TM_DAEMON ?? join(root, "target/debug/tm-daemon");
  if (!existsSync(daemon)) throw new Error(`build the daemon first: cargo build -p tm-daemon (looked for ${daemon})`);
  const dist = resolve(here, "../dist");
  if (!existsSync(join(dist, "index.html"))) throw new Error("build the app first: npm run build");
  const base = mkdtempSync(join(tmpdir(), "tm-e2e-"));
  const project = join(base, "project");
  execFileSync("sh", ["-c", `mkdir -p "${project}" && cd "${project}" && git init -q && git config user.email e2e@example.invalid && git config user.name e2e && echo '# e2e' > README.md && git add . && git commit -qm init`]);
  // A skills store of its own, with one skill in it, and (except for screenshots, which
  // show the real providers) a home with nothing in it.
  const skills = join(base, "skills");
  mkdirSync(join(skills, "tidy-css"), { recursive: true });
  writeFileSync(join(skills, "tidy-css/SKILL.md"), "---\nname: tidy-css\ndescription: Keep stylesheets ordered and small.\n---\n# Tidy CSS\n");
  writeFileSync(
    join(skills, "skills.json"),
    JSON.stringify({ skills: [{ name: "tidy-css", source: { repo: "acme/kit", path: "skills/tidy-css", tree: "v1" }, enabled: true, installed: "2026-11-17T10:00:00Z" }] }),
  );
  mkdirSync(join(base, "home"), { recursive: true });
  const child = spawn(daemon, ["serve", project, "--bind", "127.0.0.1:0", "--web", dist, "--commons", join(base, "commons"), "--dictionary", join(base, "dictionary")], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, XDG_DATA_HOME: join(base, "data"), TASKMASTER_SKILLS: skills, ...(process.env.SHOTS ? {} : { HOME: join(base, "home") }) },
  });
  const line = await new Promise<string>((ok, fail) => {
    let said = "";
    const timer = setTimeout(() => fail(new Error(`tm-daemon did not start: ${said}`)), 20_000);
    const read = (chunk: Buffer) => {
      said += chunk.toString();
      const found = said.split("\n").find((l) => l.includes(" code "));
      if (found) {
        clearTimeout(timer);
        ok(found);
      }
    };
    child.stdout.on("data", read);
    child.stderr.on("data", read);
    child.on("exit", (code) => fail(new Error(`tm-daemon exited with ${code}: ${said}`)));
  });
  const url = /api (\S+)/.exec(line)![1]!.replace(/\/$/, "");
  const code = /code (\S+)/.exec(line)![1]!;
  const paired = await fetch(`${url}/api/pair`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ code }) });
  const { token } = (await paired.json()) as { token: string };
  child.unref();

  // A second daemon whose agents are scripts (examples/ui_fixture.rs), for runs that go
  // from plan to done with real ledgers and transcripts, and no model.
  const fixtureBin = join(root, "target/debug/examples/ui_fixture");
  let fixture: { url: string; token: string; pid: number } | null = null;
  if (existsSync(fixtureBin)) {
    const fixtureProject = join(base, "fixture");
    execFileSync("sh", ["-c", `mkdir -p "${fixtureProject}" && cd "${fixtureProject}" && git init -q && git config user.email e2e@example.invalid && git config user.name e2e && echo '# shop' > README.md && git add . && git commit -qm init`]);
    // The scripted harnesses do not need a real CLI, but the providers API only offers
    // what is installed. A stub Grok lets the shell e2e pick it on a bare runner.
    const bins = join(base, "bin");
    mkdirSync(bins, { recursive: true });
    const grok = join(bins, "grok");
    writeFileSync(grok, "#!/bin/sh\nif [ \"$1\" = models ]; then echo '* grok-4 (default)'; fi\nexit 0\n");
    chmodSync(grok, 0o755);
    const f = spawn(fixtureBin, [fixtureProject, "0", dist], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, FIXTURE_DELAY_MS: "5", TASKMASTER_SKILLS: skills, HOME: join(base, "home"), PATH: `${bins}${delimiter}${process.env.PATH ?? ""}` },
    });
    const said = await new Promise<string>((ok, fail) => {
      let text = "";
      const timer = setTimeout(() => fail(new Error(`ui_fixture did not start: ${text}`)), 20_000);
      f.stdout.on("data", (chunk: Buffer) => {
        text += chunk.toString();
        if (/api \S+/.test(text) && /code \S+/.test(text)) {
          clearTimeout(timer);
          ok(text);
        }
      });
    });
    const fUrl = /api (\S+)/.exec(said)![1]!.replace(/\/$/, "");
    const fCode = /code (\S+)/.exec(said)![1]!;
    const fPaired = await fetch(`${fUrl}/api/pair`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ code: fCode }) });
    fixture = { url: fUrl, token: ((await fPaired.json()) as { token: string }).token, pid: f.pid! };
    f.unref();
  }
  writeFileSync(STATE, JSON.stringify({ url, token, pid: child.pid, base, project, fixture }));
}
