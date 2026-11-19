//! Which free checks apply to a project, and how long each took.
//!
//! Default verification never calls a model: static checks, the project's own build and
//! tests, and one quick browser smoke check. A check whose tool is missing is left out;
//! nothing is installed.

use std::path::Path;
use std::time::Duration;

use crate::{runtime, static_site};

/// Static checks (type checker, linter, html sanity) may take this long.
pub const STATIC_TIMEOUT: Duration = Duration::from_secs(60);
/// The project's own build or test script may take this long, per step.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(180);
/// Opening the start page in a headless browser may take this long.
pub const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Html,
    Tsc,
    Eslint,
    Clippy,
    Ruff,
    Build,
    Test,
    Smoke,
}

impl Check {
    pub fn name(self) -> &'static str {
        match self {
            Check::Html => "html",
            Check::Tsc => "tsc",
            Check::Eslint => "eslint",
            Check::Clippy => "clippy",
            Check::Ruff => "ruff",
            Check::Build => "build",
            Check::Test => "test",
            Check::Smoke => "smoke",
        }
    }

    /// Static checks run first, and together with the build.
    pub fn is_static(self) -> bool {
        matches!(self, Check::Html | Check::Tsc | Check::Eslint | Check::Clippy | Check::Ruff)
    }
}

fn on_path(program: &str) -> bool {
    let names = tm_harness::sandbox::program_names(program);
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| names.iter().any(|name| dir.join(name).is_file())))
}

fn has(files: &[String], exts: &[&str]) -> bool {
    files.iter().any(|file| exts.iter().any(|ext| file.ends_with(&format!(".{ext}"))))
}

/// A project that is only HTML: no script of its own, and a start page to open.
pub fn is_static_site(tree: &Path) -> bool {
    static_site::has_no_scripts(tree) && static_site::start_page(tree).is_some() && !tree.join("Cargo.toml").is_file()
}

/// The checks that apply to `tree`, in the order they are reported. `changed` is what the
/// run built. Cheap file tests only: no tool is started to find out.
pub fn select(tree: &Path, changed: &[String]) -> Vec<Check> {
    let mut out = Vec::new();
    let bin = |name: &str| tree.join("node_modules/.bin").join(name).exists();
    if is_static_site(tree) {
        out.push(Check::Html);
        out.push(Check::Smoke);
        return out;
    }
    if tree.join("tsconfig.json").is_file() && has(changed, &["ts", "tsx", "mts", "cts"]) && bin("tsc") {
        out.push(Check::Tsc);
    }
    let eslint_config = ["eslint.config.js", "eslint.config.mjs", "eslint.config.cjs", ".eslintrc", ".eslintrc.js", ".eslintrc.json", ".eslintrc.cjs"]
        .iter()
        .any(|file| tree.join(file).is_file());
    if eslint_config && has(changed, &["js", "jsx", "ts", "tsx", "mjs", "cjs"]) && bin("eslint") {
        out.push(Check::Eslint);
    }
    if tree.join("Cargo.toml").is_file() && has(changed, &["rs"]) && on_path("cargo") {
        out.push(Check::Clippy);
    }
    if has(changed, &["py"]) && on_path("ruff") {
        out.push(Check::Ruff);
    }
    for step in runtime::steps(tree) {
        match step.name.as_str() {
            "build" => out.push(Check::Build),
            "test" => out.push(Check::Test),
            _ => {}
        }
    }
    if std::fs::read_to_string(tree.join("package.json")).is_ok_and(|text| text.contains("\"next\"")) {
        out.push(Check::Smoke);
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Timing {
    pub name: String,
    pub took: Duration,
    /// `ok`, `failed`, `timeout` or `skipped`.
    pub status: &'static str,
}

impl Timing {
    pub fn new(name: impl Into<String>, took: Duration, status: &'static str) -> Self {
        Timing { name: name.into(), took, status }
    }
}

/// `verify: eslint 1.2s ok, smoke 3.4s ok`
pub fn report(timings: &[Timing]) -> String {
    if timings.is_empty() {
        return "verify: no checks apply".into();
    }
    let parts: Vec<String> = timings
        .iter()
        .map(|t| format!("{} {:.1}s {}", t.name, t.took.as_secs_f64(), t.status))
        .collect();
    format!("verify: {}", parts.join(", "))
}

/// Obvious breakage in a static page: nothing in it, or no document at all. Returns the
/// problem per file.
pub fn html_sanity(tree: &Path, files: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for file in files.iter().filter(|f| f.ends_with(".html") || f.ends_with(".htm")) {
        let Ok(text) = std::fs::read_to_string(tree.join(file)) else { continue };
        let lower = text.to_ascii_lowercase();
        let problem = if text.trim().is_empty() {
            Some("the page is empty")
        } else if !lower.contains("<html") && !lower.contains("<!doctype") && !lower.contains("<body") {
            Some("the page has no <html> or <body>")
        } else if lower.contains("<html") && !lower.contains("</html>") {
            Some("the page is cut off: no closing </html>")
        } else if lower.contains("<script") && lower.matches("<script").count() > lower.matches("</script>").count() {
            Some("a <script> tag is never closed")
        } else {
            None
        };
        if let Some(problem) = problem {
            out.push((file.clone(), format!("{file}:1 {problem}")));
        }
    }
    out
}

/// Quality bars a static page must meet, checked from its files: a viewport meta tag, some
/// CSS applied, and a label or aria-label on every form control, including controls the
/// page's scripts create. Returns the problem per file, like [`html_sanity`].
pub fn html_quality(tree: &Path, files: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for file in files.iter().filter(|f| f.ends_with(".html") || f.ends_with(".htm")) {
        let Ok(text) = std::fs::read_to_string(tree.join(file)) else { continue };
        let lower = text.to_ascii_lowercase();
        if lower.trim().is_empty() {
            continue;
        }
        if !["name=\"viewport\"", "name='viewport'", "name=viewport"].iter().any(|v| lower.contains(v)) {
            out.push((file.clone(), format!("{file}:1 no viewport meta tag: add <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"> to <head>")));
        }
        let linked = lower.contains("<style") || (lower.contains("<link") && lower.contains("stylesheet"));
        if !linked && !lower.contains("style=") {
            out.push((file.clone(), format!("{file}:1 the page is unstyled: no stylesheet is linked and no <style> is present")));
        }
        for (line, control) in unlabeled_controls(&text) {
            out.push((file.clone(), format!("{file}:{line} {control} has no label: give it a <label for> or aria-label")));
        }
    }
    for file in files.iter().filter(|f| f.ends_with(".js") || f.ends_with(".mjs")) {
        let Ok(text) = std::fs::read_to_string(tree.join(file)) else { continue };
        let creates = text.contains("createElement(\"input\")") || text.contains("createElement('input')") || text.contains("createElement(`input`)");
        let templated = text.contains("<input");
        let labelled = text.contains("aria-label") || text.contains("ariaLabel") || text.contains("createElement(\"label\")") || text.contains("createElement('label')") || text.contains("<label") || text.contains("aria-labelledby") || text.contains("labels");
        if (creates || templated) && !labelled {
            let line = text.lines().position(|l| l.contains("createElement") && l.contains("input") || l.contains("<input")).map_or(1, |i| i + 1);
            out.push((file.clone(), format!("{file}:{line} an input made in script has no label: set aria-label or wrap it in a <label>")));
        }
    }
    out
}

/// Form controls in an HTML document with no label: not inside a `<label>`, no `aria-label`
/// or `aria-labelledby`, and no `<label for>` naming their id. Hidden and button inputs are
/// skipped. Returns the 1-based line and a short name for each.
fn unlabeled_controls(html: &str) -> Vec<(usize, String)> {
    let lower = html.to_ascii_lowercase();
    let label_for: Vec<String> = lower
        .match_indices("<label")
        .filter_map(|(at, _)| {
            let tag = &lower[at..at + lower[at..].find('>').unwrap_or(0)];
            attr(tag, "for")
        })
        .collect();
    let mut out = Vec::new();
    for name in ["<input", "<select", "<textarea"] {
        for (at, _) in lower.match_indices(name) {
            let end = lower[at..].find('>').map_or(lower.len(), |e| at + e);
            let tag = &lower[at..end];
            let kind = attr(tag, "type").unwrap_or_default();
            if name == "<input" && matches!(kind.as_str(), "hidden" | "submit" | "button" | "reset" | "image") {
                continue;
            }
            if tag.contains("aria-label") || tag.contains("aria-labelledby") || tag.contains("title=") {
                continue;
            }
            if attr(tag, "id").is_some_and(|id| label_for.contains(&id)) {
                continue;
            }
            let before = &lower[..at];
            if before.matches("<label").count() > before.matches("</label").count() {
                continue;
            }
            let line = html[..at].matches('\n').count() + 1;
            let what = if kind.is_empty() { name[1..].to_string() } else { format!("{} type={kind}", &name[1..]) };
            out.push((line, format!("<{what}>")));
        }
    }
    out
}

/// The value of `name` in an opening tag (already lowercased), quoted or not.
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let boundary = at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[at + name.len()..].trim_start();
        if !boundary || !rest.starts_with('=') {
            continue;
        }
        let rest = rest[1..].trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or_default(),
            _ => rest.split(|c: char| c.is_ascii_whitespace() || c == '/').next().unwrap_or_default(),
        };
        return Some(value.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tm-checks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_static_site_gets_html_sanity_and_the_smoke_check_only() {
        let tree = dir("static");
        std::fs::write(tree.join("index.html"), "<html><body>hi</body></html>").unwrap();
        assert_eq!(select(&tree, &["index.html".into()]), vec![Check::Html, Check::Smoke]);
        let _ = std::fs::remove_dir_all(tree);
    }

    #[test]
    fn a_node_project_gets_tsc_eslint_and_build_when_the_tools_exist() {
        let tree = dir("node");
        std::fs::write(tree.join("package.json"), r#"{"scripts":{"build":"next build","dev":"next dev"},"dependencies":{"next":"1"}}"#).unwrap();
        std::fs::write(tree.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(tree.join("eslint.config.mjs"), "export default []").unwrap();
        let changed = vec!["app/page.tsx".to_string()];
        // No node_modules: every tool is skipped, quickly, and nothing is installed.
        assert!(select(&tree, &changed).iter().all(|c| !matches!(c, Check::Tsc | Check::Eslint | Check::Build)));
        std::fs::create_dir_all(tree.join("node_modules/.bin")).unwrap();
        std::fs::write(tree.join("node_modules/.bin/tsc"), "").unwrap();
        std::fs::write(tree.join("node_modules/.bin/eslint"), "").unwrap();
        let picked = select(&tree, &changed);
        assert!(picked.contains(&Check::Tsc) && picked.contains(&Check::Eslint));
        assert!(!picked.contains(&Check::Html));
        if on_path("npm") {
            assert!(picked.contains(&Check::Build));
        }
        assert_eq!(picked.last(), Some(&Check::Smoke));
        // Untyped changes do not need the type checker.
        assert!(!select(&tree, &["app/x.css".into()]).contains(&Check::Tsc));
        let _ = std::fs::remove_dir_all(tree);
    }

    #[test]
    fn the_report_names_each_check_with_its_duration() {
        let text = report(&[
            Timing::new("eslint", Duration::from_millis(1234), "ok"),
            Timing::new("smoke", Duration::from_millis(3400), "failed"),
        ]);
        assert_eq!(text, "verify: eslint 1.2s ok, smoke 3.4s failed");
        assert_eq!(report(&[]), "verify: no checks apply");
    }

    #[test]
    fn html_sanity_flags_empty_and_cut_off_pages() {
        let tree = dir("html");
        std::fs::write(tree.join("a.html"), "<!doctype html><html><body></body></html>").unwrap();
        std::fs::write(tree.join("b.html"), "  ").unwrap();
        std::fs::write(tree.join("c.html"), "<html><body>x").unwrap();
        let files: Vec<String> = ["a.html", "b.html", "c.html"].map(String::from).to_vec();
        let found = html_sanity(&tree, &files);
        assert_eq!(found.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), vec!["b.html", "c.html"]);
        let _ = std::fs::remove_dir_all(tree);
    }

    #[test]
    fn quality_bars_catch_a_bare_page_and_pass_a_finished_one() {
        let tree = dir("quality");
        std::fs::write(
            tree.join("bare.html"),
            "<!doctype html><html><body><form><input id=\"t\" type=\"text\"><button>Add</button></form><script src=\"a.js\"></script></body></html>",
        )
        .unwrap();
        std::fs::write(tree.join("a.js"), "const c = document.createElement('input'); c.type = 'checkbox';").unwrap();
        std::fs::write(
            tree.join("good.html"),
            "<!doctype html><html><head><meta name=\"viewport\" content=\"width=device-width\"><link rel=\"stylesheet\" href=\"s.css\"></head>\
             <body><label for=\"t\">New todo</label><input id=\"t\" type=\"text\"><label><input type=\"checkbox\"> Done</label>\
             <input type=\"search\" aria-label=\"Filter\"><input type=\"hidden\" name=\"x\"></body></html>",
        )
        .unwrap();
        std::fs::write(tree.join("b.js"), "const c = document.createElement('input'); c.setAttribute('aria-label', 'Done');").unwrap();
        let bare = html_quality(&tree, &["bare.html".into(), "a.js".into()]);
        let text: Vec<&str> = bare.iter().map(|(_, l)| l.as_str()).collect();
        assert!(text.iter().any(|l| l.contains("viewport")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("unstyled")), "{text:?}");
        assert!(text.iter().any(|l| l.starts_with("bare.html:1 <input type=text> has no label")), "{text:?}");
        assert!(text.iter().any(|l| l.starts_with("a.js:1 an input made in script")), "{text:?}");
        assert!(html_quality(&tree, &["good.html".into(), "b.js".into()]).is_empty());
        let _ = std::fs::remove_dir_all(tree);
    }
}
