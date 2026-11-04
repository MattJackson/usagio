//! Detecting a provider token supplied through the environment (which pins the client to it).

use crate::providers::CLAUDE_SLUG;

/// Env var whose presence forces the Claude CLI to use a specific OAuth token,
/// bypassing the keychain login. When set, any swap we perform is silently
/// ignored by `claude`, so we skip Claude from auto-swap candidacy AND surface
/// a disabled "env override active — swap disabled" row in the menu section.
pub(crate) const CLAUDE_ENV_OVERRIDE_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The env var (if any) whose presence overrides `slug`'s stored login.
pub(crate) fn env_override_var_for(slug: &str) -> Option<&'static str> {
    match slug {
        CLAUDE_SLUG => Some(CLAUDE_ENV_OVERRIDE_VAR),
        _ => None,
    }
}

/// Whether the given provider currently has its env-override active in this
/// process's environment. `#[cfg(test)]` builds also consult a thread-local
/// hook (`set_env_override_hook_for_test`) so tests can toggle overrides
/// deterministically without racing on the real environment.
pub(crate) fn env_override_active(slug: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(v) = env_override_hook_lookup(slug) {
            return v;
        }
    }
    env_override_var_for(slug)
        .map(|var| {
            std::env::var_os(var)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

#[cfg(test)]
thread_local! {
    /// Set of slugs to report as env-overridden. `None` means "consult the
    /// real environment", `Some(set)` means "report only these slugs".
    pub(crate) static ENV_OVERRIDE_TEST_HOOK: std::cell::RefCell<Option<std::collections::HashSet<String>>>
        = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn env_override_hook_lookup(slug: &str) -> Option<bool> {
    ENV_OVERRIDE_TEST_HOOK.with(|h| h.borrow().as_ref().map(|s| s.contains(slug)))
}

/// Run `f` with the env-override lookup answering `true` only for `overrides`.
/// Restores the previous state on exit (including a panicking `f`, via `Drop`).
#[cfg(test)]
pub(crate) fn with_env_override_hook<F, R>(overrides: &[&str], f: F) -> R
where
    F: FnOnce() -> R,
{
    struct Guard(Option<std::collections::HashSet<String>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            let prev = self.0.take();
            ENV_OVERRIDE_TEST_HOOK.with(|h| *h.borrow_mut() = prev);
        }
    }
    let prev = ENV_OVERRIDE_TEST_HOOK.with(|h| h.borrow().clone());
    let set: std::collections::HashSet<String> = overrides.iter().map(|s| s.to_string()).collect();
    ENV_OVERRIDE_TEST_HOOK.with(|h| *h.borrow_mut() = Some(set));
    let _g = Guard(prev);
    f()
}
