//! A starting point for a new project.
//!
//! A template is a set of shared files plus a note for the planner. The files are written
//! when the contracts are locked, and a file the project already has is left alone. The
//! note, `TEMPLATE.md`, says where pages live and what they may import; it is given to the
//! planner and is not copied into the project.
//!
//! `next` is built in. Any other name is a directory laid out the same way.

use std::path::Path;

use anyhow::{Context, bail};
use tm_core::ContractFile;

const GUIDE: &str = "TEMPLATE.md";
const MAX_FILES: usize = 200;
const MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    /// What the planner is told about the layout. Empty when the template has no note.
    pub guide: String,
    pub files: Vec<ContractFile>,
}

const NEXT: [(&str, &str); 7] = [
    ("package.json", include_str!("../templates/next/package.json")),
    ("tsconfig.json", include_str!("../templates/next/tsconfig.json")),
    ("next-env.d.ts", include_str!("../templates/next/next-env.d.ts")),
    ("next.config.mjs", include_str!("../templates/next/next.config.mjs")),
    // Stored without the dot so the template's own ignore file does not act on this repo.
    (".gitignore", include_str!("../templates/next/gitignore")),
    ("app/layout.tsx", include_str!("../templates/next/app/layout.tsx")),
    ("app/globals.css", include_str!("../templates/next/app/globals.css")),
];

/// The templates every daemon has. Any directory with the same layout works too.
pub const BUILT_IN: [&str; 1] = ["next"];

pub fn load(name: &str) -> anyhow::Result<Template> {
    if name == "next" {
        return Ok(Template {
            name: name.into(),
            guide: include_str!("../templates/next/TEMPLATE.md").trim().to_string(),
            files: NEXT.iter().map(|(path, content)| ContractFile { path: (*path).into(), content: (*content).into() }).collect(),
        });
    }
    let root = Path::new(name);
    if !root.is_dir() {
        bail!("no template `{name}`: use `next` or a directory");
    }
    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let guide = match files.iter().position(|file| file.path == GUIDE) {
        Some(index) => files.remove(index).content.trim().to_string(),
        None => String::new(),
    };
    Ok(Template { name: name.into(), guide, files })
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<ContractFile>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if name != ".git" && name != "node_modules" {
                collect(root, &path, out)?;
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        if entry.metadata()?.len() > MAX_BYTES {
            bail!("template file {} is larger than {} KB", path.display(), MAX_BYTES / 1024);
        }
        // A template is source text. A file that is not UTF-8 is not part of one.
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        out.push(ContractFile { path: rel, content });
        if out.len() > MAX_FILES {
            bail!("template {} has more than {MAX_FILES} files", root.display());
        }
    }
    Ok(())
}

impl Template {
    /// The part of the planner's brief that describes this template.
    pub fn brief(&self) -> String {
        let mut out = format!("\nThe project starts from the `{}` template. These files will exist and are shared:\n", self.name);
        for file in &self.files {
            out.push_str(&format!("- `{}`\n", file.path));
        }
        if !self.guide.is_empty() {
            out.push_str(&format!("\n{}\n", self.guide));
        }
        out.push_str("Do not repeat a template file as a contract unless its content must change.\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_built_in_and_tells_the_planner_where_pages_go() {
        let next = load("next").unwrap();
        assert!(next.files.iter().any(|file| file.path == "package.json"));
        assert!(next.files.iter().any(|file| file.path == ".gitignore" && file.content.contains("node_modules")));
        assert!(!next.files.iter().any(|file| file.path == GUIDE));
        let brief = next.brief();
        assert!(brief.contains("`app/layout.tsx`") && brief.contains("app/<page id>/page.tsx"), "{brief}");
        assert!(load("no-such-template").is_err());
    }

    #[test]
    fn a_directory_is_a_template_and_its_note_stays_out_of_the_project() {
        let dir = std::env::temp_dir().join(format!("tm-template-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("src/main.py"), "print('hi')\n").unwrap();
        std::fs::write(dir.join("node_modules/x/index.js"), "x").unwrap();
        std::fs::write(dir.join(GUIDE), "Pages live in `src/<id>/`.\n").unwrap();
        let template = load(dir.to_str().unwrap()).unwrap();
        assert_eq!(template.files.len(), 1);
        assert_eq!(template.files[0].path, "src/main.py");
        assert_eq!(template.guide, "Pages live in `src/<id>/`.");
        let _ = std::fs::remove_dir_all(dir);
    }
}
