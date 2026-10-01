//! Finds installed skills and where each came from, so usage can be compared against
//! what's available.
//!
//! Read-only: calvin never writes to skill folders. Looked up in:
//! - `~/.claude/skills/*/SKILL.md`: global skills, available in every project. Anything can land here,
//!   so origin is worked out separately (below)
//! - `~/.claude/skills/synced/<id>/manifest.json`: skills synced from claude.ai
//! - `~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/skills/*/SKILL.md`
//!   (installed plugins; named `plugin:skill`)
//! - `<repo>/.claude/skills/*/SKILL.md` for every project calvin has seen a session in (the
//!   session's folder, and its parents up to the repository root)
//! - extra folders from config (`[skills] extra_paths`)
//!
//! Each skill has a `kind`, the type of folder it lives in, as Claude Code defines them:
//! `global` (`~/.claude/skills`, including skills synced from claude.ai), `project`
//! (a repository's `.claude/skills`) or `plugin`. Where a skill came from, when that's
//! recorded somewhere (the `npx skills` installer's lock file, the claude.ai sync
//! manifest, the plugin's marketplace), is kept separately in `detail`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// Synced claude.ai skills appear in Claude Code under this plugin-style prefix.
const SYNCED_PREFIX: &str = "anthropic-skills";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
    /// Type of folder: `global`, `project` or `plugin`.
    pub kind: String,
    /// Where it came from, if recorded (e.g. "from the vercel-labs/skills repo").
    pub detail: Option<String>,
    pub path: String,
    /// Coding tools that can discover this installed copy.
    pub providers: Vec<String>,
}

/// Where to look.
#[derive(Debug, Default, Clone)]
pub struct Locations {
    pub claude_dir: PathBuf,
    pub copilot_dir: PathBuf,
    pub cursor_dir: PathBuf,
    pub project_dirs: Vec<PathBuf>,
    pub extra: Vec<PathBuf>,
}

pub fn installed(loc: &Locations) -> Vec<InstalledSkill> {
    let lock = install_lock(&loc.claude_dir);
    let mut found: BTreeMap<String, InstalledSkill> = BTreeMap::new();
    let mut add = |skill: InstalledSkill| {
        if let Some(existing) = found.get_mut(&skill.name) {
            for provider in skill.providers {
                if !existing.providers.contains(&provider) {
                    existing.providers.push(provider);
                }
            }
            existing.providers.sort();
        } else {
            found.insert(skill.name.clone(), skill);
        }
    };

    for mut skill in scan(&loc.claude_dir.join("skills"), 2, "global", "claude-code") {
        classify_global(&mut skill, &lock);
        add(skill);
    }
    for skill in synced(&loc.claude_dir.join("skills").join("synced")) {
        add(skill);
    }

    // plugins/cache/<marketplace>/<plugin>/<version>/skills/<skill>/SKILL.md
    let cache = loc.claude_dir.join("plugins").join("cache");
    for entry in WalkDir::new(&cache)
        .min_depth(6)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let rel: Vec<String> = entry
            .path()
            .strip_prefix(&cache)
            .map(|p| {
                p.components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        if rel.len() == 6
            && rel[3] == "skills"
            && let Some(mut skill) = read_skill(entry.path(), "plugin", Some(&rel[1]))
        {
            skill.detail = Some(format!("the {} plugin ({} marketplace)", rel[1], rel[0]));
            skill.providers = vec!["claude-code".into()];
            add(skill);
        }
    }

    for skill in scan(&loc.copilot_dir.join("skills"), 2, "global", "copilot-cli") {
        add(skill);
    }
    for skill in scan(&loc.cursor_dir.join("skills-cursor"), 2, "global", "cursor") {
        add(skill);
    }

    for dir in project_roots(&loc.project_dirs) {
        let project = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for mut skill in scan(
            &dir.join(".claude").join("skills"),
            2,
            "project",
            "claude-code",
        ) {
            skill.detail = Some(format!("in the {project} repository"));
            add(skill);
        }
        for mut skill in scan(
            &dir.join(".github").join("skills"),
            2,
            "project",
            "copilot-cli",
        ) {
            skill.detail = Some(format!("in the {project} repository"));
            add(skill);
        }
        for mut skill in scan(&dir.join(".cursor").join("skills"), 2, "project", "cursor") {
            skill.detail = Some(format!("in the {project} repository"));
            add(skill);
        }
    }
    for dir in &loc.extra {
        for mut skill in scan(dir, 3, "global", "shared") {
            classify_global(&mut skill, &lock);
            skill.providers = vec!["claude-code".into(), "copilot-cli".into(), "cursor".into()];
            add(skill);
        }
    }
    found.into_values().collect()
}

/// Folders to check for `.claude/skills`: each session folder and its parents, up to the
/// repository root (the first folder with `.git`). Stops at the home folder, and never
/// looks further up than a repository.
fn project_roots(session_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    let mut roots = std::collections::BTreeSet::new();
    for dir in session_dirs {
        let mut current = Some(dir.as_path());
        while let Some(d) = current {
            if home.as_deref() == Some(d) {
                break;
            }
            roots.insert(d.to_path_buf());
            if d.join(".git").exists() {
                break;
            }
            current = d.parent();
        }
    }
    roots.into_iter().collect()
}

fn classify_global(skill: &mut InstalledSkill, lock: &HashMap<String, String>) {
    if let Some(repo) = lock.get(&skill.name) {
        skill.detail = Some(format!("from the {repo} repo"));
    }
}

#[derive(Deserialize)]
struct Lock {
    #[serde(default)]
    skills: HashMap<String, LockEntry>,
}

#[derive(Deserialize)]
struct LockEntry {
    source: Option<String>,
}

/// Skills installed with `npx skills`, which records them next to `~/.claude` in
/// `~/.agents/.skill-lock.json`.
fn install_lock(claude_dir: &Path) -> HashMap<String, String> {
    let Some(home) = claude_dir.parent() else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(home.join(".agents").join(".skill-lock.json")) else {
        return HashMap::new();
    };
    let Ok(lock) = serde_json::from_str::<Lock>(&text) else {
        return HashMap::new();
    };
    lock.skills
        .into_iter()
        .filter_map(|(name, e)| Some((name, e.source?)))
        .collect()
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    skills: Vec<ManifestSkill>,
}

#[derive(Deserialize)]
struct ManifestSkill {
    #[serde(rename = "skillId")]
    skill_id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "creatorType", default)]
    creator_type: Option<String>,
}

/// Skills synced from claude.ai: `synced/<id>/manifest.json` plus one folder per skill.
fn synced(root: &Path) -> Vec<InstalledSkill> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else {
            continue;
        };
        for s in manifest.skills {
            let skill_md = dir.join(&s.skill_id).join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let by = match s.creator_type.as_deref() {
                Some("anthropic") => "synced from claude.ai (made by Anthropic)",
                _ => "synced from claude.ai",
            };
            out.push(InstalledSkill {
                name: format!("{SYNCED_PREFIX}:{}", s.name),
                description: s.description,
                kind: "global".into(),
                detail: Some(by.into()),
                path: skill_md.to_string_lossy().to_string(),
                providers: vec!["claude-code".into()],
            });
        }
    }
    out
}

/// SKILL.md files up to `depth` levels below `root`, skipping hidden folders and the
/// claude.ai `synced` folder (handled separately).
fn scan(root: &Path, depth: usize, kind: &str, provider: &str) -> Vec<InstalledSkill> {
    if !root.is_dir() {
        return Vec::new();
    }
    WalkDir::new(root)
        .max_depth(depth)
        .into_iter()
        .filter_entry(|e| {
            let skip_dir = e.file_type().is_dir()
                && e.depth() > 0
                && (e.file_name().to_string_lossy().starts_with('.')
                    || (e.depth() == 1 && e.file_name() == "synced"));
            !skip_dir
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_name() == "SKILL.md")
        .filter_map(|e| {
            let mut skill = read_skill(e.path(), kind, None)?;
            skill.providers = vec![provider.to_string()];
            Some(skill)
        })
        .collect()
}

fn read_skill(path: &Path, kind: &str, plugin: Option<&str>) -> Option<InstalledSkill> {
    let text = std::fs::read_to_string(path).ok()?;
    let fm = front_matter(&text);
    let dir_name = path.parent()?.file_name()?.to_string_lossy().to_string();
    let name = fm
        .get("name")
        .cloned()
        .filter(|n| !n.is_empty())
        .unwrap_or(dir_name);
    let name = match plugin {
        Some(p) => format!("{p}:{name}"),
        None => name,
    };
    Some(InstalledSkill {
        name,
        description: fm.get("description").cloned().unwrap_or_default(),
        kind: kind.to_string(),
        detail: None,
        path: path.to_string_lossy().to_string(),
        providers: Vec::new(),
    })
}

/// Minimal YAML front matter reader: top-level `key: value` pairs, including folded
/// (`>`) and literal (`|`) multi-line values. Enough for `name` and `description`.
fn front_matter(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    let mut current: Option<(String, Vec<String>)> = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if indented {
            if let Some((_, parts)) = current.as_mut() {
                parts.push(line.trim().to_string());
            }
            continue;
        }
        if let Some((k, parts)) = current.take() {
            out.insert(k, parts.join(" ").trim().to_string());
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            let value = if matches!(value, ">" | "|" | ">-" | "|-") {
                ""
            } else {
                value
            };
            let value = value.trim_matches(|c| c == '"' || c == '\'');
            current = Some((key.trim().to_string(), vec![value.to_string()]));
        }
    }
    if let Some((k, parts)) = current {
        out.insert(k, parts.join(" ").trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_folded_description() {
        let fm = front_matter(
            "---\nname: shipit\ndescription: >\n  Git commit and\n  ship workflow.\n---\n# Body",
        );
        assert_eq!(fm["name"], "shipit");
        assert_eq!(fm["description"], "Git commit and ship workflow.");
    }

    #[test]
    fn finds_skills_by_folder_type() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let write = |p: PathBuf, body: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        let skill = |name: &str| format!("---\nname: {name}\ndescription: {name} skill\n---\n");
        write(claude.join("skills/shipit/SKILL.md"), &skill("shipit"));
        write(
            claude.join("skills/find-skills/SKILL.md"),
            &skill("find-skills"),
        );
        write(claude.join("skills/aspire/SKILL.md"), &skill("aspire"));
        write(
            claude.join("skills/.claude/hidden/SKILL.md"),
            &skill("hidden"),
        );
        write(
            tmp.path().join(".agents/.skill-lock.json"),
            r#"{"version":3,"skills":{"find-skills":{"source":"vercel-labs/skills"}}}"#,
        );
        write(
            claude.join("skills/synced/org1/manifest.json"),
            r#"{"skills":[{"skillId":"pdf","name":"pdf","description":"PDFs","creatorType":"anthropic"}]}"#,
        );
        write(
            claude.join("skills/synced/org1/pdf/SKILL.md"),
            &skill("pdf"),
        );
        write(
            claude.join("plugins/cache/mkt/datadog/1.0.0/skills/ddviz/SKILL.md"),
            "---\nname: ddviz\n---\n",
        );
        let project = tmp.path().join("repo");
        write(
            project.join(".claude/skills/local/SKILL.md"),
            "no front matter",
        );
        write(
            project.join(".github/skills/copilot-project/SKILL.md"),
            &skill("copilot-project"),
        );
        let copilot = tmp.path().join("copilot");
        write(
            copilot.join("skills/copilot-global/SKILL.md"),
            &skill("copilot-global"),
        );
        std::fs::create_dir_all(project.join(".git")).unwrap();
        // The session started in a subfolder; the skill is at the repository root.
        let session_dir = project.join("src/app");
        std::fs::create_dir_all(&session_dir).unwrap();

        let loc = Locations {
            claude_dir: claude,
            copilot_dir: copilot,
            cursor_dir: tmp.path().join("cursor"),
            project_dirs: vec![session_dir],
            extra: vec![],
        };
        let installed = installed(&loc);
        assert_eq!(
            installed
                .iter()
                .find(|s| s.name == "copilot-project")
                .unwrap()
                .providers,
            vec!["copilot-cli"]
        );
        let got: Vec<_> = installed
            .into_iter()
            .map(|s| (s.name, s.kind, s.detail.is_some()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("anthropic-skills:pdf".into(), "global".into(), true),
                ("aspire".into(), "global".into(), false),
                ("copilot-global".into(), "global".into(), false),
                ("copilot-project".into(), "project".into(), true),
                ("datadog:ddviz".into(), "plugin".into(), true),
                ("find-skills".into(), "global".into(), true),
                ("local".into(), "project".into(), true),
                ("shipit".into(), "global".into(), false),
            ]
        );
    }
}
