# Auto-swap redesign v3.1: evidence-gated runway engine

Status: **design v3.1, final spec** (round-3 fixes R3-B1–B3, R3-M1 and build notes; resolutions in §12; §2–§8 are
authoritative and §9/§11 are history). Earlier: design v3. It revises v2 after the round-2 review (R2-B1–B3, R2-M1–M4, R2-m1–m5, 9 open decisions).
The resolutions are in §11. v3 changes are marked **[v3]**. No code changed.

**[v3] Model principle: usage only goes up within a window until that window resets.**

- A reading that says a window is full (≥ X%) is a *lower bound*. It stays true until that window's `resets_at`,
  at any age, or until a newer reading shows otherwise (**[v3.1]** the lower bound is the *latest* reading, §3.3).
- Only readings that say a window *has room* go out of date.
- So the evidence classes (Real / Estimated / Stale) apply only to "has room" readings.
- A stale active account is never judged better than its last reading said.

Earlier status: design v2 (revised after `autoswap-redesign-review.md`, findings B1–B18).
Target: **usagio 0.12.2**. The new engine is the default. The old engine stays selectable for one release.
Baseline: 0.12.1.

What changed from v1:

- Every move off an account is now gated on evidence.
- Estimates rank candidates and set urgency. They can only *start* a move onto a verified healthy account.
- Verification fetches (`NeedReading`) are how unknown or stale accounts become known.
- Added: a rate-independent blind backstop, per-window reset staleness, a drain gain rule plus a drained latch,
  persisted swap memory, replay logging, the binding-window display feature, and an implementation plan.
- Section 9 resolves every review finding.

---

## 0. Summary

**Goal.** The active account is always one that can serve requests. Among those, prefer the one that keeps
serving longest, and use capacity that would otherwise expire. Switch as rarely as possible: every switch costs
a prompt-cache rebuild, which is real usage.

**Engine.** `autoswap::decide(&Snapshot) -> Decision` is pure. It takes no clock, network or globals. It
returns either `Stay(reason)`, `Switch(target, kind)` or `NeedReading(accounts)`. In the last case the shell
fetches those accounts and calls `decide` again. Four ideas:

1. **Runway, not percent.** Per account and per window (session, weekly): minutes until the window reaches the
   trigger (soft) and the drain exit (hard). Runway uses the account's burn rate, floored by a default rate. A
   window that resets before it would bind doesn't bind. Ranking uses runway in both windows.
2. **Evidence gates moves, estimates don't.** "Full" readings are lower bounds valid until the window resets
   (any age). "Has room" readings decay: `Real` (≤ 180 s), `Estimated` (≤ 10 min, drift), `Stale` (older).
   An estimate (or the blind projection) may start only one move: Trigger onto a *verified Ready* account with
   ≥ 15 min of runway. Every other move needs evidence about the active account: a lower-bound full reading
   (Serve, Credits, Prepare, drain exit at ≥ 99) or a `Real` reading (leaving a manual pick at the trigger).
3. **Tiers with hysteresis.** Each account is `Ready`, `Draining`, `Credits`, `Blocked` or `Unknown`. The
   active account stays `Ready` up to the trigger. A candidate needs 15 min of runway at
   `max(own rate, default rate)` to count as `Ready`. That works out to at least 4 weekly or 15 session points
   of room. Latches stop an account from being re-entered within the same reset epoch.
4. **One hard invariant.** Never stay on an exhausted account while another *verified* account can serve.
   Unknown accounts are verified first, within 2 fetches per cycle, even while the active account is in a 429
   streak.

The same runway function drives the new tray/menu feature: show the **binding window** (`96%w`).

---

## 1. Diagnosis (v1 findings, confirmed by the review)

- **D1.** A usage 429 is treated as "at the trigger" within 5 points of it (`effective_active_max_pct_for_swap_fire`).
  There were 18 active 429s between 15:38 and 17:31. Both urgent swaps today fired on readings 31 s old:
  16:23:06 dev4 12/91 → 429 at 16:23:37 → swap, and 17:10:43 dev8 40/90 → 429 at 17:11:14 → swap to dev4 16/92.
- **D2.** Targets are judged by percent inside a band plus weekly-reset date:
  - `forced_target` (0.10.5, Oct 14) let a target be closer to its limit than the account being left.
  - `candidate_order` ranked dev4 first on a reset 5 h sooner, six days out. dev4 had 3 weekly points to the
    trigger, dev8 had 5.
  - dev4 went 95 at 17:13 → 100 at 17:28.
- **D3.** Anti-thrash gates override the goal:
  - `NO_RETURN_SECS` kept the user on dev4 at a real 100% (17:28:31) while dev8 had a fresh 41/90 (17:22:27).
    The user switched by hand at 17:30:55.
  - Polling also fell to 180 s because the active was "not actionable".
- **Display defect (new).** The tray title is `pct(session.or(weekly))` (`menubar::title_for`). From 17:13 to
  17:25 it showed dev4's *session* (25–43%) while weekly went 95 → 99.
- **Credits.** Did not cause today's problem:
  - No Claude credits path fired. Every dev* account reports no credits, and pq.io (Claude) has
    `limit_reached`.
  - Codex logs `stayed reason=on_credits` every cycle, which is noise.
  - Latent issues, fixed in v2: `runs_on_credits` doesn't require `plan_exhausted`, and spendable amounts are
    compared across units.

---

## 2. Invariants (each is a test, §8)

| # | Invariant |
|---|---|
| **I1 Serve** | If the active account is lb-blocked (latest reading ≥ `BLOCKED_PCT` in a window that is not reset-pending, any age; or lapsed), and some account is *servable* (§3.4 arm 1: verified not blocked, or a post-leave and post-reset reading < 99.5), switch to it this cycle. Reset-pending accounts are read first (§5.7, exempt from the storm guard). Nothing overrides I1 except autoswap disabled or an env override. |
| **I2 No downgrade** | Trigger and Drain moves never land on an account with less runway than the active account's (floored rate). Exempt: Serve and Rebalance (§3.6). |
| **I3 Leave at trigger** | If the active account's binding window reaches the trigger (Real reading, estimate within the 10 min cap, or blind projection §3.3), and a verified `Ready` candidate exists, switch now (Trigger). No cooldown applies. Not for a manual pick unless the reading is Real (I6). |
| **I4 Bounded churn** | Per reset epoch, an account is left by Trigger at most once and by Drain/Serve at most once (latches §3.5). Optional moves (Rebalance, Prepare) are ≥ `SWAP_COOLDOWN` apart. Proptest with per-account 429 masks (§8.3). |
| **I5 Evidence** | A target is *verified* when its reading was taken ≥ 60 s after we last left it, its windows are not reset-pending (§3.3), and the reading is ≤ 120 s old or was refreshed by `NeedReading` this cycle. For accounts we left, drift is carried forward to `left_at`. Unverified targets: §3.4 post-pass. |
| **I6 Manual** | Optional moves (Rebalance, Prepare) are off while a pick is active. A pick leaves at the trigger only on a `Real` reading (0.10.4), never on an estimate or projection. Its tier is settled from the switch-time reading, then re-settled by the first fresh one (0.8.8, 0.10.4). A pick settled at Draining or worse holds until it is lb-blocked (0.8.9). An lb-blocked pick leaves for any servable account (Serve) but is never prepared away from. The hold ends when the pick's tier improves (0.12.1) or on any change of active account. |
| **I7 Credits last** | Credits are used only when the setting is on, the active account is lb-blocked, no account is servable, and every reset-pending plan account has been read (or its read failed) this cycle. Leave credits for a verified plan account with ≥ `MIN_LANDING`. Never prepare away from credits. |
| **I8 No info is no info** | A 429, error or skip changes only reading age. An lb-blocked active is not re-fetched (§5.7). A blind-in-band active gets the projection (§3.3). Any other Stale active gets `NeedReading(active)`, not a guess. |

---

## 3. The model

### 3.1 Inputs

```rust
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub provider: String,
    pub active: String,
    pub accounts: Vec<AccountIn>,          // switchable-filtered others + the active
    pub manual: Option<ManualPick>,        // { key, at, settled: Tier, settled_by_fresh: bool }
    pub memory: SwapMemory,                // persisted, §3.5
    pub policy: Policy,                    // trigger, session_ceiling, use_credits, constants
}
pub struct AccountIn {
    pub key: String,
    pub session: Option<WindowIn>,         // None = provider doesn't report it (Codex session)
    pub weekly: Option<WindowIn>,
    pub fetched_at: Option<i64>,           // last *successful* reading
    pub rate: AccountRate,                 // §3.2, per account per window, + peak
    pub credits: Option<Money>,            // spendable (requires plan_exhausted to matter)
    pub lapsed: bool,
    pub unverified: bool,                  // a NeedReading fetch failed this cycle
}
pub struct WindowIn { pub pct: f64, pub resets_at: Option<DateTime<Utc>> }
```

The switchable filter is unchanged: same provider, `provider_supports_swap`, `!needs_relogin`,
`!no_subscription`, no env override, has a reading. Optional windows: a missing window never binds and never
blocks. If both are missing, the account is `Unknown`.

### 3.2 Rates (one model; `burn_rate::estimate` is retired for decisions)

`autoswap::rates` computes, per account and per window, Δpct/Δt over the account's own **active** samples in
the last 30 min. Zero deltas are **included**, so flat data gives rate 0. Two kinds of interval are skipped:

- a drop in pct (a reset)
- a gap longer than 15 min (sleep, downtime)

It also computes `peak`: the maximum 5-minute rate in the last 60 min.

The **floored rate** used for candidate runway, drain gain and I2 is `max(rate, DEFAULT)`, with
`DEFAULT = 1.0 session / 0.25 weekly pct/min`. That fixes B7: an idle user can't make a 94.9% account look
Ready. Division guard: if rate ≤ ε, runway is ∞ when `pct < limit` and 0 otherwise. Never NaN. Ordering uses
`total_cmp`.

Same plan per provider is assumed, but each account gets its own rate (owner Q3). Until the new `usage_log`
fields exist (§6), rates come from the poller's in-memory readings of the active account. After a restart they
use the defaults until 2 readings exist. Codex gets its own samples (§6), so it no longer inherits Claude
defaults.

### 3.3 Per-window assessment

For window `w` of account `a`:

- **[v3.1] reset-pending**: the stored reading's own reported `resets_at ≤ now`, meaning the reading predates
  that reset. The pct is then an upper bound only. A reading is **post-reset** iff it is not reset-pending. A
  read taken after a reset reports the *next* `resets_at` (verified: dev6 read 05:00:05 reports weekly
  11-02 05:00). No skew constant is needed. If the local clock runs fast, the window turns pending slightly early,
  and the re-read (≤ 1 per account per 120 s) reports the old `resets_at` until the server rolls over.
- **[v3.1] window epoch**: the window identified by its reported `resets_at`. A new epoch starts when a reading
  reports a different `resets_at` or a lower pct than the previous reading. That covers an early reset, a limit
  boost, or a claimed banked reset (0.11.0). Latches (§3.5) store the epoch's `resets_at` and are void in any
  other epoch.
- **[v3.1] lower bound `lb_w`** = the pct of the **latest successful reading**, not a running maximum (usage
  only rises within an epoch, so the latest reading is the maximum). It is valid at any age until the window
  turns reset-pending or a newer reading arrives.
  - `blocked_w` ⇔ `lb_w ≥ BLOCKED_PCT` and not reset-pending. No age condition, active or candidate.
    `blocked_until = resets_at_w`. With no `resets_at`, `until = None`: still blocked, but Prepare excludes it, and
    v1's 12 h `MAX_LOCKED_STALENESS` refetch applies.
  - `drain_eligible_w` ⇔ `lb_w ≥ DRAIN_EXIT_PCT` and not reset-pending.
  - For the active account, `lb_w ≥ limit_w` means Draining at any age. A stale active account is never judged
    better than its last reading.
- **est_w** (room claims only):
  - active account: `pct_w + rate_w × min(age, EST_MAX_AGE) × SAFETY`
  - account we left: `pct_w + rate_w × max(0, left_at − fetched_at)` (B10)
  - any other account: `pct_w`
- **blind projection, no cutoff** (R2-B2): applies to the active account when its last Real reading had
  `pct_w ≥ trigger − BLIND_BAND` and that reading is older than `BLIND_AFTER` (120 s).
  - `proj_w = lb_w + R_blind_w × blind_age × SAFETY`.
  - `R_blind_w = max(peak_w, BLIND_RATE_w, floored_w)`.
  - `blind_age` excludes detected sleep (wall-clock gap minus monotonic gap > 60 s).
  - The projection has no age cap. It never creates Blocked or drain eligibility. It only enables the Trigger
    move (§3.4 arm 3P) and sets urgency and cadence.
- **binds**: the window doesn't bind if `resets_at < now + time_to(DRAIN_EXIT_PCT)`, meaning it resets before
  it could block. This applies to the active account too (B4).

Per account:

- `blocked = any blocked_w || lapsed`
- `blocked_until = max(resets_at of the blocked windows)`. This is `None` when unknown.
- `soft_runway = min over binding windows of time_to(limit_w)`, where `limit_session = min(trigger, ceiling)`
  and `limit_weekly = trigger`.
- `hard_runway = min over binding windows of time_to(DRAIN_EXIT_PCT)`.
- `evidence` = `Real` / `Estimated` / `Stale` (by age, and post-leave + 60 s for left accounts).

**Tiers.** For candidates, runway uses the floored rate.

| Tier | Active account | Candidate |
|---|---|---|
| `Ready` | every binding `est_w`/`proj_w` < `limit_w` and no `lb_w ≥ limit_w` (re-entry from Draining needs a Real reading < limit − 1) | `soft_runway ≥ MIN_LANDING`, verified (I5), not latched `trigger_left` in the current epoch |
| `Draining` | not blocked, some binding window has `lb_w`, `est_w` or `proj_w` ≥ `limit_w` | not blocked, not Ready (includes `trigger_left`-latched accounts) |
| `Credits` | blocked plan, spendable credits, `use_credits` | same |
| `Blocked{until}` | lb-blocked (any age) or lapsed | lb-blocked (any age) |
| `Unknown` | no reading, or every reported window is reset-pending | no reading, or reset-pending in a window whose stale pct would otherwise block |

**Ranking** (`rank_key`; also used by `auto_pick`, `optimize_now` and `switch` with no selector):

- `Ready`: `(expiring = weekly reset < now + REBALANCE_HORIZON ? reset : MAX, −soft_runway, email)`
- `Draining`: `(−hard_runway, email)`
- `Credits`: `(unit == provider's dominant unit ? 0 : 1, −spendable.amount_minor, email)`. Amounts are compared
  only within a unit (B11).
- `Blocked`: `(blocked_until or MAX, email)`

### 3.4 Decision

```text
decide(s):
  if autoswap disabled | env override → Stay(disabled)
  act = assess(active); C = assess(others)
  verified(c) := I5 holds && !c.unverified
  servable(c) := !lb_blocked(c) && (verified(c)
                 || (c has a post-leave(+60 s), non-reset-pending reading, any age))   // R2-M1
  pick = s.manual if for act, else None

  // 0. Unknown / stale active (I8)
  if act.tier == Blocked or act.lapsed → arm 1                         // lower bound: no read needed
  if act.tier == Unknown or (act.evidence == Stale and !blind_in_band(act)):
      NeedReading([act if !in_backoff(act)] + reset_pending(C) by soonest reset)
      → Stay(active_unknown)                                           // logged, never silent

  // 1. Serve — active lb-blocked (I1). Held picks get here only once lb-blocked (I6).
  if act.tier == Blocked or act.lapsed:
      t = best(servable && !drained_latched) ?? best(servable)         // drained latch yields to I1
      if t → Switch(t, Serve)
      pending = reset_pending(C) not yet attempted this cycle                        // storm-guard exempt
              ∪ {c : !lb_blocked(c) && !servable(c), not attempted this cycle}       // e.g. pre-leave reading; storm guard applies
      if pending → NeedReading(pending, reset-pending first)   // budget §5.7; run before any active fetch
      // after the shell re-decides, failed reads are `unverified` and not pending → at most one re-decide delay
      if use_credits and t = best(tier == Credits && verified) → Switch(t, Credits)
      if Prepare preconditions (§3.6) → Switch(best(tier == Blocked), Prepare)
      Stay(all_blocked)

  // 2. Credits active (I7)
  if act.tier == Credits:
      t = best(verified && (Ready || (Draining && hard_runway ≥ MIN_LANDING)))
      t ? Switch(t, OffCredits) : (reset_pending(C) ? NeedReading(them) : Stay(on_credits))

  // 3. Draining active
  if act.tier == Draining:
      if pick && pick.settled ≥ Draining → Stay(manual_hold)          // until lb-blocked → arm 1
      if pick && act.evidence != Real → NeedReading(act if !in_backoff) → Stay(manual_pick)   // 0.10.4
      if t = best(verified && Ready) → Switch(t, Trigger)              // I3, safe
      if lb binding ≥ DRAIN_EXIT_PCT:                                   // a reading, never a projection (R3-M1)
          t = best(verified && Draining && !drained_latched
                   && t.hard_runway ≥ max(MIN_DRAIN_LANDING, act.hard_runway + DRAIN_GAIN))
          if t → Switch(t, Drain)
          if a candidate could qualify once read → NeedReading(it)
      Stay(draining)                                                    // ride to lb-blocked → arm 1

  // 3P. Blind-in-band projection (R2-B2), active Ready on its last reading, not a pick
  if blind_in_band(act) && !pick:
      if proj binding ≥ limit && t = best(verified && Ready) → Switch(t, Trigger)
      if best Ready candidate is unverified → NeedReading(it)          // pre-verify
      // the projection never drives Drain, Serve, Credits, Prepare or an unverified target

  // 4. Ready active
  if act binding est ≥ limit (within the 10 min cap) → treated as Draining (arm 3); only Trigger can fire
  if pick → Stay(manual_pick)
  Rebalance (§3.6) if cooldown elapsed → Switch(t, Rebalance)
  Stay(ok)

  // Evidence post-pass for every Switch (one rule, by kind):
  //   Serve:     target must be servable (no runway minimum, no freshness needed once read or tried — R2-M1)
  //   Trigger:   verified; if a NeedReading on it failed or was budget-deferred, allowed only with a post-leave,
  //              non-reset-pending reading AND soft_runway ≥ 2×MIN_LANDING (floored)
  //   Drain, OffCredits, Credits, Rebalance: verified; else NeedReading([target, next-best]) and no switch this pass
  //   Prepare:   no read (targets are lb-blocked by definition)
  //   A read not attempted because of the budget counts as failed for this rule.
```

`Decision` also carries `next_check_at` and `urgency ∈ {Calm, Band, Draining, Blocked}`. These feed the cadence
(§5): next_check_at is the earliest of (the estimate crossing the trigger or drain exit, the next reset of any
window, cooldown expiry while an optional move is pending, the pre-verify refresh time).

### 3.5 Persisted swap memory (B11)

Stored in `state.json`, keyed by provider. That restores the per-provider isolation 0.8.8 had in `SwapGuards`.

```rust
pub struct SwapMemory {
    pub last_switch_at: Option<i64>,               // auto or manual
    pub left_at: BTreeMap<String, i64>,            // account → when we last left it
    pub trigger_left: BTreeMap<String, i64>,       // account → epoch resets_at of the binding window it was left in
    pub drained: BTreeMap<String, i64>,            // same, for Drain/Serve exits
    pub stuck_notified: bool,
    pub last_seen_active: Option<String>,          // [v3.1] External detection
}
```

`BindingShown` (§4) stays **in memory** in the menu-bar process. It is not persisted (R2-M2).

**[v3.1] Every change of active account is recorded (R2-M2, R3-B1).**
`SwapMemory::note_active_change(from, to, now, kind: Auto|Manual|External)` sets `left_at[from] = now` and
`last_switch_at = now`, and clears the manual pick unless `kind == Manual` (which sets a new one). It fires **only
when the active account key changes**:

- `switch_if_still_active` (Auto)
- `switch_to` / `switch_to_provider_account` from the menu or CLI (Manual)
- **External**: detected at the start of each engine cycle under the state lock, when
  `state.active ≠ memory.last_seen_active` and no Auto or Manual note recorded the change. `last_seen_active`
  is a new `SwapMemory` field, updated by every note.

Token adoption for the **same** account (`sync_active_from_keychain outcome=adopted`, 324 lines in today's log,
and Codex `active_refresh_adopted`) is not an active change and calls nothing. Today no code path changes `active`
on an external login (`identity_mismatch` returns early), so External is a safety net for state edited by
another process or a future adoption path.

**[v3] Merge under the state lock.** Memory is never written from a pre-fetch copy. Every write is
`with_state_lock { reload; merge; save }`, and the daemon writes it in the same locked section as the CAS switch.
Merge rules:

- `left_at`, `trigger_left`, `drained`: union, keeping the max per key.
- `last_switch_at`: max.
- `stuck_notified`: the daemon's value; only the engine owner writes it.

**[v3] Single engine owner (open decision 8).** The engine runs only in the process holding a non-blocking flock
on `~/.config/usagio/engine.lock`. That's the menu-bar app, or `usagio watch` if no app is running. A second
process logs `event=engine_lock_busy` and runs display-only: it reads state and runs no poll, decide or switch.
That avoids double request budgets. The tray computes `binding_window` from the same `usage_log` samples with the
same `rates` function (read-only).

**[v3] Rollback compatibility (open decision 7).** While v1 exists, v2 **dual-writes** `manual_holds` (held ⇔
settled tier ≥ Draining) alongside `manual_pick_tier`. v1 ignores `swap_memory`. A 0.12.1 downgrade drops
unknown fields on save, which is harmless. The documented rollback is **Settings ▸ Auto-swap ▸ Engine:
Classic** (`State.autoswap_engine = "v1"`). `USAGIO_AUTOSWAP_ENGINE=v1` is secondary and reaches only
`usagio watch` or the CLI, not a launchd or login-item app.

**[v3] Latched-but-roomy candidates (open decision 3).** An account latched `trigger_left` is **Draining**
whatever its runway. It may be a drain target (gain rule, not `drained`-latched) and a Serve target. It may never
be a Trigger or Rebalance target. Its room is logged as `latched_pts` in the stranding log (R2-m4).

**Latch rules (B1, B8).**

- An account left by **Trigger** is not a Ready target again until its binding window resets, or until a Real
  post-leave reading shows it ≥ `LATCH_CLEAR_PTS` (5) under the trigger.
- An account left by **Drain/Serve** is not a drain target until it resets. It can be a Serve target only when
  every other switchable account is Blocked, and only on a post-leave reading < `BLOCKED_PCT`.
- Entries are pruned when their epoch's reset passes, or after 8 days.

### 3.6 Named rules

| Rule | Definition |
|---|---|
| **Cooldown** | `SWAP_COOLDOWN = 300 s` since `last_switch_at`. Applies only to Rebalance and Prepare. A cooldown-blocked move is logged as `action=blocked reason=cooldown` (as today). |
| **Rebalance** (use-it-or-lose-it) | Active is Ready and unpicked. The candidate is verified Ready, not latched, and its weekly reset is `< now + REBALANCE_HORIZON` (6 h) and earlier than the active account's weekly reset. Not announced as a decision (keeps 5749). **Why 6 h:** the only value of a flip is room that would otherwise expire. Beyond ~6 h (one 5-hour session window plus margin) the active account will normally exit on its own (trigger, or a session block) before the candidate's reset, and drain mode uses the room then. Inside 6 h, the next natural exit may come after the reset. 24 h (v1) flipped on resets almost a day out and was a hard step (B17). The comparison is strict `<` with `now` injected. |
| **Prepare** (all blocked) | All must hold (B12, v1 parity): the active account is lb-blocked with a known `blocked_until`; every switchable row has data; no reset-pending row is still unattempted this cycle; the target is lb-blocked with `blocked_until` strictly earlier than the active account's; cooldown has elapsed; no pick is active; the active account is not on credits. |
| **Credits** | §2 I7. |
| **Stuck notice** | Fires once on entering `Stay(all_blocked)`, or `Stay(draining)` with no drain target. Re-arms only on a switch, or on a *Real-reading* tier change of the active account. It is cleared when the active account is Ready or held (keeps 5843 semantics, B8). Never fires for `on_credits` or `manual_hold`. |
| **Decision log** | One `event=swap_decision v=2 …` line per *change* of (action, reason, target), and every switch. This removes the per-cycle Codex `on_credits` spam. |

### 3.7 Constants (`autoswap::policy`)

| Name | Value | Note |
|---|---|---|
| `BLOCKED_PCT` | 99.5 | the single definition of blocked |
| `DRAIN_EXIT_PCT` | 99 | drain exit needs a reading with `lb ≥ 99` (never a projection) |
| `MIN_LANDING_MIN` | 15 | Ready entry (owner Q2) |
| `MIN_DRAIN_LANDING_MIN`, `DRAIN_GAIN_MIN` | 5, 5 | the real-gain rule. It supersedes 0.8.8's 5-point gain: 1 weekly point at the default rate is 4 min, which is under the gain |
| `REAL_MAX_AGE_S`, `EST_MAX_AGE_S` | 180, 600 | evidence classes |
| `SAFETY` | 1.25 | drift multiplier |
| `BLIND_BAND`, `BLIND_AFTER_S` | 5, 120 | blind projection (§3.3) |
| **[v3]** `BLIND_RATE` | 2.0 session / 0.67 weekly pct/min | worst plausible blind rate (observed 17:52–17:55) |
| **[v3]** `VERIFY_PER_HOUR`, `PROVIDER_REQ_PER_MIN`, `STORM_GUARD_S` | 20, 3, 60 | request budget (§5.7) |
| `DEFAULT_RATE` | 1.0 / 0.25 pct/min | floor and no-data default |
| `RATE_WINDOW_MIN`, `RATE_GAP_MAX_MIN`, `PEAK_WINDOW_MIN` | 30, 15, 5 (over 60) | rates |
| `TARGET_VERIFY_MAX_AGE_S`, `POST_LEAVE_SETTLE_S` | 120, 60 | evidence (`RESET_SKEW` deleted in v3.1: reset-pending is judged by the reading's own `resets_at`) |
| `MAX_VERIFY_FETCHES`, `VERIFY_MIN_INTERVAL_S` | 2 per cycle, 120 per account | request budget |
| `SWAP_COOLDOWN_S`, `MANUAL_PICK_SETTLE_S` | 300, 300 | unchanged |
| `LATCH_CLEAR_PTS`, `REBALANCE_HORIZON_H` | 5, 6 | |
| `ACTIVE_TIGHT_FLOOR_S`, `ACTIVE_BAND_429_CAP_S` | 60, 120 | cadence (owner Q7, B3) |
| `BIND_SWITCH_MARGIN`, `BIND_MIN_DWELL_S` | max(10 min, 20%), 300 | display hysteresis |

Deleted: `NO_RETURN_SECS`, `ESCALATION_MIN_BAND`, `ESCALATION_MIN_CONSECUTIVE_429S`, `FALLBACK_MIN_GAIN_PTS`
(→ drain gain), `UNVERIFIED_TARGET_MIN_HEADROOM_PTS` (→ 2×MIN_LANDING plus I5), `PROACTIVE_HEADROOM_MARGIN`.
`--ceiling` stays: it only sets `limit_session`.

---

## 4. New feature: binding-window display

The tray percentage and the active row show the window that will **block first**. The function is shared with
the engine: `autoswap::runway::binding_window(&AccountIn, now) -> Binding { window, pct, time_to_block }`.

1. For each present window, compute `time_to_block = time_to(100)` using the account's own rate (not floored:
   this is a display of reality).
2. A window whose `resets_at < now + time_to_block` doesn't bind.
3. Binding = the binding window with the smaller `time_to_block`.
4. No rate (≤ ε, or no samples), or no window binds: use the higher pct. A tie goes to the higher pct.
5. A reset-pending window shows `—` (it's an upper bound), and the other window is used.

**Hysteresis.** `BindingShown { window, since }` is kept in memory. Switch the displayed window only when:

- **[v3]** the other window's `time_to_block` is shorter by **20% (relative only)** and `BIND_MIN_DWELL`
  (5 min) has passed; or
- **[v3] override, no dwell:** the other window has `time_to_block < 15 min` or `pct ≥ trigger` (R2-m2). For
  example, session 70% and weekly 96 shows `96%w` at once; or
- the displayed window reset or became blocked; or
- in no-rate mode, the other pct is ≥ 3 points higher.

**[v3]** The tooltip figure is labelled "at current pace" (own rate). The engine's floored-rate verdict is shown
separately when it differs, e.g. "auto-swap treats it as near limit" (R2-m3).

**Rendering.**

- Tray: session is `47%`, weekly is `96%w` (e.g. dev4 at 17:16 shows `96%w`, not `29%`). Recovery titles
  (locked countdowns, No plan) are unchanged.
- Active account row (`trailing_for_account`): the existing `sa / sb` text, with the binding window's segment
  bold and severity-tinted (the color ranges already exist).
- At-risk tray target (`tray_target`, default mode): pick the active account with the smallest
  `time_to_block`, falling back to the highest pct.
- Tooltip: `email — session X, weekly Y%w ← binds, blocks in ~Nm`.

Codex has no session window, so it shows `1%w`.

---

## 5. Polling changes (same release, required by the model)

1. **Active poll floor 60 s in the TIGHT band** (owner Q7). It was 30 s, and 30-second polls are what draw the
   429s. This applies only together with the blind projection (§3.3).
2. **Active 429 backoff while in band or Draining is capped at 120 s.** Outside the band, the current doubling
   applies, up to `WATCH_MAX_INTERVAL_SECS`.
3. **Honor the active account's own backoff.** Today the loop wakes at the minimum over providers and a 429
   doesn't advance `fetched_at`, so the account is re-fetched on every wake. Use the per-provider `current`
   interval as the active fetch floor.
4. **`actionable` comes from `Decision.urgency`.** Draining or Blocked with any verified or unknown candidate
   means TIGHT cadence, not BASE (D3).
5. **Wake at `next_check_at`.** A wake with no fetch due still runs `decide`.
6. **Run `NeedReading` fetches even in a cycle where the active account 429'd.** The limit is per token: today
   verify fetches succeeded seconds after active 429s, at 16:23:38 and 17:11:14. They are limited by
   `MAX_VERIFY_FETCHES` and `VERIFY_MIN_INTERVAL`.

7. **[v3] Per-provider request budget (R2-M4, R2-B3, open decision 5).** Usage requests per provider:

   | Item | Limit |
   |---|---|
   | Active fetch | ≤ 1 per 60 s in band or Draining (≤ 1 per 120 s while 429ing); BASE 180 s otherwise. **[v3.1] Never while the active is lb-blocked** (R3-B2): it is fetched again only once that window is reset-pending, at the 12 h `MAX_LOCKED_STALENESS` cap, or on a forced refresh (Refresh now, banked-reset claim). `NeedReading(active)` is not issued while the active is in backoff or 429'd this cycle, and counts toward the per-minute cap. |
   | Verification reads (`NeedReading` on other accounts) | ≤ 2 per cycle; ≥ 120 s apart per account; **≤ 20 per hour per provider**; none for 60 s after a 429 on any token (storm guard). **[v3.1] Reset-pending reads are exempt from the storm guard** (≤ 1 per account per 120 s, still within the hourly cap and the per-minute total) and run before the active fetch when the active is lb-blocked. |
   | Total | **≤ 3 per 60 s per provider** (active + verification). The excess is deferred to the next cycle and logged `event=budget_defer`. |
   | Locked accounts | **0 extra.** lb-blocked rows are never verification targets. Only reset-pending windows (once per reset, as v1's reset wake already does) and chosen targets older than 120 s are read. |

   **Overnight all-blocked:** the same as v1. The active fetch runs at BASE (no candidate is servable or
   pending), plus one read per account reset. Today's 02:11–04:58 window would add 0 requests.
   **429 streak in band (15:38–16:01 shape):** at most 1 active attempt per 120 s (12 attempts vs v1's 8) and
   no verification reads within 60 s of any 429. In the worst case the total stays under v1 plus 4.
   Per-token `outcome=429` logging (§6) confirms or refutes the per-token claim within days. If 429s turn out to
   be correlated, the 60 s storm guard already covers it.

Kept unchanged: the inactive 600 s floor and failure backoff, the locked-account skip, polling the active
account when it is `needs_relogin`, active-first order and sparing inactive fetches on an active 429 (except
`NeedReading`), refreshing a newly switched-to active account immediately, per-provider cadence, and the
wake-from-sleep detection. **Changed in v3.1:** an lb-blocked active is no longer polled (row above).

---

## 6. Logging for replay (B5, B11; ships in 0.12.2)

- **`usage_log::Snapshot` gains fields**, all `#[serde(default)]` so old rows still parse:
  - `active: Option<bool>`
  - `session_reset`, `weekly_reset: Option<DateTime>`
  - `fetched_at: Option<i64>`
  - `outcome: Option<"ok"|"429"|"err"|"skip">`

  One row is written per account per cycle, including 429/err/skip rows (with the cached pct). **Codex rows are
  added** (currently 0 of 16,992 October rows).
- **New `decisions.<YYYY-MM>.ndjson`** (same rotation and retention as `usage_log`). It gets one line per
  decision change plus every switch, including manual ones: `{ts, provider, engine, action, reason, target,
  active_tier, est, lb, proj, runway, rates, need_reading, budget}`.
- **[v3] Shadow comparison dropped** (R2-M3). v1's `evaluate_swap` reads a `SwapGuard` and 429 tracker that v2
  doesn't maintain, so its disagreements would be artifacts. Engines are compared by the `engine` field across
  rollback periods instead.
- **[v3]** Fix v1's mislabelled Prepare log line (`reason=soonest_reset_in_band` at 17:55:46 → `reason=prepare_reset`)
  in the v1 code kept for rollback (R2-m5).
- **Epoch-end stranding log (B18).** When any window resets, log the last pre-reset pct of that window, i.e.
  how much room expired unused. This measures the real cost of `MIN_LANDING` over a cycle.

---

## 7. Replays (rule named for each step)

Method: §8.2's simulator uses real starting readings and resets, with the workload rate observed *at that time*
on whichever account was in use. That rate is ~0.71/0.18 pct/min from 16:25 to 17:10, then ~1.8/0.47 from 17:11
(it's the user's workload, not the account). Polls run every 60 s in band, with 429s on alternate polls as
observed from 16:03 to 16:21. The times below are what the sim must reproduce within ±2 min. They're stated as
the expected trace, not a measurement. Blocked minutes are bounded by one poll at each ride-to-block point.

### 7.1 Today on the v2 path (stays on dev4 at 16:23)

| Time | State | Decision, rule |
|---|---|---|
| 16:23:06 | dev4 Real 12/91 | Ready. Stay. |
| 16:23:37 | 429, age 31 s | est ≈ 91.1. Stay (**I8**). Backstop not yet active (age < 120 s). dev8 (0/80, read 16:20:05) is a Ready candidate, so pre-verify it once dev4 has been blind 120 s. |
| ~16:45 | dev4 weekly reaches 95 (0.18/min). A Real reading or the blind projection reaches the trigger | Switch → dev8 (**I3 Trigger**, safe move, dev8 verified ≤ 120 s). dev4 latched `trigger_left`. |
| ~17:31 | dev8 reaches 95 (+4 by 17:10, then ~0.5/min) | Draining. dev4 (≈ 95 weekly) is not Ready (latched, and 0 points under the trigger). **Stay(draining)**. Stuck notice once. |
| ~17:39 | dev8 Real 99 | Drain arm: dev4 hard runway (99−95)/0.5 ≈ 8 min ≥ max(5, 0 + 5). Verify → Switch → dev4 (**Drain**). dev8 latched `drained`. |
| ~17:47 | dev4 Real 99 | Drain arm: dev8 is drained-latched. Stay; ride to a Real block. |
| ~17:49 | dev4 Real 100 | **Serve (I1)**: no unlatched server, so the latch yields to I1 (dev8 post-leave reading 99 < 99.5). Switch → dev8. |
| ~17:51 | dev8 Real 100 | All blocked by Real readings, rows complete, `blocked_until` known. Prepare → dev6 (weekly 10-26 04:59), after the cooldown (~17:54). |

Result: 4 automatic switches, no manual switch. Blocked time is at most one poll interval at ~17:49 and ~17:51,
about 2 min total. Actual (v1): two needless 429 swaps, blocked on dev4 from 17:28:31 to the manual switch at
17:30:55, and v1 would have stayed stuck until 17:31:14.

**Counterfactual on v1's actual state at 17:11:14** (on dev8 40/90, 429): Stay (**I8**). dev4 16/92 is Draining
for any rate, because at the floored 0.25/min, 3 points is 12 min < 15. The 0.18-vs-0.21 wobble the review
found in v1 no longer matters (B6). dev8 reaches a Real 95 around 17:21–17:38 depending on the rate → Stay
(draining) → drain exit at Real 99 → dev4 if it has ≥ 5 min more hard runway → then Serve/Prepare as above.

### 7.2 v0.8.7 thrash (2026-10-01, trigger 85; real samples 07:25–09:47: dev@ 16–24 session / 82–85 weekly; dev4 55–75 / 39–44; dev3 and matthew@ also in rotation)

- **On dev@ with 429s:** Stay (**I8**).
- **Flip back to dev@ while it's at 82–84:** impossible.
  - Primary rule: with 3 points under the trigger at the floored 0.25/min, dev@ has 12 min < `MIN_LANDING`, so
    it isn't Ready.
  - Secondary: the dev@ reset (~10-02 18:00) is outside `REBALANCE_HORIZON`.
  - Once left by Trigger it's latched until its reset, since it never drops ≥ 5 under the trigger.
- **dev@ reaches a real 85:** Trigger → the best verified Ready account, by runway.
  - dev4 at 07:25 (58 session) has (85−58)/1.0 = 27 min, so it qualifies.
  - dev4 at 09:42 (75 session) has 10 min, so it doesn't, and dev3 or matthew@ is chosen instead.
- **Expected:** at most 1 exit from dev@ per epoch, and no A→B→A. That's the I4 proptest, plus a sim seeded
  with these samples.

### 7.3 v0.8.7 blind run (dev@ session 93, trigger 95, 429s)

See §11.1. With the §3.3 blind projection (rate ≥ 2.0 session/min, no cutoff), Trigger fires onto the
pre-verified Ready target about 2 min into blindness, whatever the EWMA.

---

## 8. Implementation plan (for a separate engineer)

### 8.1 Files

**Add `src/autoswap/`:**

| File | Contents |
|---|---|
| `mod.rs` | `Snapshot`, `AccountIn`, `WindowIn`, `ManualPick`, `Tier`, `Evidence`, `Decision { action: Action, reason: Reason, need_reading: Vec<String>, next_check_at, urgency }`, `Action::{Stay, Switch{to, kind: Kind::{Trigger, Drain, Serve, OffCredits, Credits, Prepare, Rebalance}}, NeedReading}`, `pub fn decide(&Snapshot) -> Decision` |
| `runway.rs` | `time_to(pct, limit, rate) -> f64` (ε guard), `est_window`, `binding_window`, `BindingShown::update` |
| `assess.rs` | `Assessed`, `assess(&AccountIn, &Ctx) -> Assessed`, `rank_key`, `verified` |
| `rates.rs` | `AccountRate { session, weekly, peak_session, peak_weekly }`, `rates_from_samples(&[usage_log::Snapshot], key, now)` |
| `memory.rs` | `SwapMemory` (serde), latch helpers, `prune(now)` |
| `policy.rs` | the §3.7 constants plus `Policy` |
| `shell.rs` | `build_snapshot(&State, slug, &rates, now)`, `run_cycle(slug, …)` (decide → NeedReading via `verify_swap_target`, at most 2 → re-decide → `switch_if_still_active` → memory, notify, log) |
| `sim.rs` `#[cfg(test)]` | fleet simulator: accounts with true pct and resets, a workload rate per time segment, a poll schedule, per-account 429 masks, manual switch events. Returns a trace (switches, blocked minutes, notices). |
| `tests.rs` | table tests, §8.3 |

**Change `src/main.rs`:**

- `provider_swap_cycle` delegates to `autoswap::shell::run_cycle` when `engine == v2`. The old body is kept
  behind `engine == v1` for one release. The engine is chosen by `State.autoswap_engine: Option<String>` or the
  env var `USAGIO_AUTOSWAP_ENGINE=v1`.
- `watch_cycle` loads `SwapMemory`. `SwapGuards` is used only by v1.
- `Cadence::advance` and `next_interval` take `urgency`. TIGHT active floor 60 s. Band 429 cap 120 s.
  `next_check_at` wake.
- The `refresh_usage_cache` and `refresh_provider_usage_caches` active floors use the per-provider backoff and
  append the new `usage_log` fields and outcome rows.
- `settle_manual_pick` and `release_reset_hold` store a `settled` tier (`manual_pick_tier` map). The hold
  becomes "tier ≥ Draining".
- `auto_pick` and `optimize_now` use `autoswap::assess::rank_key`.

**Other files:**

- `src/store.rs`: add `swap_memory: HashMap<String, SwapMemory>`, `autoswap_engine`, `manual_pick_tier`. Read
  the legacy `manual_holds` (a held key maps to tier Draining).
- `src/usage_log.rs`: add the new fields and a `decisions` appender.
- `src/menubar.rs`: `title_for`, `tooltip_for`, `tray_target`, `trailing_for_account` use `binding_window`.
- `src/burn_rate.rs`: remove its use in the poll projection. The menu "empty in" rows call `runway::time_to` so
  there is one rate model.

**Delete in 0.12.3, once v1 is retired:** `evaluate_swap`, `evaluate_swap_verified`, `choose_swap_target`,
`SwapEval`, `stale_swap_candidate`, `active_in_trouble`, `earliest_blocked_target`, `worth_returning_to`,
`candidate_order`, `Row::{eligible_target, forced_target}`, `swap_fire_pct`,
`effective_active_max_pct_for_swap_fire`, `held_on`, `picked_on`, `hold_applies`, `is_choice`,
`fresh_since_left`, `SwapGuard`, `SwapGuards`, `prune_swap_guard`, `project_active_max_pct_at_horizon`,
`per_account_fetch_floor_secs_with_projection`, `tighten_tier_once`, `UsageFetchTracker.consecutive_429s` (as a
swap input; the fetch backoff fields stay), and the deleted constants.

### 8.2 Simulator contract

`sim::run(Scenario) -> Trace`. A `Scenario` has:

- `start: DateTime`
- `accounts: [(key, session_pct, session_reset, weekly_pct, weekly_reset, credits)]`
- `workload: [(from, session_rate, weekly_rate)]`
- `poll: cadence model from §5`
- `mask_429: per-account fn(t)->bool`
- `verify_fail`
- `manual: [(t, key)]`
- `active`

Only the active account accrues usage. A reset drops a window to 0 at `resets_at`. Readings are integers, like
the API. `Trace` has `switches[(t, from, to, kind)]`, `blocked_minutes`, `notices`, `fetches`.

**[v3] Realism knobs (R2-M3).** Each defaults to the observed behavior, and S1–S11 must pass with them **on**:

- `post_leave_lag`: the account just left keeps accruing +1–4 session / +0–1 weekly points over 60 s (dev4
  after 16:23:38).
- Integer readings sampled at poll time, so values get skipped (dev8 98→100, 17:52:43→17:55:46).
- `mask_429: Correlated(p)`: one draw per cycle for every token, alongside per-account masks.
- `sleep: [(from, to)]`: no polls, no local usage, and the first fetch after wake fails.
- `external: [(t, key)]`: the active key changes outside usagio's switch paths (External, §3.5). Same-account token adoption is also simulated every cycle and must change nothing.
- `budget`: enforces §5.7 and fails the test on any overrun.

### 8.3 Test list

**Table tests on `decide`** (`tests.rs`, one row each; `now` is injected; times are UTC 2026-10-25 unless
noted).

Incident and history rows:

| # | Name | Setup | Expect |
|---|---|---|---|
| T1 | `today_1711_429_on_fresh_reading_stays` | active dev8 40/90 read 17:10:43, now 17:11:14, 429; dev4 16/92 | Stay; dev4 is Draining |
| T2 | `today_1623_429_31s_old_stays` | dev4 12/91 read 16:23:06, now 16:23:37 | Stay |
| T3 | `today_1728_real_block_serves_immediately` | dev4 Real 47/100 at 17:28:31; dev8 41/90 read 17:22:27 (post-leave), left 17:11:14 | Switch dev8 (Serve) this cycle; no read required (servable via its post-leave reading, §3.4 arm 1) |
| T4 | `today_session_vs_weekly_ranking` | candidates 16/92 vs 40/90 | 40/90 ranks higher |
| T19 | `rebalance_horizon_strict_and_6h` | resets at now+5h59m vs now+6h; boundary at fixed `now` | the first flips, the second doesn't |
| T20 | `ready_requires_floored_runway` | idle (rate 0), candidate 94.9/95 | Draining, not Ready (B7); no NaN |
| T21 | `rate_includes_zero_deltas_and_skips_gaps` | flat 30 min; then an 8 h gap | rate 0; gap ignored |
| T22 | `credits_and_codex_logging` | Codex single account on credits, 10 cycles | 1 decision line, not 10 |
| T23 | `prepare_requires_lb_block_and_known_until` | v1 parity (B12) | no Prepare if any row is missing data or `until` is unknown |
| T24 | `drain_gain_rule_supersedes_5pt` | active 99 vs 98 / 95 / 90 | no / yes (≥ 5 min) / yes |

Blocker scenarios from the review:

| # | Name | Setup | Expect |
|---|---|---|---|
| T5 | `b1_two_at_real_99_no_pingpong` | A 99 (dev4 43/99 at 17:25:29), B 99 | Stay; ride A to 100 → Serve → B; ≤ 1 switch before both block |
| T6 | `b2_429_streak_no_phantom_prepare` | dev4 4/89 read 15:36:59, now 16:01 (age 24 min), 429s; dev8 100/80 session-reset 16:19:59; rest 100 weekly | Stay(active_unknown); no Prepare; no notice; NeedReading(dev4) only once its backoff expires (none while in backoff) |
| T7 | `b2_sleep_8h_no_switch_on_no_info` | active read 8 h ago at 92 (8 h sleep detected, blind_age excludes it); first post-wake fetch fails; verify of the target fails | blind-in-band (92 ≥ 90): projection near 92 (sleep excluded) → Stay; no switch; the regular active poll re-reads it after backoff (§5.7) |
| T8 | `b3_burst_after_idle_backstop` | dev4 last real 12/91 at 16:23:06, peak 0.67/min weekly, blind 120 s+, dev8 0/80 verified | pre-verify dev8; Switch (Trigger) once proj ≥ 95 |
| T9 | `b4_reset_pending_verified_first` | active dev8 Real 100 weekly; dev6 reset 10-26 04:59:59, dev7 07:59:59, now 05:00, active 429 | NeedReading(dev6) before any Prepare toward dev7 |
| T10 | `b4_unknown_active_keeps_weekly_block` | dev4 47/100, session reset 19:30, now 19:31 | session is reset-pending but weekly lb 100 still blocks → arm 1 (Serve/Prepare per rules); dev4 not fetched (lb-blocked); never Stay(ok) |
| T11 | `b4_credits_leave_after_reset_verified` | on credits; a plan account's reset passed | NeedReading(it) → OffCredits if Ready |
| T12 | `b8_phantom_drift_no_aba` | A crosses by drift → B; B crosses by drift; A Real 4 under the trigger | no return to A (latched; needs ≥ 5 under) |
| T13 | `b9_unsettled_pick_96_holds` | manual pick at cached 96, 429s, no fresh reading | Stay(manual_hold) |
| T14 | `b9_pick_leaves_only_on_real_reading` | pick at 93, est 95.5, age 4 min | NeedReading(active), no switch; Real 95 → Switch |
| T15 | `b9_held_pick_leaves_at_real_99_5` | held at 97; est 99.2 / Real 99.5 | Stay / Serve |
| T16 | `b10_serve_rejects_preleave_reading` | target read before `left_at` | NeedReading, not Switch |
| T17 | `b15_post_leave_reading_needs_60s` | left dev4 16:23:38, reading at 16:23:50 | not evidence |
| T18 | `reset_pending_by_reported_reset` | stored reading reports `resets_at` = now − 60 s; a re-read 60 s later still reports it (fast local clock) | pending; re-read ≤ once per 120 s; a reading reporting the next `resets_at` is post-reset |

**Simulator scenarios** (`sim` tests):

| # | Scenario | Assertion |
|---|---|---|
| S1 | today, v2 path from 16:20 (§7.1) | ≤ 5 switches; blocked ≤ 3 min; 0 manual needed |
| S2 | today from v1's 17:11 state | no switch before dev8 reaches Real 95 |
| S3 | v0.8.7 thrash, Oct-1 seeds, p(429) = 0.5, 07:00–10:00 | ≤ 1 exit from dev@ |
| S4 | v0.8.7 blind run, 93%, 429s for 6 min | switch ≤ 2 min after the true crossing |
| S5 | B3 burst: rate 0 for 30 min, then 0.6/min weekly, p(429) = 0.5 in band | blocked ≤ 2 min |
| S6 | B2 15:36–16:23 streak (8× 429 15:38–15:58) | 0 switches |
| S7 | B1 two accounts at 99, 30 s and 60 s polls | ≤ 1 switch before both are blocked |
| S8 | all blocked, overnight resets (dev6 04:59:59, dev7 07:59:59) with an active 429 streak | on dev6 by 05:05 |
| S8b | S8 with an overnight manual pick on dev8 and every dev8 fetch 429ing | on dev6 by 05:05; no Prepare while the pick is active; 0 dev8 fetches while it is lb-blocked |
| S9 | credits: plan exhausted → credits → a plan account resets | leaves credits ≤ 2 cycles after the reset is verified |
| S10 | manual picks at 79 / 96 / 100 with 429s | 79: leaves at Real 95. 96: holds to Real 99.5. 100: leaves for a verified server; never prepared away |
| S11 | idle overnight, then resume | no switch while idle; no flip into the band |

**[v3] Additional table tests:**

| # | Name | Setup | Expect |
|---|---|---|---|
| T25 | `r2b1_old_full_reading_still_blocked` | dev4 47/100 read 17:28:31, now 17:35:11 (400 s), every active fetch 429; dev8 verified 93 | Switch dev8 (Serve) |
| T26 | `r2b3_overnight_locked_rows_need_no_reads` | 02:30 on 10-26, active dev6 lb 100 weekly (until 04:59:59); 9 other rows lb 100, read 6 h ago, all later resets | 0 NeedReading; 0 active fetches; **no Prepare** (dev6 is already earliest); Stay(all_blocked) |
| T27 | `r2b2_blind_projection_no_cutoff` | last Real 90 weekly, idle peak 0, blind 25 min, verified Ready target | Trigger at proj ≥ 95 (~6 min blind at 0.67 × 1.25) |
| T28 | `r2m1_serve_accepts_postleave_reading_after_failed_verify` | dev4 Real 100 at 17:28:31; dev8 41/90 read 17:22:27, left 17:11:14; verify 429 | Switch dev8 (Serve) |
| T29 | `r2m1_trigger_still_needs_2x_landing_if_unverified` | active Real 95, target 20 min runway unverified | no switch |
| T30 | `r2m1_reset_pct_drop_counts_as_post_reset` | dev6 0/0 at 05:00:05 reporting the next weekly reset (11-02 05:00); old reset 04:59:59 | post-reset → Ready, no re-read |
| T31 | `r2m2_manual_switch_records_left_at` | manual switch A→B at t; A read at t−30 s | A's reading is not post-leave evidence |
| T32 | `r3b1_token_adoption_is_not_an_active_change` | `sync_active_from_keychain outcome=adopted` for the active key, pick set, last switch 10 min ago | `left_at`, pick and `last_switch_at` unchanged; plus a case where `state.active` changes with no note → External: `left_at` set, pick cleared, cooldown restarts |
| T33 | `r2m2_memory_merge_keeps_cli_write` | daemon writes after a CLI wrote `left_at[X]` meanwhile | both entries survive |
| T34 | `r2m2_dual_write_manual_holds` | pick settled Draining | `manual_holds[slug]` written; v1 holds after a toggle |
| T35 | `r2m4_budget_storm_guard` | 429 on any token within 60 s | 0 verification reads except reset-pending ones; ≤ 3 per 60 s total |
| T36 | `r2b2_dev8_1752_skip_to_100_with_pick` | dev8 = manual pick (17:30:55, settled Ready at 90); Real 70/98 at 17:52:43, then 429; all else lb 100 | 17:52 Stay(draining) (pick, no Ready target); 17:55:46 Real 100 → arm 1: nothing servable, **no Prepare (pick)**, Stay(all_blocked); 0 dev8 fetches afterwards |

**[v3.1] Round-3 table tests:**

| # | Name | Setup | Expect |
|---|---|---|---|
| T37 | `r3b3_claimed_reset_starts_new_epoch` | dev8 lb 100 weekly (reset 11-01 03:00); a fresh 0/0 reading, still reporting reset 11-01 03:00 | lb = 0 → not Blocked; Ready as a candidate; `trigger_left`/`drained` latches void; as active, no Serve away |
| T38 | `r3b2_pick_on_blocked_dev8_reset_read_moves_user` | pick on dev8 (lb 100, 10-25 21:00 onward), every dev8 fetch would 429, 429 on dev8 within the last 60 s; dev6 resets 10-26 04:59:59 | dev8 not fetched; at 05:00 NeedReading(dev6) despite the storm guard; dev6 0/0 → Switch dev6 (Serve, I6) |
| T39 | `r3m1_projection_never_drains` | blind-in-band, proj ≥ 99, real last reading 93, verified Draining target at 97 with gain | no Drain; Trigger only if a verified Ready target exists |

**[v3] Additional simulator scenarios:**

| # | Scenario | Assertion |
|---|---|---|
| S12 | usage lands right after a switch (post-leave lag on) through S1 | no return on a pre-lag reading |
| S13 | skipped integer values (98→100) at 60 s and 180 s polls | blocked ≤ 1 poll; no Serve before lb ≥ 99.5 |
| S14 | correlated 429 storm, p = 0.5 on all tokens, 25 min, in band | blocked ≤ 2 min (projection); budget held |
| S15 | sleep 8 h with the active at 92, first fetch after wake fails | 0 switches until a successful read |
| S16 | external active-key change to a blocked account mid-drain; same-account token adoption every cycle | Serve from it within ≤ 2 cycles; adoption never changes `left_at`, pick or cooldown |
| S4′/S5′ | extended to a 25-minute 429 streak | switch ≤ 2 min after the true crossing |

**Proptest** (`proptest` dev-dep): random fleets (2–9 accounts), workloads, per-account 429 masks, resets.
Assert:

- **I1**: never more than 2 cycles on an lb-blocked active while a servable account exists (excluding cycles in which every read fails).
- **I2**: forced moves don't downgrade.
- **I4**: latch counts hold, optional moves ≥ cooldown apart.
- **No-NaN** in ranking.

**Binding display tests** (`menubar` / `runway`):

- 17:16 dev4 29/96 with rates 1.8/0.47 → `96%w`.
- 16:30 dev8 12/83 with 0.71/0.18 → session: (100−12)/0.71 = 124 min vs (100−83)/0.18 = 94 min, so weekly
  binds → `83%w`.
- A session reset in 10 min doesn't bind.
- No rate → the higher pct.
- Hysteresis: alternating inputs within the margin don't flicker for 5 min.
- Codex with weekly only.

### 8.4 Existing tests (`src/main_tests.rs`)

- **Flip explicitly** (they assert removed behavior):
  - `first_429_near_trigger_swaps_immediately_even_after_a_recent_swap` → T2 plus S4.
  - `a_429_near_the_trigger_does_not_move_a_manual_pick` → a 429 moves nobody.
  - `provider_swap_cycle_labels_urgent_and_prepared_moves_as_non_proactive` (6897): replace its 429 setup with a
    Real 96.
  - `watch_cycle_reports_the_claude_429_and_swaps_claude_accounts` (6973): assert 429 reported, **no** swap.
  - `reactive_swap_drains_a_banded_account_that_resets_soonest` → the banded account is used in drain mode.
  - `choose_swap_target_proactive_swap_beyond_headroom_margin` and
    `proactive_flip_back_needs_a_full_margin_when_the_active_reset_is_unknown` → no swap.
  - `choose_swap_target_excludes_recently_left_account` → a post-leave reading allows the move.
  - `fallback_needs_a_real_gain` → T24.
  - `eligible_target_requires_session_and_weekly_strictly_below_band` → T20 family.
  - `unreachable_target_is_used_only_by_an_urgent_swap_with_ample_room` → the 2×MIN_LANDING plus I5 rule.
  - `next_interval_*` tests expecting 30 s in the TIGHT band for the *active* account → 60 s.
- **Delete:** the `escalation_*` tests (6), `effective_max_pct_escalates_from_exactly_the_band_edge`,
  `escalation_reads_the_providers_own_tracker`, the no-return halves of
  `no_return_window_lifts_after_twenty_minutes_and_cooldown_after_five` and
  `blocked_preparation_skips_an_account_left_within_the_no_return_window`, and
  `choose_swap_target_proactive_respects_no_return_window` (→ a cooldown plus I5 variant).
- **Keep, re-pointed at v2:** every other swap, manual, credits, verify, stale-rescue (as NeedReading) and
  auto_pick test listed in v1 §5.1. That includes `hold_lets_go_exactly_at_99_5` (now on a Real reading),
  `blocked_preparation_requires_known_future_resets_and_valid_login` (T23 parity) and
  `reset_clock_alone_never_clears_a_target_but_a_reading_at_the_reset_does`.
- **Keep unchanged (polling and infra, B13):**
  - `floor_with_projection_*` → rewritten to `next_check_at`
  - `per_account_fetch_floor_matches_the_cadence_ramp`
  - `cadence_keeps_one_providers_429_from_slowing_another`
  - `cadence_restarts_backoff_tight_after_a_switch`
  - `note_polled_active_reports_only_a_change_of_active_account`
  - the inactive backoff tests (3035, 3056, 3111)
  - `active_account_is_fetched_first_and_its_429_spares_the_inactive_ones` (plus a new case: NeedReading still
    runs)
  - the locked-skip and active-polled-when-`needs_relogin` tests. **Flip** `the_active_account_is_polled_even_when_it_looks_locked` (5295): an lb-blocked active is not polled until reset-pending (R3-B2)
  - the Codex polling tests (5540–5576)
  - `provider_swap_cycle_tracks_the_stuck_notice_only_while_stuck` (5843 semantics)
  - flip-back not announced (5749)
  - cooldown-blocked log (5891)
  - the wake-from-sleep and immediate refresh of a new active account tests.

### 8.5 Rollout (0.12.2)

1. Land `autoswap/` with the sim, table tests and proptests. **Gate: T1–T39 and S1–S16 plus S8b (incl. S4′/S5′) green, with the simulator's realism knobs on.** The gate is not
   shadow lines (B5).
2. Same release: logging (§6), polling (§5), the binding display (§4), persisted memory, and v2 as the default.
3. Rollback for one release: **Settings ▸ Auto-swap ▸ Engine: Classic** (primary). `USAGIO_AUTOSWAP_ENGINE=v1`
   is secondary and reaches only `usagio watch` or the CLI. v1 still reads `SwapGuards` in memory, and v2 memory isn't touched by v1.
4. 0.12.3: delete v1 (§8.1), after a week of `decisions.ndjson` review, including the budget log and
   the stranding log.

---

## 9. Review findings: resolutions

History. Where v3/v3.1 changed a rule, §2–§8 are authoritative.

| # | Sev | Resolution |
|---|---|---|
| **B1** | BLOCKER | **Fixed.** I verified the ping-pong against v1's pseudo-code: a 99 is under 99.5, so it counts as Draining. Fixes: the drain exit needs a Real reading ≥ 99 *and* a target with `hard_runway ≥ max(5 min, act + 5 min)` at the floored rate. Otherwise ride to a Real block, then Serve. The `drained` latch blocks re-entry, and yields only when every other account is blocked (I1). T5 and S7. |
| **B2** | BLOCKER | **Fixed.** Confirmed: dev4 4/89 from 15:36:59, 8× 429 from 15:38 to 15:58, next success 16:01:08 at 89; dev8 100/80 until 16:19:59. Fixes: extrapolation capped at `EST_MAX_AGE` (10 min); a Stale active gets `NeedReading(active)`; Blocked for the active needs a Real reading; Prepare and unverified Serve need a Real-blocked active; rates include zero deltas and skip gaps > 15 min. T6, T7, S6. |
| **B3** | BLOCKER | **Fixed.** Confirmed: the 16:20:04 → 16:23:06 burst was +8 session / +2 weekly. Fix: the rate-independent backstop (in band, blind > 120 s → drift at `max(rate, default, 5-min peak)`) pre-verifies a Ready target, and Trigger fires on the estimate within the cap. The 60 s floor and 120 s band 429 cap ship only together with it. T8, S5, S4. |
| **B4** | BLOCKER | **Fixed.** Confirmed: `stale_swap_candidate` exists for this, and inactive fetches are skipped after an active 429 (test 5199). Fixes: staleness is per window (reset-pending = an upper bound only); an Unknown active has an explicit arm; NeedReading ranks reset-pending accounts first when the active is Blocked, Credits or Draining, even during a 429 (§5.6, the limit is per token as 16:23:38 and 17:11:14 show); the reset-proximity rule applies to the active too. T9, T10, T11, S8. |
| B5 | MAJOR | **Fixed, with one part rejected.** Confirmed: no `active`, resets, outcome or Codex rows in `history.*.ndjson`, and `usagio.log.1` starts 10-24 21:18. Fix: the §6 logging plus `decisions.ndjson` ship now, and the gate is the simulator, not shadow. **Rejected:** waiting 2 weeks before switching engines. The owner chose 0.12.2 with v2 default; the sim scenarios built from real seeds plus the rollback switch cover the risk. |
| B6 | MAJOR | **Fixed.** I verified that v1 §4.1 used post-17:11 rates and that §4.2 didn't match Oct 1 (dev4 was 55–75 session). §7 is redone with rates the v2 path would observe, the rule named per step, and expected traces the sim must reproduce. The floored rate makes dev4 16/92 Draining for any EWMA. §7.2 now names the right rules (MIN_LANDING at the floored rate first, horizon second). |
| B7 | MAJOR | **Fixed.** Candidate runway uses `max(rate, DEFAULT)`, which implies a floor of ≥ 4 weekly / 15 session points. An ε guard prevents NaN, and ordering uses `total_cmp`. T20. |
| B8 | MAJOR | **Fixed.** `trigger_left` latch (cleared by a reset or a Real reading ≥ 5 under the trigger); active re-entry to Ready needs a Real reading < limit − 1; the stuck notice re-arms only on a switch or a Real tier change. Proptest with per-account 429 masks. T12. |
| B9 | MAJOR | **Fixed.** 0.10.4 is kept explicitly: picks leave only on Real readings. While unsettled, the tier comes from the switch-time reading (0.8.8 "not undone"). The held check sits inside the Draining arm, so a held pick reaches Serve only at a Real ≥ 99.5. A blocked pick leaves for a verified server (owner Q6). T13, T14, T15, S10. |
| B10 | MAJOR | **Fixed.** Unverified targets need a post-leave (+60 s) and post-reset reading, even under Serve. Left accounts drift to `left_at`. T16. |
| B11 | MAJOR | **Fixed.** `SwapMemory` is persisted per provider. I confirmed the restart at 17:31:26. Optional windows; an `unverified` flag in `AccountIn`; credits ranked within a unit; per-account rates with Codex samples; `burn_rate::estimate` retired for decisions and polling so there is one model. |
| B12 | MAJOR | **Fixed.** I confirmed both v1 preconditions in `earliest_blocked_target` (`return None` on a missing row; `.max()?`). Both are restored, plus a Real-blocked active. T23. |
| B13 | MAJOR | **Fixed.** I confirmed the two "keep" tests (6897, 6973) assert the 429 swap; they're now explicit flips. Every cadence, fetch-budget, Codex, wake, notice, refresh-on-switch and per-provider item has a stated fate (§5, §8.4). The 5-point gain is superseded by the drain gain rule (T24). The 10-point unverified rule is replaced by 2×MIN_LANDING plus I5. Both have reasons in §3.7. |
| B14 | MINOR | **Fixed.** I2 exempts Rebalance and Serve. |
| B15 | MINOR | **Fixed.** Confirmed: dev4 16/92 at 16:33:48 after 12/91 at 16:23:06. Evidence needs `fetched_at ≥ left_at + 60 s`. T17. |
| B16 | MINOR | **Fixed.** v3.1: reset-pending is judged by the reading's own reported `resets_at` (§3.3), with re-reads bounded by `VERIFY_MIN_INTERVAL`. `RESET_SKEW` is deleted. |
| B17 | MINOR | **Fixed.** The horizon is now 6 h, with strict `<` and an injected `now`. T19. |
| B18 | MINOR | **Fixed** by measuring: the §6 epoch-end stranding log reports expired room per window reset. If it shows material loss, the review in 0.12.3 decides whether to lower `MIN_LANDING`. |

---

## 10. Owner decisions recorded (from the coordinator)

1. Ride to 99% on real readings when no candidate meets the landing runway: **adopted** (drain mode).
2. `MIN_LANDING = 15 min`, as a named constant: **adopted**.
3. Same plan per provider, per-account rates: **adopted**.
4. Flip-back only for use-it-or-lose-it within a short horizon: **6 h**, justified in §3.6.
5. Leave credits for a plan account with ≥ landing runway: **adopted**.
6. A manual pick of a blocked account doesn't hold; 0.10.4 and 0.8.8 are kept: **adopted** (I6).
7. Active poll floor of 60 s near the trigger: **adopted**, bundled with the blind projection (§3.3).
8. `--ceiling` is **kept**. It only sets the session limit in `time_to` and adds no branch.

Remaining open items, for the 0.12.3 review: the default rates (1.0 / 0.25) after two weeks of the new logs,
and `MIN_LANDING` against the stranding log.

---

## 11. Round 2 resolutions (v3)

### 11.1 Replays through the v3 rules

**dev8 98 → 100 at 17:55:46** (real data: 17:52:43 Real 70/98, 17:55:46 Real 76/100, never read 99; all other
accounts lb 100). v3 cadence: 60 s in Draining.

| Time | v3 decision, rule |
|---|---|
| 17:52:43 | Real 98 ≥ trigger → Draining. No verified Ready or drain target: dev4 lb 100 at 17:28:31 is still Blocked at any age (R2-B1/B3). **Stay(draining)**, 0 verification reads (T36). |
| 17:53:43, 17:54:43 | dev8 is the manual pick, so neither an estimate nor the projection moves it (I6); there is also no Ready target. Stay(draining). (v3.1: the projection can never drain, R3-M1.) |
| 17:55:46 | Real 100 → lb-blocked → arm 1. Nothing servable (dev4 lb 100 since 17:28:31; the rest lb 100). No pending reset reads. No Credits. **No Prepare: dev8 is a manual pick** (I6; v1 prepared to dev6 here because its rule only covered held picks). Stay(all_blocked), stuck notice once. dev8 is no longer fetched (R3-B2). |
| 10-26 04:59:59 → 05:00:05 | dev6's window turns reset-pending. NeedReading(dev6), exempt from the storm guard and run before any active fetch. It reads 0/0 reporting the next reset (11-02 05:00), so it is post-reset and servable → **Serve → dev6** (the pick on dev8 doesn't hold, I6). This is T38 / S8b. |

**v0.8.7 blind run** (dev@ Real session 93, trigger 95, a 25-minute 429 streak). The last Real reading is in
band. After `BLIND_AFTER` (120 s), proj = 93 + 2.0 × 1.25 × t passes 95 at t ≈ 0.8 min, so the threshold is met
as soon as blindness starts. A Ready target has been pre-verified since the start of blindness, so **Trigger**
fires at about 2 min, whatever the measured EWMA. With no Ready target: Stay (the projection never drains,
R3-M1). Serve then fires on the first
successful read at ≥ 99.5, which stays valid at any age (R2-B1). This is S4′.

**R2-B2's own scenario** (last Real 90 weekly, idle peak 0, burst 0.47/min). proj = 90 + 0.84 t reaches 95 at
about 6 min, when the real usage is about 92.8, so the move onto a verified Ready target is early but safe. There
is no 10-minute cutoff. This is T27 / S14.

### 11.2 Findings

| # | Resolution |
|---|---|
| **R2-B1** | **Fixed.** The lower-bound rule (§3.3): `lb_w ≥ 99.5` is Blocked until `resets_at_w` at any age. ≥ trigger keeps the active account Draining, and ≥ 99 keeps it drain-eligible. Arm 0 now sends lb-blocked straight to Serve. T25. |
| **R2-B2** | **Fixed.** Blind-in-band projection with no age cap at `max(peak, BLIND_RATE 2.0/0.67, floored)`, sleep excluded. It drives only the Trigger move onto a verified Ready target (v3.1: the Drain clause was removed). T27, T36, S4′, S5′, S14. |
| **R2-B3** | **Fixed.** lb-blocked candidates are Blocked at any age. "Stale-but-maybe-serving" is deleted. Only reset-pending windows and chosen targets older than 120 s are read. Fetch budget §5.7: overnight requests are the same as v1 (0 extra for 02:11–04:58). T26. |
| **R2-M1** | **Fixed.** Serve from a blocked active accepts any target with a post-leave (+60 s) and post-reset reading < 99.5, at any age, even after a failed verify. Trigger keeps the 2×MIN_LANDING rule. T28, T29. |
| **R2-M2** | **Fixed.** `note_active_change` on Auto, Manual and External changes; reload-merge-save under `with_state_lock` (max per key); `BindingShown` kept in memory; `manual_holds` dual-written; the Settings toggle is the primary rollback. T31–T34. |
| **R2-M3** | **Fixed.** Simulator realism knobs (post-leave lag, skipped integers, correlated 429s, sleep, external changes, budget) are on for the gate. S12–S16. The `v1_action` shadow is dropped as an artifact. |
| **R2-M4** | **Fixed.** Per-provider budget (§5.7): ≤ 3 requests/min, ≤ 2 verification reads per cycle, ≤ 20 per hour, a 60 s storm guard after any 429, and no `NeedReading(active)` while the active is in backoff. T35. |
| R2-m1 | **Fixed** (v3.1 form): a reading is post-reset when it reports the next `resets_at`. T30. |
| R2-m2 | **Fixed.** Relative 20% margin; immediate override when the other window is < 15 min to block or ≥ trigger. |
| R2-m3 | **Fixed.** The tooltip says "at current pace", with the engine verdict shown separately. |
| R2-m4 | **Accepted** as a capacity-ordering cost; measured as `latched_pts` in the stranding log. |
| R2-m5 | **Fixed** in the v1 rollback code (§6). |

### 11.3 The 9 open implementation decisions, resolved

| # | Decision |
|---|---|
| 1 | Lower-bound rule: §3.3 `lb_w` = the latest reading; valid until reset-pending; a new epoch on any pct drop or `resets_at` change (v3.1). |
| 2 | "Stale-but-maybe-serving" is removed. The only reads are: reset-pending windows (soonest reset first; storm-guard exempt), an Unknown or Stale active that is not lb-blocked and not in backoff, a pre-verify of the best Ready candidate while blind in band, a not-lb-blocked but not-yet-servable candidate when the active is lb-blocked (§3.4 arm 1), and a chosen target that isn't verified (post-pass). lb-blocked rows are never read. |
| 3 | A `trigger_left`-latched roomy candidate is Draining. It can be a drain or Serve target, never a Trigger or Rebalance target (§3.5). |
| 4 | In arm 1, pending reset reads run first, within the same cycle's re-decide. Failed reads become `unverified` and stop blocking, so Credits and Prepare wait at most one re-decide, never indefinitely. |
| 5 | `NeedReading(active)` counts toward the per-provider 3/min cap but not toward the 2 verification reads per cycle. It is never issued while the active is in backoff or 429'd this cycle (§5.7). |
| 6 | `note_active_change` only on a change of active key (Auto, Manual, External detected via `last_seen_active`); merge under the lock (§3.5). |
| 7 | v2 dual-writes `manual_holds` while v1 exists; v1 ignores `swap_memory` (§3.5). |
| 8 | Single engine owner via `engine.lock` flock; a second process is display-only and reads rates through the same function (§3.5). |
| 9 | v3.1: no skew constant. Reset-pending means the reading's own `resets_at ≤ now`; post-reset means it reports the next reset (§3.3). |


---

## 12. Round 3 resolutions (v3.1)

| # | Resolution |
|---|---|
| **R3-B1** | **Fixed.** Confirmed: `sync_active_from_keychain` returns early on `identity_mismatch`, and `adopted` (324 lines today) is a same-account token refresh. `note_active_change` fires only when the active key changes. External is detected via `last_seen_active` (§3.5). T32 now asserts that adoption changes nothing; S16 too. |
| **R3-B2** | **Fixed.** An lb-blocked active is not fetched until reset-pending, the 12 h cap, or a forced refresh. Reset-pending reads bypass the 60 s storm guard (within the hourly and per-minute caps) and run first (§5.7). T38, S8b. Flip of test 5295 noted in §8.4. |
| **R3-B3** | **Fixed.** `lb_w` is the latest reading's pct. Any pct drop or `resets_at` change starts a new epoch and voids latches (§3.3). Reset-pending is judged by the reading's own `resets_at`, so `RESET_SKEW` is deleted. T37 (claimed reset), T18 rewritten. |
| **R3-M1** | **Fixed.** The projection-driven Drain clause is deleted. A drain exit needs a reading with `lb ≥ 99`. T39. |
| Note 1 | The tier table, §0 and Prepare now say lb-blocked (any age), not "Real". |
| Note 2 | The post-pass is one rule by kind: Serve uses `servable` (R2-M1, no runway minimum); 2×MIN_LANDING applies only to unverified Trigger targets. |
| Note 3 | T26 expects no Prepare (dev6 is the earliest) and 0 reads. |
| Note 4 | T36 and the §11.1 17:55:46 / 05:00 rows account for the dev8 manual pick: no Prepare, then Serve to dev6 via the reset read. |
| Note 5 | v1's Prepare log label is fixed (§6). |

**Consistency pass (v3.1).** Each rule is stated once in §2–§8; §9 and §11 are marked as history:

- **lb-blocked** is defined in §3.3 and used in §0, I1, I6, I7, the tiers, §3.4 and §3.6.
- **verified** (I5) and **servable** (§3.4) each have one definition, and the post-pass references them.
- The only **projection** use is arm 3P Trigger.
- Every **fetch rule** is in §5.7.
- Tests T18, T26, T32, T35 and T36 were rewritten to match. T37–T39 and S8b were added. The gate list is updated.
