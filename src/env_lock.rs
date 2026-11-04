//! Crate-wide serialization for process-global env mutation.
//!
//! `std::env::set_var` mutates a process-shared table; two test threads that
//! both flip `$HOME` around a tempdir will race and one thread ends up
//! resolving paths under the *other* thread's temp home. This crate hit that
//! exact bug when `src/platform/macos.rs` set `$HOME` without taking the
//! `HOME_LOCK` used by other tests, which cascaded into a live state.json
//! wipe on a developer machine (see the never-re-login postmortem).
//!
//! Every test that mutates `$HOME`, `$XDG_CONFIG_HOME`, or `$CODEX_HOME`
//! MUST go through [`scoped_env_var`], which:
//!   1. takes the crate-wide [`ENV_LOCK`] (so no other scoped mutator runs),
//!   2. snapshots the current value,
//!   3. installs the caller's value,
//!   4. runs the closure,
//!   5. restores the previous value on the way out — including on panic,
//!      via a `Drop` guard.
//!
//! A `clippy.toml` `disallowed-methods` entry blocks any new direct
//! `std::env::set_var` / `std::env::remove_var` call from slipping in.

use std::ffi::OsString;
use std::sync::Mutex;

/// The one true lock. Every scoped env mutation across the crate serialises
/// on this Mutex — including nested crates' test modules — so no two tests
/// ever see interleaved `$HOME` values.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Keys currently bound by a live [`scoped_env_var`] (tests only).
static SCOPED_KEYS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// True while some test has explicitly scoped `key`.
pub(crate) fn is_scoped(key: &str) -> bool {
    SCOPED_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|k| k == key)
}

/// For tests that set an env var by hand while already holding [`ENV_LOCK`]
/// (because `scoped_env_var` is not reentrant): marks `key` as explicitly
/// scoped until the returned guard drops.
pub(crate) struct ScopedKeyMark(String);

pub(crate) fn mark_scoped(key: &str) -> ScopedKeyMark {
    SCOPED_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(key.to_string());
    ScopedKeyMark(key.to_string())
}

impl Drop for ScopedKeyMark {
    fn drop(&mut self) {
        let mut s = SCOPED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = s.iter().rposition(|k| *k == self.0) {
            s.remove(i);
        }
    }
}

/// Test-build environment read for path resolution: the real value only when a
/// test explicitly scoped `key`; otherwise `None`, so a developer's exported
/// `CLAUDE_CONFIG_DIR` / `USERPROFILE` / `CODEX_HOME` can never redirect a
/// test onto real user paths.
pub(crate) fn guarded_var(key: &str) -> Option<OsString> {
    if is_scoped(key) {
        std::env::var_os(key)
    } else {
        None
    }
}

/// Run `f` with `key` bound to `val` (or unset when `val` is `None`).
/// Restores the prior value (including unset) after `f` returns or panics.
/// Serialised across the crate via [`ENV_LOCK`].
///
/// NOT reentrant: calling `scoped_env_var` from inside `f` will deadlock.
/// The tests that use this helper never nest.
pub(crate) fn scoped_env_var<F, R>(key: &str, val: Option<&str>, f: F) -> R
where
    F: FnOnce() -> R,
{
    // Poisoning is fine: the previous panicked closure ran its Drop guard
    // (below) before unwinding past this lock, so `$HOME` is already restored.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let prev = std::env::var_os(key);
    // SAFETY: guarded by ENV_LOCK — no other thread inside this test binary
    // reads or writes `key` concurrently while this scope is live.
    #[allow(clippy::disallowed_methods)]
    match val {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
    // Mark scoped only after the value is set, so no other thread can see the
    // key as scoped while it still holds the developer's real value.
    SCOPED_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(key.to_string());

    struct Restore<'a> {
        key: &'a str,
        prev: Option<OsString>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            {
                let mut s = SCOPED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(i) = s.iter().rposition(|k| k == self.key) {
                    s.remove(i);
                }
            }
            #[allow(clippy::disallowed_methods)]
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
    let _r = Restore { key, prev };
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_env_var_sets_then_restores_previous_value() {
        // H3 (round-1 codeaudit): use a test-unique env key so the outer
        // snapshot doesn't race sibling tests that legitimately mutate $HOME
        // under the same ENV_LOCK — the previous form captured `outer` before
        // acquiring the lock, then compared after the lock released, leaving
        // a window for interleaving to make the "prior restored" assertion
        // flaky. A per-test key removes the race entirely (nothing else in
        // the crate touches this name).
        let key = "USAGIO_ENV_LOCK_TEST_RESTORE";
        let outer = std::env::var_os(key);
        scoped_env_var(key, Some("value-a"), || {
            assert_eq!(std::env::var_os(key), Some(OsString::from("value-a")));
        });
        assert_eq!(std::env::var_os(key), outer, "prior value restored");
    }

    #[test]
    fn scoped_env_var_restores_after_panic() {
        // See H3 note above — unique key avoids the outer-snapshot race.
        let key = "USAGIO_ENV_LOCK_TEST_PANIC";
        let outer = std::env::var_os(key);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scoped_env_var(key, Some("value-panic"), || {
                panic!("boom");
            });
        }));
        assert!(r.is_err(), "closure panicked");
        assert_eq!(
            std::env::var_os(key),
            outer,
            "value restored even when the closure panicked"
        );
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn scoped_env_var_none_unsets_then_restores() {
        // Seed the key as SET before the scoped call (directly, under the
        // crate lock — scoped calls can't nest), so the None arm has a real
        // value to unset and a real previous value to restore.
        let key = "USAGIO_ENV_LOCK_TEST_KEY";
        {
            let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var(key, "seed");
        }
        scoped_env_var(key, None, || {
            assert!(std::env::var_os(key).is_none(), "None unsets a set key");
        });
        let after = std::env::var_os(key);
        {
            let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var(key);
        }
        assert_eq!(
            after,
            Some(OsString::from("seed")),
            "previously-set value restored after a None scope"
        );
    }
}
