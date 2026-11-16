//! Entries come from people's projects, so one can carry a credential by accident. An
//! entry that looks like it holds one is refused before anyone else can see it.

use std::sync::OnceLock;

use regex::Regex;

const PATTERNS: [(&str, &str); 13] = [
    ("an AWS access key", r"\b(AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("a GitHub token", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    ("a GitHub token", r"\bgithub_pat_[A-Za-z0-9_]{40,}\b"),
    ("an Anthropic key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}"),
    ("an OpenAI key", r"\bsk-(proj-)?[A-Za-z0-9_\-]{32,}"),
    ("a Slack token", r"\bxox[abposr]-[A-Za-z0-9\-]{10,}"),
    ("a Google API key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
    ("a Stripe key", r"\b(sk|rk)_live_[0-9A-Za-z]{20,}\b"),
    ("a private key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    ("a database URL with a password", r"\b(postgres(ql)?|mysql|mongodb(\+srv)?|redis|amqp)://[^\s:/@]+:[^\s@/]+@"),
    ("a Taskmaster token", r"\btmc_[0-9a-f]{40,}\b"),
    ("a signed token (JWT)", r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
    ("a password", r#"(?i)\b(password|passwd|secret|api_?key)\s*[:=]\s*["'][^"'\s]{8,}["']"#),
];

fn compiled() -> &'static Vec<(&'static str, Regex)> {
    static SET: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    SET.get_or_init(|| PATTERNS.iter().map(|(name, pattern)| (*name, Regex::new(pattern).expect("a valid pattern"))).collect())
}

/// What `text` seems to contain, one name per kind of credential.
pub fn credentials(text: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> =
        compiled().iter().filter(|(_, regex)| regex.is_match(text)).map(|(name, _)| *name).collect();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_found_and_ordinary_code_is_not() {
        let fake_aws = format!("const k = '{}{}';", "AKIA", "ABCDEFGHIJKLMNOP");
        assert_eq!(credentials(&fake_aws), vec!["an AWS access key"]);
        assert_eq!(credentials("DATABASE_URL=postgres://app:hunter22@db.neon.tech/x"), vec!["a database URL with a password"]);
        assert_eq!(credentials("-----BEGIN OPENSSH PRIVATE KEY-----"), vec!["a private key"]);
        assert_eq!(credentials(r#"const password = "correct-horse-battery";"#), vec!["a password"]);
        assert!(credentials("export const money = (n: number) => n.toFixed(2);\nconst password = props.password;").is_empty());
        assert!(credentials("postgres://localhost/db").is_empty());
    }
}
