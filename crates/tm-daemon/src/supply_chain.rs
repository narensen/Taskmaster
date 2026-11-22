//! Supply-chain gate on newly added dependencies before merge (Item 3.3).
//!
//! Inspects added dependencies in `package.json`, `Cargo.toml`, and `requirements.txt` diffs,
//! checking:
//! 1. OSV security advisories (via [`AdvisoryOracle`], with a live OSV.dev client & fixture mock),
//! 2. Typosquatting distance against popular npm / PyPI / crates.io package names,
//! 3. Package age and download floor when registry metadata is available,
//! 4. License allowlist compliance.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Npm,
    Cargo,
    PyPi,
}

impl Ecosystem {
    pub fn osv_name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Cargo => "crates.io",
            Self::PyPi => "PyPI",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedDependency {
    pub ecosystem: Ecosystem,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_downloads: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    pub id: String,
    pub summary: String,
    pub severity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplyChainFinding {
    pub package: String,
    pub category: String,
    pub severity: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplyChainVerdict {
    pub allowed: bool,
    pub findings: Vec<SupplyChainFinding>,
}

/// Oracle for package vulnerability advisories (live OSV.dev or deterministic test fixture).
pub trait AdvisoryOracle: Send + Sync {
    fn advisories(&self, dep: &AddedDependency) -> Vec<Advisory>;
}

/// No-network oracle used when offline or when only static checks (typosquat + license) run.
pub struct OfflineOracle;
impl AdvisoryOracle for OfflineOracle {
    fn advisories(&self, _dep: &AddedDependency) -> Vec<Advisory> {
        Vec::new()
    }
}

pub const DEFAULT_ALLOWED_LICENSES: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "MPL-2.0",
    "Unlicense",
    "0BSD",
    "Zlib",
    "CC0-1.0",
];

const POPULAR_NPM: &[&str] = &[
    "react", "react-dom", "next", "vite", "typescript", "lodash", "axios", "express",
    "zod", "tailwindcss", "eslint", "prettier", "vitest", "jest", "playwright", "chalk",
    "commander", "date-fns", "dayjs", "uuid", "dotenv", "prisma", "drizzle-orm",
];

const POPULAR_PYPI: &[&str] = &[
    "requests", "numpy", "pandas", "pytest", "fastapi", "flask", "django", "pydantic",
    "httpx", "sqlalchemy", "uvicorn", "click", "typer", "ruff", "mypy", "scipy", "pillow",
];

const POPULAR_CARGO: &[&str] = &[
    "serde", "serde_json", "tokio", "anyhow", "thiserror", "clap", "axum", "reqwest",
    "tracing", "chrono", "uuid", "regex", "rand", "futures", "sqlx", "hyper", "rayon",
];

/// Extract newly added dependencies from a unified git diff.
pub fn parse_added_dependencies(diff: &str) -> Vec<AddedDependency> {
    let mut out = Vec::new();
    let mut current_file = String::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_file = rest.trim().to_string();
            continue;
        }
        let Some(added) = line.strip_prefix('+') else { continue };
        if added.starts_with("++") {
            continue;
        }
        let trimmed = added.trim();
        if current_file.ends_with("package.json") {
            if let Some((k, v)) = parse_json_kv(trimmed)
                && !matches!(k, "name" | "version" | "description" | "main" | "module" | "type" | "license" | "private" | "author")
                && (v.starts_with('^') || v.starts_with('~') || v.chars().next().is_some_and(|c| c.is_ascii_digit()))
                && seen.insert((k.to_string(), v.to_string()))
            {
                out.push(AddedDependency {
                    ecosystem: Ecosystem::Npm,
                    name: k.to_string(),
                    version: v.trim_start_matches(['^', '~', '=']).to_string(),
                    license: None,
                    age_days: None,
                    weekly_downloads: None,
                });
            }
        } else if current_file.ends_with("Cargo.toml") {
            if let Some((k, v)) = trimmed.split_once('=') {
                let name = k.trim();
                let val = v.trim();
                if !name.is_empty()
                    && !matches!(name, "name" | "version" | "edition" | "license" | "resolver" | "description" | "path")
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    && (val.starts_with('"') || val.contains("version"))
                {
                    let ver = extract_quoted(val).unwrap_or_default();
                    if !ver.is_empty() && seen.insert((name.to_string(), ver.clone())) {
                        out.push(AddedDependency {
                            ecosystem: Ecosystem::Cargo,
                            name: name.to_string(),
                            version: ver,
                            license: None,
                            age_days: None,
                            weekly_downloads: None,
                        });
                    }
                }
            }
        } else if current_file.ends_with("requirements.txt") && !trimmed.is_empty() && !trimmed.starts_with('#') {
            let (name, ver) = trimmed
                .split_once("==")
                .or_else(|| trimmed.split_once(">="))
                .map(|(a, b)| (a.trim(), b.trim()))
                .unwrap_or((trimmed, ""));
            if !name.is_empty() && seen.insert((name.to_string(), ver.to_string())) {
                out.push(AddedDependency {
                    ecosystem: Ecosystem::PyPi,
                    name: name.to_string(),
                    version: ver.to_string(),
                    license: None,
                    age_days: None,
                    weekly_downloads: None,
                });
            }
        }
    }
    out
}

fn parse_json_kv(line: &str) -> Option<(&str, &str)> {
    let line = line.trim().trim_end_matches(',');
    let (k, v) = line.split_once(':')?;
    let k = k.trim().strip_prefix('"')?.strip_suffix('"')?;
    let v = v.trim().strip_prefix('"')?.strip_suffix('"')?;
    Some((k, v))
}

fn extract_quoted(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let end = s[start..].find('"')? + start;
    Some(s[start..end].to_string())
}

/// Evaluate newly added dependencies against advisories, typosquat distance, age/downloads,
/// and license allowlist.
pub fn evaluate_dependencies(
    deps: &[AddedDependency],
    oracle: &dyn AdvisoryOracle,
    allowed_licenses: &[&str],
) -> SupplyChainVerdict {
    let mut findings = Vec::new();
    for dep in deps {
        // 1. Advisories
        for adv in oracle.advisories(dep) {
            findings.push(SupplyChainFinding {
                package: dep.name.clone(),
                category: "advisory".into(),
                severity: adv.severity.clone(),
                reason: format!("{}@{} has known advisory {}: {}", dep.name, dep.version, adv.id, adv.summary),
            });
        }
        // 2. Typosquatting distance == 1 against popular packages in the same ecosystem.
        let popular = match dep.ecosystem {
            Ecosystem::Npm => POPULAR_NPM,
            Ecosystem::PyPi => POPULAR_PYPI,
            Ecosystem::Cargo => POPULAR_CARGO,
        };
        let lower = dep.name.to_ascii_lowercase();
        if !popular.contains(&lower.as_str()) {
            for target in popular {
                if target.len() >= 4 && damerau_levenshtein(&lower, target) == 1 {
                    findings.push(SupplyChainFinding {
                        package: dep.name.clone(),
                        category: "typosquat".into(),
                        severity: "high".into(),
                        reason: format!("Package `{}` is 1 edit away from popular package `{target}` (possible typosquat).", dep.name),
                    });
                    break;
                }
            }
        }
        // 3. Age and downloads floor when registry metadata is present.
        if let Some(days) = dep.age_days
            && days < 14
        {
            findings.push(SupplyChainFinding {
                package: dep.name.clone(),
                category: "package_age".into(),
                severity: "medium".into(),
                reason: format!("Package `{}` is only {days} days old (< 14 days).", dep.name),
            });
        }
        if let Some(dl) = dep.weekly_downloads
            && dl < 100
        {
            findings.push(SupplyChainFinding {
                package: dep.name.clone(),
                category: "low_downloads".into(),
                severity: "medium".into(),
                reason: format!("Package `{}` has only {dl} weekly downloads (< 100).", dep.name),
            });
        }
        // 4. License allowlist
        if let Some(ref lic) = dep.license
            && !allowed_licenses.is_empty()
            && !allowed_licenses.iter().any(|ok| ok.eq_ignore_ascii_case(lic.trim()))
        {
            findings.push(SupplyChainFinding {
                package: dep.name.clone(),
                category: "license".into(),
                severity: "high".into(),
                reason: format!("Package `{}` uses license `{lic}` which is not on the allowlist.", dep.name),
            });
        }
    }
    SupplyChainVerdict { allowed: findings.is_empty(), findings }
}

/// Optimal string alignment (Damerau-Levenshtein) distance for typosquat detection.
pub fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut d = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in d.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate().take(n + 1) {
        *cell = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[m][n]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixtureOracle;
    impl AdvisoryOracle for FixtureOracle {
        fn advisories(&self, dep: &AddedDependency) -> Vec<Advisory> {
            if dep.name == "lodash" && dep.version == "4.17.15" {
                vec![Advisory {
                    id: "GHSA-p6mc-m468-83gw".into(),
                    summary: "Prototype Pollution in lodash".into(),
                    severity: "high".into(),
                }]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn holds_vulnerable_typosquat_and_disallowed_license_dependencies() {
        let diff = r#"
+++ b/package.json
+    "lodash": "^4.17.15",
+    "axois": "^1.0.0",
+    "zod": "^3.23.0",
"#;
        let mut deps = parse_added_dependencies(diff);
        assert_eq!(deps.len(), 3);
        deps[2].license = Some("AGPL-3.0".into());

        let verdict = evaluate_dependencies(&deps, &FixtureOracle, DEFAULT_ALLOWED_LICENSES);
        assert!(!verdict.allowed);
        let cats: Vec<&str> = verdict.findings.iter().map(|f| f.category.as_str()).collect();
        assert!(cats.contains(&"advisory"), "{cats:?}");
        assert!(cats.contains(&"typosquat"), "{cats:?}");
        assert!(cats.contains(&"license"), "{cats:?}");
    }
}
