//! Package skills into a zip to share with colleagues.
//!
//! Read-only on skill folders. For each skill the packer decides, file by file, what goes
//! in and why anything is left out:
//! - build and dependency folders (`target/`, `node_modules/`, `.git/`, …) are skipped;
//! - anything the skill's `.gitignore` ignores is skipped;
//! - files that usually hold secrets (`.env`, keys, certificates) are never packed;
//! - text that looks like a credential is packed but flagged, so you can check first.

use std::collections::HashSet;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use walkdir::WalkDir;

use crate::skills::InstalledSkill;

/// Never packed, and not descended into.
const SKIP_DIRS: &[(&str, &str)] = &[
    (".git", "git history"),
    ("node_modules", "installed packages"),
    ("target", "build output"),
    ("__pycache__", "Python cache"),
    (".venv", "Python environment"),
    ("venv", "Python environment"),
    (".pytest_cache", "test cache"),
    (".mypy_cache", "type-checker cache"),
];

/// A package bigger than this is almost certainly carrying something it shouldn't.
pub const MAX_PACKAGE_BYTES: u64 = 100 * 1024 * 1024;
const LARGE_FILE_BYTES: u64 = 5 * 1024 * 1024;
const SCAN_LIMIT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileEntry {
    /// Relative to the skill folder, with `/` separators. Folders end in `/`.
    pub path: String,
    pub bytes: u64,
    pub included: bool,
    /// Why it was left out, or what to check before sharing.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackPlan {
    pub skill: String,
    /// Folder name inside the package.
    pub folder: String,
    pub dir: String,
    pub files: Vec<FileEntry>,
    pub included_bytes: u64,
    pub excluded_bytes: u64,
    /// Included files worth a look before sharing.
    pub warnings: Vec<String>,
}

pub fn skill_dir(skill: &InstalledSkill) -> PathBuf {
    Path::new(&skill.path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

pub fn plan(skill: &InstalledSkill) -> Result<PackPlan> {
    let dir = skill_dir(skill);
    if !dir.join("SKILL.md").is_file() {
        bail!("{} has no SKILL.md", dir.display());
    }

    // What .gitignore allows (build folders are skipped here too).
    let mut allowed = HashSet::new();
    let walker = ignore::WalkBuilder::new(&dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .filter_entry(|e| skip_reason(e.file_name()).is_none())
        .build();
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            allowed.insert(entry.into_path());
        }
    }

    let mut files = Vec::new();
    let mut walk = WalkDir::new(&dir)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter();
    while let Some(entry) = walk.next() {
        let entry = entry?;
        let rel = relative(&dir, entry.path());
        if entry.file_type().is_dir() {
            if let Some(why) = skip_reason(entry.file_name()) {
                walk.skip_current_dir();
                files.push(FileEntry {
                    path: format!("{rel}/"),
                    bytes: dir_size(entry.path()),
                    included: false,
                    note: Some(why.to_string()),
                });
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let name = entry.file_name().to_string_lossy().to_string();
        let (included, note) = if let Some(why) = secret_file(&name) {
            (false, Some(why.to_string()))
        } else if !allowed.contains(entry.path()) {
            (false, Some("ignored by .gitignore".to_string()))
        } else if let Some(why) = looks_sensitive(entry.path(), bytes) {
            (true, Some(why))
        } else if bytes > LARGE_FILE_BYTES {
            (true, Some(format!("large file ({})", human_bytes(bytes))))
        } else {
            (true, None)
        };
        files.push(FileEntry {
            path: rel,
            bytes,
            included,
            note,
        });
    }

    let included_bytes = files.iter().filter(|f| f.included).map(|f| f.bytes).sum();
    let excluded_bytes = files.iter().filter(|f| !f.included).map(|f| f.bytes).sum();
    let warnings = files
        .iter()
        .filter(|f| f.included && f.note.is_some())
        .map(|f| format!("{}: {}", f.path, f.note.as_deref().unwrap_or_default()))
        .collect();
    Ok(PackPlan {
        skill: skill.name.clone(),
        folder: folder_name(&dir, &skill.name),
        dir: dir.to_string_lossy().to_string(),
        files,
        included_bytes,
        excluded_bytes,
        warnings,
    })
}

/// Write the included files of every plan into one zip, plus an INSTALL.md.
pub fn write_zip<W: Write + Seek>(plans: &[PackPlan], out: W) -> Result<W> {
    let total: u64 = plans.iter().map(|p| p.included_bytes).sum();
    if total > MAX_PACKAGE_BYTES {
        bail!(
            "the package would be {}, over the {} limit. Check the file list for something that shouldn't be shared",
            human_bytes(total),
            human_bytes(MAX_PACKAGE_BYTES)
        );
    }
    let mut folders = HashSet::new();
    for p in plans {
        if !folders.insert(p.folder.clone()) {
            bail!("two selected skills would both be packed as '{}'", p.folder);
        }
    }

    let mut zip = zip::ZipWriter::new(out);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    zip.start_file("INSTALL.md", options)?;
    zip.write_all(install_notes(plans).as_bytes())?;
    for p in plans {
        let dir = Path::new(&p.dir);
        for f in p.files.iter().filter(|f| f.included) {
            let data = std::fs::read(dir.join(&f.path))
                .with_context(|| format!("reading {}/{}", p.dir, f.path))?;
            zip.start_file(format!("{}/{}", p.folder, f.path), options)?;
            zip.write_all(&data)?;
        }
    }
    Ok(zip.finish()?)
}

fn install_notes(plans: &[PackPlan]) -> String {
    let mut s =
        String::from("# Skills\n\nShared from calvin (https://github.com/jmelosegui/calvin).\n\n");
    for p in plans {
        s.push_str(&format!("- **{}**: `{}/`\n", p.skill, p.folder));
    }
    s.push_str(
        "\n## Install in Claude Code\n\n\
         Copy each folder into your skills folder, then start a new Claude Code session:\n\n\
         - macOS / Linux: `~/.claude/skills/`\n\
         - Windows: `%USERPROFILE%\\.claude\\skills\\`\n\n\
         To share a skill with everyone working in one repository instead, copy it into that\n\
         repository's `.claude/skills/` folder and commit it.\n\n\
         Read each SKILL.md before installing: skills can include scripts that Claude runs.\n",
    );
    s
}

fn skip_reason(name: &std::ffi::OsStr) -> Option<&'static str> {
    let name = name.to_str()?;
    SKIP_DIRS
        .iter()
        .find(|(d, _)| *d == name)
        .map(|(_, why)| *why)
}

fn secret_file(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let is = |exts: &[&str]| exts.iter().any(|e| lower.ends_with(e));
    if lower == ".env" || lower.starts_with(".env.") {
        Some("environment file, may hold secrets")
    } else if is(&[".pem", ".key", ".p12", ".pfx", ".jks", ".kdbx", ".keystore"]) {
        Some("key or certificate file")
    } else if lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
    {
        Some("SSH key")
    } else if lower == ".npmrc"
        || lower == ".pypirc"
        || lower == ".netrc"
        || lower == ".git-credentials"
    {
        Some("credentials file")
    } else {
        None
    }
}

/// Flag text that looks like a credential. Cheap substring checks, not a secret scanner.
fn looks_sensitive(path: &Path, bytes: u64) -> Option<String> {
    if bytes > SCAN_LIMIT_BYTES {
        return None;
    }
    let text = String::from_utf8(std::fs::read(path).ok()?).ok()?;
    let markers: &[(&str, &str)] = &[
        ("PRIVATE KEY-----", "contains a private key"),
        ("AKIA", "may contain an AWS access key"),
        ("ghp_", "may contain a GitHub token"),
        ("github_pat_", "may contain a GitHub token"),
        ("sk-ant-", "may contain an Anthropic API key"),
        ("xoxb-", "may contain a Slack token"),
        ("xoxp-", "may contain a Slack token"),
        ("AccountKey=", "may contain an Azure storage key"),
        ("SharedAccessSignature=", "may contain an Azure SAS token"),
        ("Password=", "may contain a connection-string password"),
    ];
    markers
        .iter()
        .find(|(m, _)| {
            text.match_indices(m).any(|(i, _)| {
                // Require something after the marker, so docs that merely mention it pass.
                let rest = &text[i + m.len()..];
                rest.chars()
                    .take(12)
                    .filter(|c| c.is_ascii_alphanumeric())
                    .count()
                    >= 8
            })
        })
        .map(|(_, why)| why.to_string())
}

fn folder_name(dir: &Path, skill: &str) -> String {
    let from_dir = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let base = if from_dir.is_empty() {
        skill.rsplit(':').next().unwrap_or(skill).to_string()
    } else {
        from_dir
    };
    base.chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn dir_size(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / (1u64 << 10) as f64),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn skill_at(dir: &Path) -> InstalledSkill {
        InstalledSkill {
            name: "demo".into(),
            description: "Demo".into(),
            kind: "global".into(),
            detail: None,
            path: dir.join("SKILL.md").to_string_lossy().to_string(),
        }
    }

    fn write(path: PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn plan_skips_builds_secrets_and_gitignored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("demo");
        write(dir.join("SKILL.md"), "---\nname: demo\n---\nDo things.");
        write(dir.join("scripts/run.sh"), "echo hi");
        write(dir.join("target/debug/demo.exe"), "binary");
        write(dir.join("node_modules/x/index.js"), "x");
        write(dir.join(".env"), "TOKEN=abc");
        write(dir.join(".gitignore"), "out/\n");
        write(dir.join("out/result.txt"), "generated");
        write(
            dir.join("notes.md"),
            "key: ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        );
        write(
            dir.join("docs.md"),
            "Tokens look like ghp_... never paste them.",
        );

        let p = plan(&skill_at(&dir)).unwrap();
        let find = |path: &str| {
            p.files
                .iter()
                .find(|f| f.path == path)
                .unwrap_or_else(|| panic!("{path} missing"))
        };

        assert!(find("SKILL.md").included);
        assert!(find("scripts/run.sh").included);
        assert!(find(".gitignore").included);
        assert!(!find("target/").included);
        assert!(!find("node_modules/").included);
        assert!(!find(".env").included);
        assert_eq!(
            find("out/result.txt").note.as_deref(),
            Some("ignored by .gitignore")
        );
        assert!(find("notes.md").included);
        assert_eq!(
            find("notes.md").note.as_deref(),
            Some("may contain a GitHub token")
        );
        assert_eq!(
            find("docs.md").note,
            None,
            "a mention without a value is not flagged"
        );
        assert_eq!(p.warnings.len(), 1);
        assert!(
            p.files.iter().all(|f| !f.path.starts_with("target/debug")),
            "skipped folders are not listed file by file"
        );
    }

    #[test]
    fn zip_contains_only_included_files_and_install_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("demo");
        write(dir.join("SKILL.md"), "---\nname: demo\n---\n");
        write(dir.join("ref/guide.md"), "guide");
        write(dir.join(".env"), "SECRET=1");

        let p = plan(&skill_at(&dir)).unwrap();
        let bytes = write_zip(&[p], std::io::Cursor::new(Vec::new()))
            .unwrap()
            .into_inner();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<_> = archive.file_names().map(str::to_string).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["INSTALL.md", "demo/SKILL.md", "demo/ref/guide.md"]
        );
        let mut install = String::new();
        archive
            .by_name("INSTALL.md")
            .unwrap()
            .read_to_string(&mut install)
            .unwrap();
        assert!(install.contains("~/.claude/skills/"));
    }
}
