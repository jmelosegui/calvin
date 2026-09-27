//! Finds installed skills, so usage can be compared against what's available.
//!
//! Read-only: calvin never writes to skill folders. Looked up in:
//! - `~/.claude/skills/**/SKILL.md` (user skills)
//! - `~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/skills/*/SKILL.md`
//!   (installed plugins; named `plugin:skill`)
//! - `<project>/.claude/skills/*/SKILL.md` for every project calvin has seen a session in
//! - extra folders from config (`[skills] extra_paths`)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
    /// `user`, `plugin`, `project` or `extra`.
    pub source: String,
    pub path: String,
}

pub fn installed(
    claude_dir: &Path,
    project_dirs: &[PathBuf],
    extra: &[PathBuf],
) -> Vec<InstalledSkill> {
    let mut found: BTreeMap<String, InstalledSkill> = BTreeMap::new();
    let mut add = |skill: InstalledSkill| {
        found.entry(skill.name.clone()).or_insert(skill);
    };

    for skill in scan(&claude_dir.join("skills"), 3, "user") {
        add(skill);
    }

    // plugins/cache/<marketplace>/<plugin>/<version>/skills/<skill>/SKILL.md
    let cache = claude_dir.join("plugins").join("cache");
    for entry in WalkDir::new(&cache)
        .min_depth(6)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let rel: Vec<_> = entry
            .path()
            .strip_prefix(&cache)
            .map(|p| p.components().collect())
            .unwrap_or_default();
        if rel.len() == 6 && rel[3].as_os_str() == "skills" {
            let plugin = rel[1].as_os_str().to_string_lossy().to_string();
            if let Some(skill) = read_skill(entry.path(), "plugin", Some(&plugin)) {
                add(skill);
            }
        }
    }

    for dir in project_dirs {
        for skill in scan(&dir.join(".claude").join("skills"), 2, "project") {
            add(skill);
        }
    }
    for dir in extra {
        for skill in scan(dir, 3, "extra") {
            add(skill);
        }
    }
    found.into_values().collect()
}

fn scan(root: &Path, depth: usize, source: &str) -> Vec<InstalledSkill> {
    if !root.is_dir() {
        return Vec::new();
    }
    WalkDir::new(root)
        .max_depth(depth)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_name() == "SKILL.md")
        .filter_map(|e| read_skill(e.path(), source, None))
        .collect()
}

fn read_skill(path: &Path, source: &str, plugin: Option<&str>) -> Option<InstalledSkill> {
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
        source: source.to_string(),
        path: path.to_string_lossy().to_string(),
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
    fn finds_user_plugin_and_project_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let write = |p: PathBuf, body: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        write(
            claude.join("skills/shipit/SKILL.md"),
            "---\nname: shipit\ndescription: Ship it\n---\n",
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

        let names: Vec<_> = installed(&claude, &[project], &[])
            .into_iter()
            .map(|s| (s.name, s.source))
            .collect();
        assert_eq!(
            names,
            vec![
                ("datadog:ddviz".into(), "plugin".into()),
                ("local".into(), "project".into()),
                ("shipit".into(), "user".into())
            ]
        );
    }
}
