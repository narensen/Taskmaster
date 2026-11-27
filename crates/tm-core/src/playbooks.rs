//! Curated repair playbooks keyed by compiler/linter error code, plus deterministic brief
//! template hashing for A/B eval comparisons.
//!
//! Playbooks are separate from per-model [`crate::errorography::Book`]: they inject short,
//! language-specific fixes only when a matching error code appears in a check failure.

use std::collections::BTreeSet;

/// One curated remediation hint for a specific compiler or linter code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playbook {
    pub code: &'static str,
    pub hint: &'static str,
}

/// Curated hints for high-frequency TypeScript, Rust, Python, and ESLint codes.
pub const PLAYBOOKS: &[Playbook] = &[
    // TypeScript
    Playbook {
        code: "TS2307",
        hint: "Cannot find module: check relative path spelling, `.js`/extension rules for ESM, and whether the package is declared in `package.json` or contracts.",
    },
    Playbook {
        code: "TS2304",
        hint: "Cannot find name: import the symbol from its owning contract/module or declare its type instead of relying on an implicit global.",
    },
    Playbook {
        code: "TS2322",
        hint: "Type not assignable: inspect the locked contract type definition and align your return/prop shape rather than casting with `any`.",
    },
    Playbook {
        code: "TS2345",
        hint: "Argument type mismatch: check the callee signature in the contract file and narrow union/nullable arguments before passing them.",
    },
    Playbook {
        code: "TS2339",
        hint: "Property does not exist on type: narrow the union/unknown value or use the exact field name declared in the contract interface.",
    },
    Playbook {
        code: "TS7006",
        hint: "Implicit `any` parameter: add an explicit parameter type annotation matching the callback or interface signature.",
    },
    Playbook {
        code: "TS2532",
        hint: "Object is possibly `undefined`: guard with an explicit nullish check or optional chaining before accessing properties.",
    },
    Playbook {
        code: "TS18046",
        hint: "Caught value is of type `unknown`: narrow with `err instanceof Error ? err.message : String(err)` before reading properties.",
    },
    // Rust
    Playbook {
        code: "E0432",
        hint: "Unresolved import: verify module visibility (`pub`), `mod` declaration in `lib.rs`/`main.rs`, and crate dependency in `Cargo.toml`.",
    },
    Playbook {
        code: "E0425",
        hint: "Cannot find value/function in scope: bring it into scope with `use` or check spelling against the owning module.",
    },
    Playbook {
        code: "E0308",
        hint: "Mismatched types: compare expected vs found types, check `Option`/`Result` wrapping, references (`&T` vs `T`), and integer widths.",
    },
    Playbook {
        code: "E0382",
        hint: "Use of moved value: borrow (`&val`), clone only where cheap, or restructure ownership so the value is consumed once.",
    },
    Playbook {
        code: "E0599",
        hint: "Method not found: verify the receiver type and import the trait that defines the method.",
    },
    Playbook {
        code: "E0277",
        hint: "Trait bound not satisfied: derive or implement the required trait (e.g. `Clone`, `Debug`, `Send + Sync`, `Serialize`) or adjust generic bounds.",
    },
    Playbook {
        code: "E0502",
        hint: "Mutable and immutable borrow overlap: end the immutable borrow before mutating, or extract the needed value before calling `&mut self`.",
    },
    // Python / Ruff
    Playbook {
        code: "F821",
        hint: "Undefined name: add the missing import or define the variable before use; check `from __future__ import annotations` for forward references.",
    },
    Playbook {
        code: "F401",
        hint: "Unused import: remove the unused symbol or re-export it via `__all__` if it is part of the public API.",
    },
    Playbook {
        code: "F841",
        hint: "Unused local variable: use the computed value, return it, or prefix with `_` if intentionally discarded.",
    },
    // ESLint
    Playbook {
        code: "NO-UNDEF",
        hint: "Undefined identifier in JS/TS: import the symbol explicitly rather than assuming a global.",
    },
    Playbook {
        code: "NO-UNUSED-VARS",
        hint: "Unused variable or import: remove dead bindings or prefix intentionally unused parameters with `_`.",
    },
    // Browser / Web verification
    Playbook {
        code: "BROKEN_IMAGE",
        hint: "An `<img>` failed to load (`naturalWidth === 0`): never invent or guess remote image URLs from memory. Use verified HTTP-200 real image URLs (check with a quick request or use real local image files saved under `public/images/` and referenced by relative/root path).",
    },
    Playbook {
        code: "FAILED_RESOURCE",
        hint: "A subresource (image, script, stylesheet, or fetch) returned an error or failed to load: verify the file exists at the exact served path, check Vite/bundler asset paths, and confirm external URLs are reachable without hotlink blocking.",
    },
    Playbook {
        code: "BLANK_PAGE",
        hint: "The page rendered completely blank: inspect runtime/console errors, verify the root mount element (`#root`/`#app`) matches the entry script, and ensure `<script type=\"module\">` paths resolve cleanly.",
    },
    Playbook {
        code: "APP_START_FAILED",
        hint: "The web app server failed to start: inspect `package.json` `dev`/`start`/`preview` scripts, verify all imported dependencies are listed in `package.json`, and ensure the server listens on `127.0.0.1` and `$PORT`.",
    },
];

/// Extract curated playbooks matching any error codes present in `errors`.
/// Returns deduplicated playbooks in deterministic table order, capped at 5 hints.
pub fn match_playbooks(errors: &str) -> Vec<Playbook> {
    let mut found_codes: BTreeSet<String> = BTreeSet::new();
    // Pull codes recognized by errorography::classify ("<tool> <code>").
    for (sig, _) in crate::errorography::classify(errors) {
        if let Some((_, code)) = sig.split_once(' ') {
            found_codes.insert(code.to_ascii_uppercase());
        }
    }
    // Also scan raw tokens for standard compiler error codes (TSxxxx, Exxxx, Fxxx) and ESLint rules.
    for token in errors.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        if token.len() < 3 || token.len() > 24 {
            continue;
        }
        let upper = token.to_ascii_uppercase();
        if PLAYBOOKS.iter().any(|p| p.code == upper) {
            found_codes.insert(upper);
        }
    }
    PLAYBOOKS
        .iter()
        .copied()
        .filter(|p| found_codes.contains(p.code))
        .take(5)
        .collect()
}

/// Format matching playbooks as a concise prompt section. Empty when no codes match.
pub fn format_playbooks(errors: &str) -> String {
    let matched = match_playbooks(errors);
    if matched.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Error-code playbooks\n\n");
    for pb in matched {
        out.push_str(&format!("- **{}**: {}\n", pb.code, pb.hint));
    }
    out
}

/// Stable 64-bit FNV-1a version hash for a brief template (16 lowercase hex chars).
/// Recorded on [`crate::ledger::LedgerEntry::brief_hash`] so evals can A/B prompt changes.
pub fn brief_hash(template: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in template.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_typescript_rust_and_python_error_codes_only_when_present() {
        let ts = "src/app.ts(12,5): error TS2307: Cannot find module './foo'\nsrc/app.ts(18,2): error TS2322: Type 'string' is not assignable";
        let matched = match_playbooks(ts);
        let codes: Vec<&str> = matched.iter().map(|p| p.code).collect();
        assert_eq!(codes, vec!["TS2307", "TS2322"]);

        let rs = "error[E0432]: unresolved import `crate::missing`\nerror[E0382]: borrow of moved value";
        let rs_codes: Vec<&str> = match_playbooks(rs).iter().map(|p| p.code).collect();
        assert_eq!(rs_codes, vec!["E0432", "E0382"]);

        let py = "ruff: F821: Undefined name `Widget`";
        let py_codes: Vec<&str> = match_playbooks(py).iter().map(|p| p.code).collect();
        assert_eq!(py_codes, vec!["F821"]);

        assert!(match_playbooks("all tests passed").is_empty());
        assert!(format_playbooks("all tests passed").is_empty());

        let browser = "In the browser at /: FAILED_RESOURCE (Image): HTTP 404 loading /horse.jpg\nBROKEN_IMAGE: 1 <img> element(s) failed to load";
        let browser_codes: Vec<&str> = match_playbooks(browser).iter().map(|p| p.code).collect();
        assert_eq!(browser_codes, vec!["BROKEN_IMAGE", "FAILED_RESOURCE"]);
    }

    #[test]
    fn brief_hash_is_deterministic_and_sensitive_to_edits() {
        let a = brief_hash("Build unit {{id}} using locked contracts.");
        let b = brief_hash("Build unit {{id}} using locked contracts.");
        let c = brief_hash("Build unit {{id}} using locked contracts and tests.");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert_ne!(a, c);
    }
}
