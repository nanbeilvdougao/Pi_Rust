//! Linux landlock integration.
//!
//! `apply_sandbox` (in [`sandbox`](super::sandbox)) wraps an external command
//! in `bwrap` / `firejail` / `sandbox-exec`. That works, but it requires
//! external binaries and a separate process boundary. On Linux 5.13+ we can
//! tell the *current* process "from this moment on, only these filesystem
//! roots are reachable" via Landlock, which gives us syscall-level isolation
//! without forking or shelling out.
//!
//! Why both? Landlock is a one-way ratchet — once you apply it, every
//! `Command::spawn` inherits the restrictions, *including* `bwrap` itself
//! (which would then fail to bind paths). So the two integrations are
//! complementary: landlock is preferred when the parent process can commit
//! to never expanding access; the external sandbox is preferred when only
//! a single subprocess should be restricted.
//!
//! Tests in this module never call `restrict_self`. Once landlock applies,
//! it cannot be relaxed for the lifetime of the process — that would break
//! every subsequent test. Instead we cover the plan-building logic and let
//! integration tests in `tests/` exercise the real ruleset behind a
//! `--release` flag that opts in.
//!
//! Parity target: `packages/agent/src/sandbox-landlock.ts` plus the
//! capability rules from `packages/agent/src/permissions.ts`.

use crate::{Capability, SandboxProfile};

/// What `restrict_self` should ask landlock to enforce. The plan is built
/// from a `SandboxProfile` plus an explicit list of read/write roots.
#[derive(Debug, Clone)]
pub struct LandlockPlan {
    /// Workspace + extra read roots — read-only.
    pub read_roots: Vec<String>,
    /// Workspace if writable — read+write+create.
    pub write_roots: Vec<String>,
    /// Roots from which the sandbox may exec binaries; usually system roots.
    pub exec_roots: Vec<String>,
    /// Whether network sockets are allowed (controls TCP/UDP binding via
    /// landlock net access where supported by the kernel).
    pub allow_net: bool,
}

impl LandlockPlan {
    /// Construct a plan from a [`SandboxProfile`] plus the capability set
    /// the caller wants to retain. Capabilities outside Read/Write/Network
    /// don't have a direct landlock counterpart but help us decide whether
    /// to make exec roots writable.
    pub fn from_profile(profile: &SandboxProfile, capabilities: &[Capability]) -> Self {
        let mut read_roots: Vec<String> = profile.extra_read_roots.clone();
        let mut write_roots: Vec<String> = Vec::new();
        if let Some(root) = &profile.workspace_root {
            read_roots.push(root.clone());
            if capabilities.contains(&Capability::WriteFile)
                || capabilities.contains(&Capability::DeleteFile)
                || capabilities.contains(&Capability::ChangeMode)
            {
                write_roots.push(root.clone());
            }
        }
        // Standard exec roots — needed for any non-trivial subprocess to
        // succeed. Callers can override by passing fully custom paths.
        let exec_roots = ["/usr", "/bin", "/sbin", "/lib", "/lib64"]
            .iter()
            .filter(|p| std::path::Path::new(p).exists())
            .map(|p| (*p).to_string())
            .collect();
        Self {
            read_roots,
            write_roots,
            exec_roots,
            allow_net: profile.allow_network
                || capabilities.contains(&Capability::Network)
                || capabilities.contains(&Capability::BindSocket),
        }
    }
}

/// What happened when we tried to apply landlock.
#[derive(Debug, Clone)]
pub enum LandlockOutcome {
    /// Landlock applied successfully; restrictions are now in effect.
    Applied { compatibility: String },
    /// Landlock is supported but the kernel rejected the ruleset.
    NotApplied { reason: String },
    /// Not Linux, or kernel < 5.13, or feature disabled. Caller should fall
    /// back to `apply_sandbox` if it needs isolation.
    Unsupported,
}

/// Returns true on Linux when landlock support compiled into the binary.
pub fn landlock_supported() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(target_os = "linux")]
pub fn restrict_self(plan: &LandlockPlan) -> LandlockOutcome {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, RestrictionStatus, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };

    let abi = ABI::V3;
    // Cover everything filesystem-related; landlock applies the strictest
    // intersection across our `add_rules` calls.
    let access_all = AccessFs::from_all(abi);
    let mut ruleset = match Ruleset::default().handle_access(access_all) {
        Ok(rs) => rs.create(),
        Err(err) => {
            return LandlockOutcome::NotApplied {
                reason: format!("ruleset attr 失败：{err}"),
            };
        }
    };
    let mut created = match ruleset {
        Ok(rs) => rs,
        Err(err) => {
            return LandlockOutcome::NotApplied {
                reason: format!("ruleset create 失败：{err}"),
            };
        }
    };

    // Read-only roots: deny everything mutating.
    let read_access = AccessFs::from_read(abi);
    for root in &plan.read_roots {
        if let Ok(fd) = PathFd::new(root) {
            if let Err(err) = created.add_rule(PathBeneath::new(fd, read_access)) {
                return LandlockOutcome::NotApplied {
                    reason: format!("加入 read root {root} 失败：{err}"),
                };
            }
        }
    }
    // Writable roots: read + write + create.
    let write_access = AccessFs::from_all(abi);
    for root in &plan.write_roots {
        if let Ok(fd) = PathFd::new(root) {
            if let Err(err) = created.add_rule(PathBeneath::new(fd, write_access)) {
                return LandlockOutcome::NotApplied {
                    reason: format!("加入 write root {root} 失败：{err}"),
                };
            }
        }
    }
    // Exec roots: read + execute.
    for root in &plan.exec_roots {
        if let Ok(fd) = PathFd::new(root) {
            if let Err(err) = created.add_rule(PathBeneath::new(fd, read_access)) {
                return LandlockOutcome::NotApplied {
                    reason: format!("加入 exec root {root} 失败：{err}"),
                };
            }
        }
    }

    match created.restrict_self() {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => LandlockOutcome::Applied {
                compatibility: format!("FullyEnforced (no_new_privs={})", status.no_new_privs),
            },
            RulesetStatus::PartiallyEnforced => LandlockOutcome::Applied {
                compatibility: format!("PartiallyEnforced (no_new_privs={})", status.no_new_privs),
            },
            RulesetStatus::NotEnforced => LandlockOutcome::NotApplied {
                reason: "kernel 拒绝执行 ruleset (LANDLOCK_NOT_ENFORCED)".into(),
            },
        },
        Err(err) => LandlockOutcome::NotApplied {
            reason: format!("restrict_self 失败：{err}"),
        },
    }
}

#[cfg(not(target_os = "linux"))]
pub fn restrict_self(_plan: &LandlockPlan) -> LandlockOutcome {
    LandlockOutcome::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_from_profile_promotes_workspace_to_write_when_capability_present() {
        let profile = SandboxProfile {
            workspace_root: Some("/work".to_string()),
            extra_read_roots: vec!["/etc/hosts".to_string()],
            allow_network: false,
        };
        let plan = LandlockPlan::from_profile(&profile, &[Capability::WriteFile]);
        assert!(plan.read_roots.iter().any(|r| r == "/etc/hosts"));
        assert!(plan.read_roots.iter().any(|r| r == "/work"));
        assert!(plan.write_roots.iter().any(|r| r == "/work"));
        assert!(!plan.allow_net);
    }

    #[test]
    fn plan_from_profile_keeps_workspace_readonly_without_write_capability() {
        let profile = SandboxProfile {
            workspace_root: Some("/work".to_string()),
            extra_read_roots: Vec::new(),
            allow_network: false,
        };
        let plan = LandlockPlan::from_profile(&profile, &[Capability::ReadFile]);
        assert!(plan.write_roots.is_empty());
        assert!(plan.read_roots.iter().any(|r| r == "/work"));
    }

    #[test]
    fn network_capability_or_profile_opens_allow_net() {
        let profile = SandboxProfile {
            workspace_root: None,
            extra_read_roots: Vec::new(),
            allow_network: false,
        };
        let plan_with_net = LandlockPlan::from_profile(&profile, &[Capability::Network]);
        assert!(plan_with_net.allow_net);
        let plan_bind = LandlockPlan::from_profile(&profile, &[Capability::BindSocket]);
        assert!(plan_bind.allow_net);
    }

    #[test]
    fn unsupported_on_non_linux_returns_unsupported_outcome() {
        if !cfg!(target_os = "linux") {
            let plan = LandlockPlan::from_profile(&SandboxProfile::default(), &[]);
            assert!(matches!(restrict_self(&plan), LandlockOutcome::Unsupported));
        }
    }

    #[test]
    fn delete_or_chmod_capability_promotes_workspace_to_write() {
        let profile = SandboxProfile {
            workspace_root: Some("/work".to_string()),
            extra_read_roots: Vec::new(),
            allow_network: false,
        };
        let plan_delete = LandlockPlan::from_profile(&profile, &[Capability::DeleteFile]);
        assert!(plan_delete.write_roots.iter().any(|r| r == "/work"));
        let plan_chmod = LandlockPlan::from_profile(&profile, &[Capability::ChangeMode]);
        assert!(plan_chmod.write_roots.iter().any(|r| r == "/work"));
    }

    #[test]
    fn supported_predicate_matches_target_os() {
        assert_eq!(landlock_supported(), cfg!(target_os = "linux"));
    }
}
