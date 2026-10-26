//! Persisted swap memory (§3.5), one per provider in `state.json`.
//!
//! Every change of active account is recorded (`note_active_change`), latches
//! stop an account from being re-entered within the same reset epoch, and
//! every write merges under the state lock (max per key) so a CLI switch made
//! while the daemon was deciding is never lost.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::policy::LATCH_MAX_AGE_DAYS;
use super::WindowId;

/// A latch on an account we left: the reset epoch of the window it was left
/// in. Void in any other epoch (a different `resets_at`, or a lower pct than
/// when it was latched — an early or claimed reset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Latch {
    /// The window it was left in.
    pub window: WindowId,
    /// That window's reported reset (unix secs), identifying the epoch.
    #[serde(default)]
    pub resets_at: Option<i64>,
    /// The window's pct when latched; a lower reading means a new epoch.
    pub pct: f64,
    /// When the latch was set (unix secs), for the 8-day prune.
    pub at: i64,
}

/// Which latch a leave sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchKind {
    /// Left by Trigger: not a Trigger/Rebalance target again this epoch.
    Trigger,
    /// Left by Drain or Serve: not a drain target again this epoch.
    Drained,
}

/// How an active-account change happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The engine switched (`switch_if_still_active`).
    Auto,
    /// The user switched (menu or CLI).
    Manual,
    /// The active key changed outside usagio's switch paths.
    External,
}

/// Swap memory for one provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SwapMemory {
    /// Last change of active account (auto, manual or external), unix secs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_switch_at: Option<i64>,
    /// Account → when we last left it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub left_at: BTreeMap<String, i64>,
    /// Account → the epoch it was left in by a Trigger move.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trigger_left: BTreeMap<String, Latch>,
    /// Account → the epoch it was left in by a Drain or Serve move.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub drained: BTreeMap<String, Latch>,
    /// The stuck notice was sent and hasn't re-armed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stuck_notified: bool,
    /// The active key as of the last note (External detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_active: Option<String>,
    /// The active account the engine last judged Draining (re-entry to Ready
    /// needs a Real reading under `limit − 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draining: Option<String>,
    /// The active account's tier as of its last Real reading (the stuck
    /// notice re-arms on a Real-backed tier change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_tier: Option<String>,
}

fn lc(s: &str) -> String {
    s.to_ascii_lowercase()
}

impl SwapMemory {
    /// Record a change of active account `from → to` at `now`. Only call when
    /// the key really changed; same-account token adoption is not a change.
    pub fn note_active_change(&mut self, from: Option<&str>, to: &str, now: i64, kind: ChangeKind) {
        let _ = kind; // the pick itself is set/cleared by the caller's state write
        if let Some(from) = from.filter(|f| !f.eq_ignore_ascii_case(to)) {
            self.left_at.insert(lc(from), now);
        }
        self.last_switch_at = Some(self.last_switch_at.map_or(now, |t| t.max(now)));
        self.last_seen_active = Some(to.to_string());
        self.stuck_notified = false;
        self.draining = None;
        self.real_tier = None;
    }

    /// Set the leave latch `kind` on `key`.
    pub fn apply_latch(&mut self, key: &str, kind: LatchKind, latch: Latch) {
        match kind {
            LatchKind::Trigger => self.latch_trigger(key, latch),
            LatchKind::Drained => self.latch_drained(key, latch),
        }
    }

    /// External detection: the active key differs from the last noted one.
    /// Returns the account that was left, if this is a real change. A memory
    /// that has never seen an active key just starts tracking (no change).
    pub fn detect_external(&mut self, active: Option<&str>, now: i64) -> Option<String> {
        let active = active?;
        match self.last_seen_active.clone() {
            None => {
                self.last_seen_active = Some(active.to_string());
                None
            }
            Some(seen) if seen.eq_ignore_ascii_case(active) => None,
            Some(seen) => {
                self.note_active_change(Some(&seen), active, now, ChangeKind::External);
                Some(seen)
            }
        }
    }

    pub fn left_at(&self, key: &str) -> Option<i64> {
        self.left_at.get(&lc(key)).copied()
    }

    pub fn trigger_latch(&self, key: &str) -> Option<&Latch> {
        self.trigger_left.get(&lc(key))
    }

    pub fn drained_latch(&self, key: &str) -> Option<&Latch> {
        self.drained.get(&lc(key))
    }

    pub fn latch_trigger(&mut self, key: &str, latch: Latch) {
        self.trigger_left.insert(lc(key), latch);
    }

    pub fn latch_drained(&mut self, key: &str, latch: Latch) {
        self.drained.insert(lc(key), latch);
    }

    /// Drop latches whose epoch reset has passed or that are older than 8
    /// days, and `left_at` entries older than 8 days.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let now_ts = now.timestamp();
        let max_age = LATCH_MAX_AGE_DAYS * 86_400;
        let keep = |l: &Latch| l.resets_at.is_none_or(|r| r > now_ts) && now_ts - l.at < max_age;
        self.trigger_left.retain(|_, l| keep(l));
        self.drained.retain(|_, l| keep(l));
        self.left_at.retain(|_, t| now_ts - *t < max_age);
    }

    /// Merge another writer's memory into this one (§3.5): maps are unioned
    /// keeping the max per key, `last_switch_at` is the max. `stuck_notified`,
    /// `draining` and `last_seen_active` stay this (the engine owner's) value.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn merge_from(&mut self, other: &SwapMemory) {
        for (k, v) in &other.left_at {
            let e = self.left_at.entry(k.clone()).or_insert(*v);
            *e = (*e).max(*v);
        }
        for (mine, theirs) in [
            (&mut self.trigger_left, &other.trigger_left),
            (&mut self.drained, &other.drained),
        ] {
            for (k, v) in theirs {
                match mine.get(k) {
                    Some(cur) if cur.at >= v.at => {}
                    _ => {
                        mine.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        self.last_switch_at = match (self.last_switch_at, other.last_switch_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}
