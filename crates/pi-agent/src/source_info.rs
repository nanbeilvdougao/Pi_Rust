//! Embed source-control + project-manager facts into the system prompt.
//!
//! Two probes:
//!
//! 1. **Git**: walk up from `cwd` until a `.git` directory shows up, then
//!    read the current ref, commit hash, and dirty status from plumbing
//!    files (`HEAD`, `refs/heads/<branch>`, and `git status --porcelain`).
//!    We deliberately do *not* shell out for things we can read from disk
//!    so the prompt construction is fast and unaffected by network proxies.
//! 2. **Package manager / project type**: detect cargo / npm / pnpm / yarn
//!    / bun / pip / poetry / uv / go / maven / gradle by looking for the
//!    canonical lockfile or manifest at the workspace root. The result is
//!    enough for the assistant to suggest the right `cargo test` / `pnpm
//!    install` / `uv run` invocation up front.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceInfo {
    pub git: Option<GitInfo>,
    pub project: Vec<ProjectManager>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub remote: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectManager {
    Cargo,
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Pip,
    Poetry,
    Uv,
    Go,
    Maven,
    Gradle,
}

impl ProjectManager {
    pub fn name(self) -> &'static str {
        match self {
            ProjectManager::Cargo => "cargo",
            ProjectManager::Npm => "npm",
            ProjectManager::Pnpm => "pnpm",
            ProjectManager::Yarn => "yarn",
            ProjectManager::Bun => "bun",
            ProjectManager::Pip => "pip",
            ProjectManager::Poetry => "poetry",
            ProjectManager::Uv => "uv",
            ProjectManager::Go => "go",
            ProjectManager::Maven => "maven",
            ProjectManager::Gradle => "gradle",
        }
    }
}

pub fn detect(cwd: &Path) -> SourceInfo {
    SourceInfo {
        git: detect_git(cwd),
        project: detect_project(cwd),
    }
}

fn detect_git(cwd: &Path) -> Option<GitInfo> {
    let git_dir = find_git_dir(cwd)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let (branch, commit) = if let Some(rest) = head.trim().strip_prefix("ref: ") {
        let branch = rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string();
        let commit_path = git_dir.join(rest);
        let commit = fs::read_to_string(commit_path)
            .ok()
            .map(|s| s.trim().to_string());
        (Some(branch), commit)
    } else {
        // Detached HEAD: HEAD contains a raw hash.
        (None, Some(head.trim().to_string()))
    };

    // Best-effort remote: read first url under `[remote "origin"]` in
    // .git/config without invoking git.
    let remote = fs::read_to_string(git_dir.join("config"))
        .ok()
        .and_then(|cfg| parse_origin_url(&cfg));

    // Dirty: invoke `git status --porcelain` with cwd. We fall back to
    // `false` if git is unavailable; the prompt is informational, not
    // load-bearing.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false);

    Some(GitInfo {
        branch,
        commit,
        remote,
        dirty,
    })
}

fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start.to_path_buf());
    while let Some(path) = current {
        let candidate = path.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            // Submodule pointer: `gitdir: <relative path>`
            if let Ok(text) = fs::read_to_string(&candidate) {
                if let Some(rest) = text.trim().strip_prefix("gitdir: ") {
                    return Some(path.join(rest));
                }
            }
        }
        current = path.parent().map(|p| p.to_path_buf());
    }
    None
}

fn parse_origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[remote ") {
            in_origin = trimmed.contains("\"origin\"");
            continue;
        }
        if trimmed.starts_with('[') {
            in_origin = false;
        }
        if in_origin {
            if let Some(rest) = trimmed.strip_prefix("url = ") {
                return Some(rest.trim().to_string());
            }
            if let Some(rest) = trimmed.strip_prefix("url=") {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

fn detect_project(cwd: &Path) -> Vec<ProjectManager> {
    let mut out: Vec<ProjectManager> = Vec::new();
    if cwd.join("Cargo.toml").exists() {
        out.push(ProjectManager::Cargo);
    }
    if cwd.join("pnpm-lock.yaml").exists() {
        out.push(ProjectManager::Pnpm);
    }
    if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
        out.push(ProjectManager::Bun);
    }
    if cwd.join("yarn.lock").exists() {
        out.push(ProjectManager::Yarn);
    }
    if cwd.join("package-lock.json").exists()
        || (cwd.join("package.json").exists()
            && !out.iter().any(|p| {
                matches!(
                    p,
                    ProjectManager::Pnpm | ProjectManager::Yarn | ProjectManager::Bun
                )
            }))
    {
        out.push(ProjectManager::Npm);
    }
    if cwd.join("uv.lock").exists() {
        out.push(ProjectManager::Uv);
    }
    if cwd.join("poetry.lock").exists() {
        out.push(ProjectManager::Poetry);
    }
    if cwd.join("requirements.txt").exists()
        && !out
            .iter()
            .any(|p| matches!(p, ProjectManager::Uv | ProjectManager::Poetry))
    {
        out.push(ProjectManager::Pip);
    }
    if cwd.join("go.mod").exists() {
        out.push(ProjectManager::Go);
    }
    if cwd.join("pom.xml").exists() {
        out.push(ProjectManager::Maven);
    }
    if cwd.join("build.gradle").exists() || cwd.join("build.gradle.kts").exists() {
        out.push(ProjectManager::Gradle);
    }
    out
}

pub fn render_prompt_section(info: &SourceInfo, zh: bool) -> String {
    if info.git.is_none() && info.project.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push('\n');
    if zh {
        out.push_str("Source 信息：\n");
    } else {
        out.push_str("Source info:\n");
    }
    if let Some(git) = &info.git {
        if let Some(branch) = &git.branch {
            out.push_str(&format!("- git branch: {branch}\n"));
        }
        if let Some(commit) = &git.commit {
            let short = if commit.len() >= 12 {
                &commit[..12]
            } else {
                commit.as_str()
            };
            out.push_str(&format!("- git commit: {short}\n"));
        }
        if let Some(remote) = &git.remote {
            out.push_str(&format!("- git remote: {remote}\n"));
        }
        out.push_str(&format!(
            "- git dirty: {}\n",
            if git.dirty { "yes" } else { "no" }
        ));
    }
    if !info.project.is_empty() {
        let names: Vec<&'static str> = info.project.iter().map(|p| p.name()).collect();
        out.push_str(&format!("- project_manager: {}\n", names.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detects_cargo_project() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let pms = detect_project(dir.path());
        assert!(pms.contains(&ProjectManager::Cargo));
    }

    #[test]
    fn prefers_pnpm_over_npm_when_both_lockfiles() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        let pms = detect_project(dir.path());
        assert!(pms.contains(&ProjectManager::Pnpm));
        assert!(!pms.contains(&ProjectManager::Npm));
    }

    #[test]
    fn parse_origin_url_from_config() {
        let cfg = "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:foo/bar.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n";
        assert_eq!(
            parse_origin_url(cfg).as_deref(),
            Some("git@github.com:foo/bar.git")
        );
    }

    #[test]
    fn no_git_dir_returns_none() {
        let dir = tempdir().unwrap();
        assert!(detect_git(dir.path()).is_none());
    }

    #[test]
    fn render_section_skips_when_empty() {
        let info = SourceInfo::default();
        assert!(render_prompt_section(&info, true).is_empty());
    }

    #[test]
    fn render_section_includes_branch_and_managers() {
        let info = SourceInfo {
            git: Some(GitInfo {
                branch: Some("main".to_string()),
                commit: Some("0123456789abcdef".to_string()),
                remote: Some("git@example.com:x.git".to_string()),
                dirty: true,
            }),
            project: vec![ProjectManager::Cargo, ProjectManager::Pnpm],
        };
        let rendered = render_prompt_section(&info, true);
        assert!(rendered.contains("git branch: main"));
        assert!(rendered.contains("git commit: 0123456789ab"));
        assert!(rendered.contains("git dirty: yes"));
        assert!(rendered.contains("project_manager: cargo, pnpm"));
    }
}
