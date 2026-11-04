//! The built-in sign-in / renew window flow and its per-account cookie stores.

use super::actions::run_on_ui_thread;
use super::poller::request_poll_now;
use crate::notifications::notify;
use crate::providers;
use crate::providers::CLAUDE_SLUG;
use crate::store::State;
use anyhow::Result;
use chrono::DateTime;
use std::time::Duration;

/// Sign-in window store for a renew: the store the account was added with
/// (Claude: `Account::login_store`; any other provider: its slot's
/// `login_stores`), else one keyed by the account's email: workspaces of one
/// email share its vendor session (the sign-in page picks the workspace), and
/// keys from before workspace keying keep theirs.
pub(crate) fn renew_login_store(st: &State, slug: &str, key: &str) -> String {
    let added = if slug == CLAUDE_SLUG {
        st.find(key).and_then(|a| a.login_store.clone())
    } else {
        st.provider_accounts(slug)
            .and_then(|pa| pa.login_store(key))
            .map(String::from)
    };
    added.unwrap_or_else(|| crate::providers::trait_def::key_email(key).to_string())
}

/// "Renew login…": run the provider's own login in usagio's sign-in window —
/// the default browser where no window can be shown — and save the result
/// (see `crate::login`). Runs on the UI thread; the wait happens off it.
pub(super) fn handle_renew(slug: &str, key: &str) {
    if providers::get(slug).is_none() {
        return;
    }
    let (slug, key) = (slug.to_string(), key.to_string());
    let s = slug.clone();
    begin_login_after_superseding(&s, move || begin_renew(&slug, &key));
}

pub(super) fn begin_renew(slug: &str, key: &str) {
    let Some(provider) = providers::get(slug) else {
        return;
    };
    let store = State::load()
        .ok()
        .map(|st| renew_login_store(&st, slug, key))
        .unwrap_or_else(|| crate::providers::trait_def::key_email(key).to_string());
    let target = crate::login::Target::Renew(key.to_string());
    let started = crate::login::start(provider, target, |result| {
        sweep_sign_in_stores_later();
        match result {
            Ok(r) => {
                notify(&renewed_message(&r));
                request_poll_now();
            }
            Err(e) => notify(&format!("Renew failed: {e:#}")),
        }
    });
    match started {
        Ok(s) => show_sign_in(
            &format!(
                "Renew {} — usagio",
                crate::providers::trait_def::key_email(key)
            ),
            slug,
            &store,
            &s,
        ),
        Err(e) => notify(&format!("Renew failed: {e:#}")),
    }
}

/// "Sign in to a new <Provider> account…": the same login as Renew, in a
/// fresh sign-in window store, adding whichever account signs in (never
/// switching to it). The account remembers that store for its renewals.
pub(super) fn handle_sign_in(slug: &str) {
    if providers::get(slug).is_none() {
        return;
    }
    let owned = slug.to_string();
    begin_login_after_superseding(slug, move || begin_sign_in(&owned));
}

pub(super) fn begin_sign_in(slug: &str) {
    let Some(provider) = providers::get(slug) else {
        return;
    };
    let store = crate::login::new_store_key();
    let target = crate::login::Target::Add {
        store: store.clone(),
    };
    let started = crate::login::start(provider, target, |result| {
        // A failed add leaves its fresh store unclaimed: sweep it.
        sweep_sign_in_stores_later();
        match result {
            Ok(r) => {
                notify(&added_message(&r));
                request_poll_now();
            }
            Err(e) => notify(&format!("Sign-in failed: {e:#}")),
        }
    });
    match started {
        Ok(s) => show_sign_in(
            &format!("Sign in to {} — usagio", provider.display_name()),
            slug,
            &store,
            &s,
        ),
        Err(e) => notify(&format!("Sign-in failed: {e:#}")),
    }
}

/// Show a started login in the sign-in window, or the default browser where
/// no window can be shown here. UI thread only.
pub(super) fn show_sign_in(title: &str, slug: &str, store: &str, started: &crate::login::Started) {
    let platform = crate::platform();
    present_sign_in(
        slug,
        crate::login::store_id(slug, store),
        started,
        platform.sign_in_window_available().then_some(|| {
            platform.open_sign_in_window(
                title,
                &started.url,
                slug,
                store,
                std::sync::Arc::clone(&started.progress),
            )
        }),
        || platform.open_url(&started.url),
    );
}

/// [`show_sign_in`]'s logic with the window and browser injected: try the
/// window, fall back to the browser, and track the flow either way so a newer
/// sign-in for `slug` can stop it (and free its port).
pub(super) fn present_sign_in(
    slug: &str,
    store: [u8; 16],
    started: &crate::login::Started,
    open_window: Option<impl FnOnce() -> Result<()>>,
    open_browser: impl FnOnce() -> Result<()>,
) {
    let in_window = open_window.is_some_and(|open| match open() {
        Ok(()) => true,
        Err(e) => {
            crate::logging::log(&format!("sign-in window failed, using browser: {e:#}"));
            false
        }
    });
    if in_window {
        track_login(slug, std::sync::Arc::clone(&started.progress));
        track_flow_store(store, std::sync::Arc::clone(&started.progress));
        return;
    }
    match open_browser() {
        Ok(()) => track_login(slug, std::sync::Arc::clone(&started.progress)),
        Err(e) => {
            started.progress.cancel();
            notify(&format!("Sign-in failed: {e:#}"));
        }
    }
}

/// Sign-in window stores of flows still running. A flow whose window closed
/// can still save its account (the code was already received), so its store
/// is kept until the flow ends.
pub(super) static FLOW_STORES: std::sync::Mutex<
    Vec<([u8; 16], std::sync::Arc<crate::login::Progress>)>,
> = std::sync::Mutex::new(Vec::new());

pub(super) fn track_flow_store(store: [u8; 16], progress: std::sync::Arc<crate::login::Progress>) {
    let mut flows = FLOW_STORES.lock().unwrap_or_else(|e| e.into_inner());
    flows.retain(|(_, p)| !p.is_finished());
    flows.push((store, progress));
}

/// Every sign-in store still wanted: each saved account's (the one its
/// "Renew login…" opens, [`renew_login_store`]) plus those of running flows.
pub(crate) fn sign_in_stores_to_keep(st: &State) -> std::collections::HashSet<[u8; 16]> {
    let claude = st
        .accounts
        .iter()
        .map(|a| (CLAUDE_SLUG, a.key().to_string()));
    let others = st.providers.iter().flat_map(|(slug, p)| {
        p.accounts
            .iter()
            .map(move |a| (slug.as_str(), a.key.clone()))
    });
    let mut keep: std::collections::HashSet<[u8; 16]> = claude
        .chain(others)
        .map(|(slug, key)| crate::login::store_id(slug, &renew_login_store(st, slug, &key)))
        .collect();
    let mut flows = FLOW_STORES.lock().unwrap_or_else(|e| e.into_inner());
    flows.retain(|(_, p)| !p.is_finished());
    keep.extend(flows.iter().map(|(id, _)| *id));
    keep
}

/// Delete orphaned sign-in stores — a removed account's, a failed add's — on
/// the UI thread. Never with unreadable state: that would delete them all.
pub(super) fn sweep_sign_in_stores() {
    sweep_sign_in_stores_with(|keep| crate::platform().sweep_sign_in_stores(keep));
}

pub(super) fn sweep_sign_in_stores_with(sweep: impl FnOnce(std::collections::HashSet<[u8; 16]>)) {
    match State::load() {
        Ok(st) => sweep(sign_in_stores_to_keep(&st)),
        Err(e) => crate::logging::log(&format!("event=sign_in_store_sweep_skipped reason={e:#}")),
    }
}

/// [`sweep_sign_in_stores`] at the UI thread's next tick (callable anywhere).
pub(crate) fn sweep_sign_in_stores_later() {
    run_on_ui_thread(sweep_sign_in_stores);
}

/// "dev@example.com renewed — login good until Oct 31 · live now".
pub(super) fn renewed_message(r: &crate::login::Renewed) -> String {
    login_message(format!("{} renewed", r.key), r)
}

/// "Added dev@example.com — login good until Oct 31".
pub(super) fn added_message(r: &crate::login::Renewed) -> String {
    login_message(format!("Added {}", r.key), r)
}

pub(super) fn login_message(mut msg: String, r: &crate::login::Renewed) -> String {
    if let Some(at) = r.login_expires_at.and_then(DateTime::from_timestamp_millis) {
        msg.push_str(&format!(
            " — login good until {}",
            at.with_timezone(&chrono::Local).format("%b %-d")
        ));
    }
    if r.made_live {
        msg.push_str(" · live now");
    }
    msg
}

/// The in-flight login per provider, shown in the sign-in window or the
/// browser. A newer sign-in for the same provider supersedes it first: an
/// open (possibly hidden) window or a closed tab would otherwise keep its
/// loopback listener — Codex's fixed port 1455 included — and the new flow
/// could not bind. A superseded window closes itself once its flow ends.
pub(super) static IN_FLIGHT_LOGINS: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::sync::Arc<crate::login::Progress>>>,
> = std::sync::Mutex::new(None);

pub(super) fn track_login(slug: &str, progress: std::sync::Arc<crate::login::Progress>) {
    let replaced = IN_FLIGHT_LOGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(slug.to_string(), std::sync::Arc::clone(&progress));
    // A flow replaced here would otherwise keep its listener, untracked.
    if let Some(old) = replaced.filter(|old| !std::sync::Arc::ptr_eq(old, &progress)) {
        old.supersede();
    }
}

/// Supersede the previous login for `slug`, if any. Never blocks; the
/// caller waits for the listener to be released ([`wait_released`]) off the UI
/// thread before binding the same port again.
pub(super) fn supersede_previous_login(
    slug: &str,
) -> Option<std::sync::Arc<crate::login::Progress>> {
    let prev = IN_FLIGHT_LOGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|m| m.remove(slug))?;
    // A finished flow holds no listener: nothing to wait for, so the new
    // sign-in runs inline and may use the sign-in window.
    if prev.is_finished() {
        return None;
    }
    prev.supersede();
    Some(prev)
}

/// Wait (briefly) until a superseded flow's worker has released its listener.
pub(super) fn wait_released(prev: &crate::login::Progress) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !prev.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Latest sign-in click per provider, so a worker still waiting for an old
/// listener never starts (and then supersedes) a flow begun by a newer click.
pub(super) static LOGIN_GEN: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

pub(super) fn login_gen(slug: &str, bump: bool) -> u64 {
    let mut m = LOGIN_GEN.lock().unwrap_or_else(|e| e.into_inner());
    let g = m
        .get_or_insert_with(Default::default)
        .entry(slug.to_string())
        .or_insert(0);
    if bump {
        *g += 1;
    }
    *g
}

/// Run `begin` for a new sign-in on `slug`, on the UI thread (it may open
/// the sign-in window, which keeps the account's own cookie store). With
/// nothing to supersede it runs inline. Otherwise a worker waits for the old
/// flow to release its listener — never the UI thread — and then hands
/// `begin` back to the UI thread ([`run_on_ui_thread`]). A newer click made
/// meanwhile wins: the handed-back `begin` then starts nothing.
pub(super) fn begin_login_after_superseding(slug: &str, begin: impl FnOnce() + Send + 'static) {
    let gen = login_gen(slug, true);
    match supersede_previous_login(slug) {
        None => begin(),
        Some(prev) => {
            let slug = slug.to_string();
            std::thread::spawn(move || {
                wait_released(&prev);
                run_on_ui_thread(move || {
                    if login_gen(&slug, false) == gen {
                        begin();
                    } else {
                        crate::logging::log(&format!(
                            "event=login_superseded_while_waiting provider={slug}"
                        ));
                    }
                });
            });
        }
    }
}

#[cfg(test)]
#[path = "signin_tests.rs"]
mod tests;
