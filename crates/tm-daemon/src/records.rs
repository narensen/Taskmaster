//! Project files for the per-model errorography and the citation graph.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use tm_core::{Book, Citations};

pub fn book_path(project: &Path) -> PathBuf {
    project.join(".taskmaster/errorography.json")
}

pub fn citation_path(project: &Path) -> PathBuf {
    project.join(".taskmaster/citations.json")
}

pub fn load_book(project: &Path) -> anyhow::Result<Book> {
    load_or_default(&book_path(project))
}

pub fn save_book(project: &Path, book: &Book) -> anyhow::Result<()> {
    atomic_write(&book_path(project), &serde_json::to_vec_pretty(book)?)
}

pub fn load_citations(project: &Path) -> anyhow::Result<Citations> {
    load_or_default(&citation_path(project))
}

pub fn save_citations(project: &Path, citations: &Citations) -> anyhow::Result<()> {
    atomic_write(&citation_path(project), &serde_json::to_vec_pretty(citations)?)
}

pub fn save_pretty(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

fn load_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    let Ok(bytes) = fs::read(path) else { return Ok(T::default()) };
    if bytes.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).with_context(|| format!("cannot read {}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
