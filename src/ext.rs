//! What an agent may load beyond its `tools`. Nothing by default.
//!
//! `plugins` — local plugin dirs, passed as `--plugin-dir`.
//! `mcp`     — MCP server configs by name; their `mcp__<name>` tools are allowed.
//! `inherit` — load user, project and local settings, all skills and MCP servers.
//! `skills`  — named skills. The CLI has no flag to select one, so a name is
//!             rejected here instead of being dropped silently.

use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

use crate::OrchError;

pub const USER_SKILLS: &str = "~/.claude/skills";

#[derive(Debug, Clone, Default)]
pub struct Extensions {
    pub skills: Vec<String>,
    pub plugins: Vec<String>,
    pub mcp: Map<String, Value>,
    pub inherit: bool,
}

impl Extensions {
    pub fn none() -> Self {
        Self::default()
    }

    /// Cache-key part; empty for the default.
    pub fn key(&self) -> String {
        if self.skills.is_empty()
            && self.plugins.is_empty()
            && self.mcp.is_empty()
            && !self.inherit
        {
            return String::new();
        }
        let mut plugins = self.plugins.clone();
        plugins.sort();
        let mut skills = self.skills.clone();
        skills.sort();
        json!([skills, plugins, Value::Object(self.mcp.clone()), self.inherit]).to_string()
    }

    /// What the runner needs: plugin dirs, MCP servers, extra read roots and
    /// which setting sources to load.
    pub fn resolve(&self) -> Result<Resolved, OrchError> {
        if !self.skills.is_empty() {
            return Err(OrchError::Extension(format!(
                "named skills ({}) have no CLI flag to select them; grant them through a \
plugin dir with --plugin, or load user settings with --inherit-settings",
                self.skills.join(", ")
            )));
        }

        let mut plugins = Vec::new();
        for p in &self.plugins {
            let path = expanduser(Path::new(p));
            let path = path.canonicalize().unwrap_or(path);
            if !path.is_dir() {
                return Err(OrchError::Extension(format!("no such plugin dir: {p}")));
            }
            plugins.push(path);
        }

        let mut extra_allowed: Vec<String> =
            self.mcp.keys().map(|n| format!("mcp__{n}")).collect();
        extra_allowed.sort();

        let mut reads = plugins.clone();
        let setting_sources: Vec<String> = if self.inherit {
            reads.push(expanduser(Path::new(USER_SKILLS)));
            // `user` only. `project` resolves against cwd — and cwd is the repo
            // under review — so loading it let a reviewed repo ship its own
            // .claude/settings.json, hooks included, alongside our confine
            // document. A tool pointed at untrusted trees must not read their
            // settings.
            vec!["user".into()]
        } else {
            Vec::new()
        };

        Ok(Resolved {
            plugins,
            extra_allowed,
            reads,
            setting_sources,
            mcp_servers: Value::Object(self.mcp.clone()),
        })
    }

    /// Validate without building plugin shims — used by argument parsing.
    pub fn check(&self) -> Result<(), OrchError> {
        self.resolve().map(|_| ())
    }
}

#[derive(Debug)]
pub struct Resolved {
    pub plugins: Vec<PathBuf>,
    pub extra_allowed: Vec<String>,
    pub reads: Vec<PathBuf>,
    pub setting_sources: Vec<String>,
    pub mcp_servers: Value,
}

/// Expand a leading `~`; a bare `Path::new("~/x")` is relative and would land
/// inside the repo, where `rm -rf` and gitignore never find it.
pub fn expanduser(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let rest = match s.strip_prefix("~/") {
        Some(rest) => rest,
        None if s == "~" => "",
        None => return path.to_path_buf(),
    };
    let Some(home) = home_dir() else {
        return path.to_path_buf();
    };
    if rest.is_empty() {
        home
    } else {
        home.join(rest)
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_extensions_have_an_empty_cache_key() {
        assert_eq!(Extensions::none().key(), "");
    }

    #[test]
    fn cache_key_ignores_plugin_order_but_not_content() {
        let a = Extensions { plugins: vec!["b".into(), "a".into()], ..Default::default() };
        let b = Extensions { plugins: vec!["a".into(), "b".into()], ..Default::default() };
        let c = Extensions { plugins: vec!["a".into()], ..Default::default() };
        assert_eq!(a.key(), b.key());
        assert_ne!(a.key(), c.key());
        assert_ne!(a.key(), "");
    }

    #[test]
    fn named_skills_are_rejected_rather_than_dropped() {
        let ext = Extensions { skills: vec!["pdf".into()], ..Default::default() };
        let err = ext.resolve().unwrap_err();
        assert!(matches!(err, OrchError::Extension(_)));
        assert!(err.to_string().contains("--inherit-settings"));
    }

    #[test]
    fn a_missing_plugin_dir_is_an_extension_error() {
        let ext = Extensions { plugins: vec!["/nope/not/here".into()], ..Default::default() };
        assert!(matches!(
            ext.resolve().unwrap_err(),
            OrchError::Extension(_)
        ));
    }

    #[test]
    fn inherit_loads_user_settings_but_never_the_target_repos() {
        // `project` resolves against cwd, and cwd is the repo under review —
        // so loading it let a reviewed tree ship its own hooks.
        let r = Extensions { inherit: true, ..Default::default() }.resolve().unwrap();
        assert_eq!(r.setting_sources, vec!["user"]);
    }

    #[test]
    fn default_loads_no_settings_and_the_default_tools_are_untouched() {
        let r = Extensions::none().resolve().unwrap();
        assert!(r.setting_sources.is_empty());
        assert!(r.extra_allowed.is_empty());
        assert!(r.plugins.is_empty());
    }

    #[test]
    fn mcp_names_become_allowed_tools() {
        let mut mcp = Map::new();
        mcp.insert("files".into(), json!({"command": "x"}));
        let r = Extensions { mcp, ..Default::default() }.resolve().unwrap();
        assert_eq!(r.extra_allowed, vec!["mcp__files"]);
    }

    #[test]
    fn expanduser_only_touches_a_leading_tilde() {
        assert_eq!(expanduser(Path::new("/tmp/x")), PathBuf::from("/tmp/x"));
        assert_eq!(expanduser(Path::new("~")), home_dir().unwrap());
        assert!(expanduser(Path::new("~a/b")).to_string_lossy().starts_with("~a"));
    }
}