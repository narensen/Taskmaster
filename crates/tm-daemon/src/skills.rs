//! One skills store for every harness: SKILL.md folders under `~/.taskmaster/skills`.
//!
//! Search asks skills.sh. Install and update read the skill's folder from its GitHub
//! repository: one API call lists the tree, and the files come from
//! raw.githubusercontent.com, so a search-heavy day does not spend the API's hourly limit.
//! `skills.json` beside the folders records where each came from, the folder's tree hash
//! (to tell when it changed upstream), and whether it is enabled.
//!
//! Skills found in the project (`.claude/skills`, `.agents/skills`) or the home directory
//! are listed too, and can be copied into the store.
//!
//! A run gets a skill only when the planner attaches it to a page; the engine copies the
//! folder into that page's worktree under `.taskmaster/skills/`, which git ignores.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::time::rfc3339_now;

const SEARCH_URL: &str = "https://skills.sh/api/search";
const GITHUB_API: &str = "https://api.github.com";
const RAW: &str = "https://raw.githubusercontent.com";
/// A skill larger than this is refused: it would not fit in an agent's context anyway.
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILES: usize = 200;

/// Reads a URL. Tests pass a map of canned answers.
pub trait Fetch: Send + Sync {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>>;
}

/// The network, with a GitHub token from `GITHUB_TOKEN` or `GH_TOKEN` when one is set.
pub struct Web;

impl Fetch for Web {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let mut request = ureq::get(url).set("user-agent", "taskmaster").timeout(std::time::Duration::from_secs(20));
        if url.starts_with(GITHUB_API)
            && let Some(token) = std::env::var("GITHUB_TOKEN").ok().or_else(|| std::env::var("GH_TOKEN").ok()).filter(|t| !t.is_empty())
        {
            request = request.set("authorization", &format!("Bearer {token}"));
        }
        let response = request.call().map_err(|err| match err {
            ureq::Error::Status(403, _) if url.starts_with(GITHUB_API) => {
                anyhow::anyhow!("GitHub's hourly limit for this machine is used up; set GITHUB_TOKEN to raise it")
            }
            ureq::Error::Status(code, _) => anyhow::anyhow!("{url} answered {code}"),
            other => anyhow::anyhow!("cannot reach {url}: {other}"),
        })?;
        let mut bytes = Vec::new();
        std::io::Read::take(response.into_reader(), (MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

use std::io::Read as _;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// `owner/repo` on GitHub.
    pub repo: String,
    /// The skill's folder in that repository.
    pub path: String,
    /// The folder's git tree hash when it was installed.
    pub tree: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    #[serde(default)]
    pub source: Option<Source>,
    pub enabled: bool,
    pub installed: String,
    #[serde(default)]
    pub updated: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Registry {
    skills: Vec<Record>,
}

/// A skill as the app and the planner see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `store`, `project` or `home`.
    pub place: &'static str,
    /// The SKILL.md, relative to the project for a project skill, otherwise absolute.
    pub path: String,
    pub enabled: bool,
    pub source: Option<Source>,
    pub installed: Option<String>,
    pub updated: Option<String>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub skill: String,
    pub installs: u64,
    pub installed: bool,
}

pub struct Store {
    root: PathBuf,
    fetch: Arc<dyn Fetch>,
}

/// `~/.taskmaster/skills`, or `TASKMASTER_SKILLS` when set.
pub fn default_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("TASKMASTER_SKILLS") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join(".taskmaster/skills")
}

/// The `name` and `description` from a SKILL.md's front matter.
pub fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    let mut in_description = false;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let clean = |v: &str| v.trim().trim_matches('"').trim_matches('\'').to_string();
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(clean(v)).filter(|v| !v.is_empty());
            in_description = false;
        } else if let Some(v) = line.strip_prefix("description:") {
            let v = clean(v);
            in_description = v.is_empty() || v == ">" || v == "|" || v == ">-" || v == "|-";
            description = (!in_description).then_some(v);
        } else if in_description && line.starts_with([' ', '\t']) {
            let more = line.trim();
            let joined = match description.take() {
                Some(d) => format!("{d} {more}"),
                None => more.to_string(),
            };
            description = Some(joined);
        } else if !line.starts_with([' ', '\t']) {
            in_description = false;
        }
    }
    (name, description)
}

/// A store-safe name: letters, digits, `-`, `_` and `.`, not starting with `.`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')));
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

impl Store {
    pub fn new(root: impl Into<PathBuf>, fetch: Arc<dyn Fetch>) -> Store {
        Store { root: root.into(), fetch }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn registry(&self) -> Registry {
        std::fs::read_to_string(self.root.join("skills.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self, registry: &Registry) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let tmp = self.root.join(".skills.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(registry)?)?;
        std::fs::rename(tmp, self.root.join("skills.json"))?;
        Ok(())
    }

    /// The store's skills, then those found in the project and the home directory.
    pub fn list(&self, project: &Path) -> Vec<Skill> {
        let registry = self.registry();
        let mut out = Vec::new();
        for record in &registry.skills {
            let file = self.root.join(&record.name).join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let (_, description) = front_matter(&text);
            out.push(Skill {
                name: record.name.clone(),
                description: description.unwrap_or_default(),
                place: "store",
                path: file.display().to_string(),
                enabled: record.enabled,
                source: record.source.clone(),
                installed: Some(record.installed.clone()),
                updated: record.updated.clone(),
            });
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut roots: Vec<(PathBuf, &'static str, String)> =
            [".agents/skills", ".claude/skills"].iter().map(|r| (project.join(r), "project", r.to_string())).collect();
        if let Some(home) = &home {
            for r in [".claude/skills", ".agents/skills"] {
                roots.push((home.join(r), "home", home.join(r).display().to_string()));
            }
        }
        for (dir, place, shown) in roots {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut found: Vec<_> = entries.flatten().collect();
            found.sort_by_key(|e| e.file_name());
            for entry in found {
                let file = entry.path().join("SKILL.md");
                let Ok(text) = std::fs::read_to_string(&file) else { continue };
                let (name, description) = front_matter(&text);
                let name = name.unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
                if out.iter().any(|s: &Skill| s.name == name) {
                    continue;
                }
                let path = if place == "project" { format!("{shown}/{}/SKILL.md", entry.file_name().to_string_lossy()) } else { file.display().to_string() };
                out.push(Skill { name, description: description.unwrap_or_default(), place, path, enabled: false, source: None, installed: None, updated: None });
            }
        }
        out
    }

    /// Enabled store skills: what the planner may attach.
    pub fn enabled(&self) -> Vec<Skill> {
        self.list(Path::new("/nonexistent")).into_iter().filter(|s| s.place == "store" && s.enabled).collect()
    }

    pub fn search(&self, query: &str) -> anyhow::Result<Vec<Hit>> {
        let query = query.trim();
        if query.len() < 2 {
            bail!("type at least two characters to search");
        }
        let url = format!("{SEARCH_URL}?q={}", encode(query));
        let body: Value = serde_json::from_slice(&self.fetch.get(&url)?).context("skills.sh did not answer with JSON")?;
        let installed: Vec<String> = self.registry().skills.into_iter().map(|r| r.name).collect();
        let mut hits = Vec::new();
        for item in body.get("skills").and_then(Value::as_array).into_iter().flatten() {
            let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            let (repo, skill) = (text("source"), text("skillId"));
            if !valid_repo(&repo) || skill.is_empty() {
                continue;
            }
            let name = Some(text("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| skill.clone());
            hits.push(Hit {
                id: format!("{repo}/{skill}"),
                installed: installed.contains(&name) || installed.contains(&skill),
                name,
                repo,
                skill,
                installs: item.get("installs").and_then(Value::as_u64).unwrap_or(0),
            });
        }
        Ok(hits)
    }

    /// The repository's tree: every path with its kind and hash.
    fn tree(&self, repo: &str) -> anyhow::Result<Vec<(String, String, String)>> {
        let url = format!("{GITHUB_API}/repos/{repo}/git/trees/HEAD?recursive=1");
        let body: Value = serde_json::from_slice(&self.fetch.get(&url)?).context("GitHub did not answer with JSON")?;
        let entries = body.get("tree").and_then(Value::as_array).context("GitHub listed no files")?;
        Ok(entries
            .iter()
            .filter_map(|e| {
                let field = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
                Some((field("path")?, field("type")?, field("sha")?))
            })
            .collect())
    }

    /// The folder in `tree` that holds the skill called `skill`.
    fn locate(&self, repo: &str, tree: &[(String, String, String)], skill: &str) -> anyhow::Result<String> {
        let folders: Vec<String> = tree
            .iter()
            .filter(|(p, kind, _)| kind == "blob" && (p == "SKILL.md" || p.ends_with("/SKILL.md")))
            .map(|(p, _, _)| p.trim_end_matches("SKILL.md").trim_end_matches('/').to_string())
            .collect();
        if let Some(found) = folders.iter().find(|f| f.rsplit('/').next() == Some(skill)) {
            return Ok(found.clone());
        }
        // The folder may be named differently from the skill: read the front matter.
        for folder in folders.iter().take(60) {
            let file = if folder.is_empty() { "SKILL.md".to_string() } else { format!("{folder}/SKILL.md") };
            let Ok(bytes) = self.fetch.get(&format!("{RAW}/{repo}/HEAD/{file}")) else { continue };
            if front_matter(&String::from_utf8_lossy(&bytes)).0.as_deref() == Some(skill) {
                return Ok(folder.clone());
            }
        }
        bail!("{repo} has no skill called `{skill}`")
    }

    fn folder_hash(tree: &[(String, String, String)], folder: &str) -> String {
        if folder.is_empty() {
            return tree.iter().filter(|(_, k, _)| k == "blob").map(|(_, _, sha)| sha.as_str()).collect::<Vec<_>>().join("").chars().take(40).collect();
        }
        tree.iter().find(|(p, k, _)| p == folder && k == "tree").map(|(_, _, sha)| sha.clone()).unwrap_or_default()
    }

    /// Install `skill` from GitHub `repo`. Replaces an earlier copy of the same name.
    pub fn install(&self, repo: &str, skill: &str) -> anyhow::Result<Skill> {
        if !valid_repo(repo) {
            bail!("`{repo}` is not an owner/repository name");
        }
        let tree = self.tree(repo)?;
        let folder = self.locate(repo, &tree, skill)?;
        self.fetch_folder(repo, &tree, &folder, None)
    }

    fn fetch_folder(&self, repo: &str, tree: &[(String, String, String)], folder: &str, keep_name: Option<&str>) -> anyhow::Result<Skill> {
        let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
        let files: Vec<&String> = tree.iter().filter(|(p, k, _)| k == "blob" && p.starts_with(&prefix)).map(|(p, _, _)| p).collect();
        if files.len() > MAX_FILES {
            bail!("the skill has {} files; at most {MAX_FILES} are taken", files.len());
        }
        let staging = self.root.join(format!(".incoming-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let result = (|| -> anyhow::Result<String> {
            let mut total = 0usize;
            for file in &files {
                let rel = &file[prefix.len()..];
                if rel.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
                    bail!("refusing the path `{file}`");
                }
                let bytes = self.fetch.get(&format!("{RAW}/{repo}/HEAD/{}", file.split('/').map(encode).collect::<Vec<_>>().join("/")))?;
                total += bytes.len();
                if total > MAX_BYTES {
                    bail!("the skill is larger than {} MB", MAX_BYTES / 1024 / 1024);
                }
                let dest = staging.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(dest, bytes)?;
            }
            let text = std::fs::read_to_string(staging.join("SKILL.md")).context("the folder has no SKILL.md")?;
            let named = keep_name.map(str::to_string).or_else(|| front_matter(&text).0).unwrap_or_else(|| folder.rsplit('/').next().unwrap_or(repo).to_string());
            if !valid_name(&named) {
                bail!("`{named}` cannot be used as a skill name");
            }
            Ok(named)
        })();
        let name = match result {
            Ok(name) => name,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(err);
            }
        };
        let dest = self.root.join(&name);
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&staging, &dest)?;
        let mut registry = self.registry();
        let now = rfc3339_now();
        let source = Some(Source { repo: repo.to_string(), path: folder.to_string(), tree: Self::folder_hash(tree, folder) });
        match registry.skills.iter_mut().find(|r| r.name == name) {
            Some(record) => {
                record.source = source;
                record.updated = Some(now);
            }
            None => registry.skills.push(Record { name: name.clone(), source, enabled: true, installed: now, updated: None }),
        }
        registry.skills.sort_by(|a, b| a.name.cmp(&b.name));
        self.save(&registry)?;
        self.get(&name).context("the skill was installed but cannot be read")
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.list(Path::new("/nonexistent")).into_iter().find(|s| s.name == name && s.place == "store")
    }

    /// Copy a skill found in the project or home directory into the store.
    pub fn import(&self, project: &Path, name: &str) -> anyhow::Result<Skill> {
        let found = self.list(project).into_iter().find(|s| s.name == name && s.place != "store").with_context(|| format!("no skill `{name}` outside the store"))?;
        if !valid_name(name) {
            bail!("`{name}` cannot be used as a skill name");
        }
        let file = if found.place == "project" { project.join(&found.path) } else { PathBuf::from(&found.path) };
        let from = file.parent().context("no folder")?;
        let dest = self.root.join(name);
        copy_dir(from, &dest, 0)?;
        let mut registry = self.registry();
        registry.skills.retain(|r| r.name != name);
        registry.skills.push(Record { name: name.to_string(), source: None, enabled: true, installed: rfc3339_now(), updated: None });
        registry.skills.sort_by(|a, b| a.name.cmp(&b.name));
        self.save(&registry)?;
        self.get(name).context("the skill was copied but cannot be read")
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> anyhow::Result<Skill> {
        let mut registry = self.registry();
        let record = registry.skills.iter_mut().find(|r| r.name == name).with_context(|| format!("no skill `{name}` in the store"))?;
        record.enabled = enabled;
        self.save(&registry)?;
        self.get(name).context("the skill cannot be read")
    }

    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        if !valid_name(name) {
            bail!("`{name}` is not a skill name");
        }
        let mut registry = self.registry();
        let before = registry.skills.len();
        registry.skills.retain(|r| r.name != name);
        if registry.skills.len() == before {
            bail!("no skill `{name}` in the store");
        }
        self.save(&registry)?;
        let _ = std::fs::remove_dir_all(self.root.join(name));
        Ok(())
    }

    /// Names of store skills whose folder changed upstream, with the new hash. One tree
    /// read per repository.
    pub fn updates(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let mut by_repo: BTreeMap<String, Vec<Record>> = BTreeMap::new();
        for record in self.registry().skills {
            if let Some(source) = &record.source {
                by_repo.entry(source.repo.clone()).or_default().push(record);
            }
        }
        let mut out = BTreeMap::new();
        for (repo, records) in by_repo {
            let tree = self.tree(&repo)?;
            for record in records {
                let source = record.source.as_ref().expect("grouped by source");
                let now = Self::folder_hash(&tree, &source.path);
                if !now.is_empty() && now != source.tree {
                    out.insert(record.name.clone(), now);
                }
            }
        }
        Ok(out)
    }

    /// Fetch a store skill again from where it came from.
    pub fn update(&self, name: &str) -> anyhow::Result<Skill> {
        let record = self.registry().skills.into_iter().find(|r| r.name == name).with_context(|| format!("no skill `{name}` in the store"))?;
        let source = record.source.context("this skill was copied from this machine, so there is nothing to update it from")?;
        let tree = self.tree(&source.repo)?;
        self.fetch_folder(&source.repo, &tree, &source.path, Some(name))
    }

    /// Copy the named enabled skills into `worktree/.taskmaster/skills/`. Returns the ones
    /// copied, with their descriptions.
    pub fn deliver(&self, names: &[String], worktree: &Path) -> Vec<(String, String)> {
        let enabled = self.enabled();
        let mut out = Vec::new();
        for name in names {
            let Some(skill) = enabled.iter().find(|s| &s.name == name) else { continue };
            let dest = worktree.join(".taskmaster/skills").join(name);
            if copy_dir(&self.root.join(name), &dest, 0).is_ok() {
                out.push((name.clone(), skill.description.clone()));
            }
        }
        out
    }
}

fn copy_dir(from: &Path, to: &Path, depth: usize) -> anyhow::Result<()> {
    if depth > 8 {
        bail!("the skill's folders nest too deep");
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &dest, depth + 1)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The skills a person asked for in a prompt: `skill \`name\`` or `$name`.
pub fn asked_for(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if valid_name(name) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    };
    for part in prompt.split("skill `").skip(1) {
        if let Some((name, _)) = part.split_once('`') {
            push(name);
        }
    }
    for word in prompt.split_whitespace() {
        if let Some(name) = word.strip_prefix('$')
            && name.starts_with(|c: char| c.is_ascii_alphabetic())
        {
            push(name.trim_end_matches(['.', ',', ';', ':', ')']));
        }
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Canned answers by URL, and every URL asked for.
    #[derive(Default)]
    pub struct Canned {
        pub answers: Mutex<HashMap<String, Vec<u8>>>,
        pub asked: Mutex<Vec<String>>,
    }

    impl Canned {
        pub fn set(&self, url: &str, body: impl Into<Vec<u8>>) {
            self.answers.lock().unwrap().insert(url.to_string(), body.into());
        }
    }

    impl Fetch for Canned {
        fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            self.asked.lock().unwrap().push(url.to_string());
            self.answers.lock().unwrap().get(url).cloned().with_context(|| format!("{url} answered 404"))
        }
    }

    pub fn tree(entries: &[(&str, &str, &str)]) -> String {
        let items: Vec<Value> = entries.iter().map(|(p, t, s)| serde_json::json!({ "path": p, "type": t, "sha": s })).collect();
        serde_json::json!({ "sha": "root", "tree": items }).to_string()
    }

    pub fn canned_repo(canned: &Canned, folder_sha: &str, body: &str) {
        canned.set(
            "https://api.github.com/repos/acme/kit/git/trees/HEAD?recursive=1",
            tree(&[
                ("README.md", "blob", "r1"),
                ("skills", "tree", "t0"),
                ("skills/tidy-css", "tree", folder_sha),
                ("skills/tidy-css/SKILL.md", "blob", "b1"),
                ("skills/tidy-css/rules/order.md", "blob", "b2"),
            ]),
        );
        canned.set("https://raw.githubusercontent.com/acme/kit/HEAD/skills/tidy-css/SKILL.md", body.to_string());
        canned.set("https://raw.githubusercontent.com/acme/kit/HEAD/skills/tidy-css/rules/order.md", "Order properties by box model.\n");
    }

    fn store(name: &str) -> (Store, Arc<Canned>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tm-skills-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let canned = Arc::new(Canned::default());
        (Store::new(dir.join("store"), canned.clone()), canned, dir)
    }

    const SKILL: &str = "---\nname: tidy-css\ndescription: >\n  Keep stylesheets ordered\n  and small.\n---\n# Tidy CSS\n";

    #[test]
    fn front_matter_reads_folded_descriptions() {
        assert_eq!(front_matter(SKILL), (Some("tidy-css".into()), Some("Keep stylesheets ordered and small.".into())));
        assert_eq!(front_matter("---\nname: \"x\"\ndescription: 'one line'\n---\n"), (Some("x".into()), Some("one line".into())));
        assert_eq!(front_matter("# no front matter"), (None, None));
    }

    #[test]
    fn search_marks_what_is_installed_and_skips_odd_sources() {
        let (store, canned, dir) = store("search");
        canned.set(
            "https://skills.sh/api/search?q=css%20order",
            r#"{"skills":[{"source":"acme/kit","skillId":"tidy-css","name":"tidy-css","installs":42},{"source":"../etc","skillId":"x","name":"x"}]}"#,
        );
        let hits = store.search("css order").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].repo.as_str(), hits[0].installs, hits[0].installed), ("acme/kit", 42, false));
        assert!(store.search("x").is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn install_enable_update_and_remove() {
        let (store, canned, dir) = store("life");
        canned_repo(&canned, "v1", SKILL);
        let skill = store.install("acme/kit", "tidy-css").unwrap();
        assert_eq!(skill.name, "tidy-css");
        assert_eq!(skill.description, "Keep stylesheets ordered and small.");
        assert!(skill.enabled);
        assert!(store.root().join("tidy-css/rules/order.md").is_file());
        assert_eq!(store.enabled().len(), 1);

        assert!(!store.set_enabled("tidy-css", false).unwrap().enabled);
        assert!(store.enabled().is_empty());
        store.set_enabled("tidy-css", true).unwrap();

        // Nothing changed upstream, then the folder's hash moves.
        assert!(store.updates().unwrap().is_empty());
        canned_repo(&canned, "v2", &SKILL.replace("# Tidy CSS", "# Tidy CSS, second edition"));
        assert_eq!(store.updates().unwrap().get("tidy-css").map(String::as_str), Some("v2"));
        store.update("tidy-css").unwrap();
        assert!(std::fs::read_to_string(store.root().join("tidy-css/SKILL.md")).unwrap().contains("second edition"));
        assert!(store.updates().unwrap().is_empty());
        assert!(store.get("tidy-css").unwrap().updated.is_some());

        // A worker's worktree gets a copy of what was attached and enabled, nothing else.
        let tree = dir.join("work");
        let given = store.deliver(&["tidy-css".into(), "missing".into()], &tree);
        assert_eq!(given, vec![("tidy-css".to_string(), "Keep stylesheets ordered and small.".to_string())]);
        assert!(tree.join(".taskmaster/skills/tidy-css/SKILL.md").is_file());

        store.remove("tidy-css").unwrap();
        assert!(!store.root().join("tidy-css").exists());
        assert!(store.remove("tidy-css").is_err());
        assert!(store.remove("../x").is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_folder_named_differently_is_found_by_its_front_matter() {
        let (store, canned, dir) = store("named");
        canned.set(
            "https://api.github.com/repos/acme/kit/git/trees/HEAD?recursive=1",
            tree(&[("x", "tree", "t"), ("x/SKILL.md", "blob", "b")]),
        );
        canned.set("https://raw.githubusercontent.com/acme/kit/HEAD/x/SKILL.md", SKILL);
        assert_eq!(store.install("acme/kit", "tidy-css").unwrap().name, "tidy-css");
        assert!(store.install("acme/kit", "nope").unwrap_err().to_string().contains("no skill called"));
        assert!(store.install("acme", "tidy-css").is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_path_that_climbs_out_is_refused_and_leaves_nothing() {
        let (store, canned, dir) = store("climb");
        canned.set(
            "https://api.github.com/repos/acme/kit/git/trees/HEAD?recursive=1",
            tree(&[("s", "tree", "t"), ("s/SKILL.md", "blob", "b"), ("s/../../evil", "blob", "e")]),
        );
        canned.set("https://raw.githubusercontent.com/acme/kit/HEAD/s/SKILL.md", SKILL);
        assert!(store.install("acme/kit", "s").is_err());
        assert!(!store.root().join("tidy-css").exists());
        assert!(std::fs::read_dir(store.root()).map(|d| d.count()).unwrap_or(0) == 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn project_skills_are_listed_and_can_be_copied_in() {
        let (store, _canned, dir) = store("import");
        let project = dir.join("project");
        std::fs::create_dir_all(project.join(".claude/skills/house-style")).unwrap();
        std::fs::write(project.join(".claude/skills/house-style/SKILL.md"), "---\nname: house-style\ndescription: Our tone.\n---\n").unwrap();
        let found = store.list(&project);
        let mine = found.iter().find(|s| s.name == "house-style").unwrap();
        assert_eq!((mine.place, mine.path.as_str()), ("project", ".claude/skills/house-style/SKILL.md"));
        let imported = store.import(&project, "house-style").unwrap();
        assert_eq!((imported.place, imported.enabled), ("store", true));
        assert_eq!(store.list(&project).iter().filter(|s| s.name == "house-style").count(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_prompt_names_skills_by_chip_or_dollar() {
        assert_eq!(asked_for("Build it with skill `tidy-css` and $web-audit, then $web-audit."), vec!["tidy-css", "web-audit"]);
        assert!(asked_for("costs $5 and $../x").is_empty());
    }
}
