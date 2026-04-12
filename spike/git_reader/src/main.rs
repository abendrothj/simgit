//! Spike: read a file at an arbitrary commit from a git repo via gitoxide.
//!
//! Validates that `gix` can stream file contents from the object DB
//! without requiring a working-tree checkout.
//!
//! Usage:  git_reader <repo-path> <commit-ish> <file-path>
//!
//! Example:
//!   git_reader /home/user/myrepo HEAD src/main.rs

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        bail!("Usage: git_reader <repo-path> <commit-ish> <file-path>");
    }

    let repo_path   = PathBuf::from(&args[1]).canonicalize()?;
    let commit_ish  = &args[2];
    let file_path   = &args[3];

    let repo = gix::open(&repo_path)
        .with_context(|| format!("open repo: {}", repo_path.display()))?;

    // Resolve commit.
    let commit_id = repo
        .rev_parse_single(commit_ish.as_str())
        .with_context(|| format!("resolve {commit_ish}"))?;

    let commit = commit_id.object()?.into_commit();
    let tree   = commit.tree()?;

    // Walk the tree to find the file entry.
    let entry = tree
        .lookup_entry_by_path(file_path)
        .with_context(|| format!("lookup {file_path} in tree"))?
        .ok_or_else(|| anyhow::anyhow!("file not found in commit: {file_path}"))?;

    let blob = entry.object()?.into_blob();
    let content = std::str::from_utf8(blob.data.as_ref())
        .unwrap_or("<binary content>");

    println!("=== {file_path} @ {commit_ish} ({commit_id}) ===");
    print!("{content}");
    println!("=== {} bytes ===", blob.data.len());

    Ok(())
}
