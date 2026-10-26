# Adversarial review: autoswap-redesign.md

Reviewed against: usagio 0.12.1 (`src/main.rs`, `src/main_tests.rs`, `src/burn_rate.rs`, `src/usage_log.rs`),
release notes 0.8.3 to 0.12.1, and real data in `~/.config/usagio` (`usagio.log`, `usagio.log.1`,
`history.2026-09/10.ndjson`, `history.jsonl*`, `state.json`). All times are UTC on 2026-10-25 unless stated.

## Verdict

**Sound with fixes. Not shippable as written.** The direction is right: a pure `decide`, judging targets by runway
in both windows, no 429 escalation, and dropping `NO_RETURN` as a guard that could override "serve". The doc's
diagnosis of today checks out against the data (section A). But the pseudo-code in §3.4 has four blockers:

- It thrashes at the drain exit.
- It switches on extrapolation with no bound on reading age.
- It goes blind when the burn-rate estimate is wrong in the low direction.
- It never verifies accounts whose window just reset, so its own I1 "Serve" invariant is not guaranteed.

The rollout's replay stage also has no data to run on.

---

## A. Claims checked against the data (these hold)

| Doc claim | Check |
|---|---|
| 16:23:06 dev4 12/91 → 16:23:37 429 → 16:23:38 urgent swap to dev8 | Confirmed (`usagio.log`, history). |
| 17:10:43 dev8 40/90 → 17:11:14 429 → swap to dev4 16/92, `reason=soonest_reset_in_band` | Confirmed. |
| dev4 crossed 95 at 17:13 and hit 100 at 17:28 | Confirmed (17:13:21 25/95, 17:28:31 47/100). |
| dev4 burn 1.8/0.47 (17:11 to 17:28), dev8 0.71/0.18 (16:25 to 17:10) | Confirmed: 31 session and 8 weekly points in 17 min; 32 and 8 points in 45 min. |
| 17:13 to 17:28 `stayed reason=no_eligible_target` because of NO_RETURN only | Confirmed. dev8 failed `eligible_target` (90 is not < 90), but `forced_target` passed. It was excluded only by the window: left at 17:11:14, window open until 17:31:14. The user switched by hand at 17:30:55. |
| No Claude credits path fired; pq.io `limit_reached: true` | Confirmed (`state.json`). |
| "19 429s on the active between 15:38 and 17:31" | Minor miscount: 17 in `usagio.log` plus 1 at 15:38:00 in `.1`, so 18. The point stands. |
| "~190 `on_credits` lines today" | Roughly right: 54 since 15:38, about 465 in `.1`. |

---

## B. Findings

### B1. BLOCKER: I1 "Serve" ping-pongs between two accounts at the drain exit

**Scenario.** End of a weekly cycle (today's fleet): two usable accounts, both reading **99** weekly. The API
returns integers, so a 99 is a real reading; see dev4 at 17:25:29, 43/99.

- Active A reads 99. `est = 99 + 0.47×0.5×1.25 ≈ 99.3`, so `hard_runway = (99 − 99.3)/r < 2 min`. The I1 arm
  fires on "Draining && hard_runway < 2".
- Its pick is `best(tier ∈ {Ready, Draining})`. B reads 99, which is under 99.5, so B is not blocked and counts
  as Draining with `hard_runway = 0`. Result: **Switch to B (Serve), no cooldown.**
- The next cycle runs about 30 s later. B as active has `hard_runway < 2`, so I1 fires again. NeedReading
  re-reads A, A still reads 99, so A is Draining. Result: **Switch back to A.**

This repeats every poll until one of them ticks to 100. At 0.18/min weekly a 99 reading lasts about 5.5 min,
which is about 10 switches. Each switch is a prompt-cache rebuild that burns the last point faster. I2 allows it
because "unless I1 applies". I4's bound does not catch it either: the account being left goes Draining→Draining,
which is not a tier transition.

**The doc contradicts itself.** The §4.1 row at ~17:45 says "dev8 at ~99 is Draining but hard runway < 2 min.
dev4 isn't blocked yet, so Stay." The pseudo-code would switch.

A milder form of the same bug: I1 picks a Draining target with 20 s of room over a Credits target. That costs
two cache rebuilds for 20 s of plan usage.

**Fix.**
- From a *Draining* active, I1 should switch only when the target's `hard_runway` is at least `act.hard_runway`
  plus a margin, with an absolute floor of about 5 min. Otherwise ride the active to really Blocked.
- An account left by Serve is latched "drained" until its reset. It can be a Serve target again only if every
  other account is Blocked and a post-leave reading shows less than `BLOCKED_PCT`.
- Add a sim scenario: two accounts at a real 99, 30 s polls. Assert at most 1 switch.

### B2. BLOCKER: Extrapolation has no age cap, so a phantom "Blocked" leads to no-cooldown switches on no information

The doc defines `est = pct + rate × age × 1.25` with no cap on `age`. Paths where the age gets large without any
usage:

- **Sleep/wake and startup after downtime.** 0.8.8 makes the loop notice wake. The first fetch after wake or
  login often fails because the network is not up yet. Under I8, a skipped fetch "changes nothing except reading
  age". With an 8 h age, any non-zero rate puts `est` far above 100, so the active is Blocked and I1 switches
  with no cooldown. The target's reading is just as old and its verify also fails, but §3.4 allows "unverified"
  targets under Serve. The result is a switch on zero information.
- **Long 429 streaks.** This happened today. Last good dev4 reading: 15:36:59, 4/89. Then eight consecutive 429s
  with backoff up to 1200 s, and the next success at 16:01:08, still 89. §3.2 specifies "EWMA of *positive*
  same-account deltas", and `consumption_deltas` keeps only increases with no Δt. Weekly was flat for 30 min, so
  there were no positive weekly deltas, "not enough data" applies, and `DEFAULT_RATE_WEEKLY = 0.25` is used.
  - `est` crosses 95 at about 15:56. There is no Ready candidate: dev8 read 100/80, session-locked until
    16:19:59. Every other account is at 100 weekly. Result: Stay(draining), and the "all accounts near limit"
    notice fires falsely.
  - At about 16:07 `hard_runway < 2`, so the I1 arm runs and finds no target. Prepare picks
    `best(Blocked && until < act.until)`. `act.until` is undefined for a Draining active, which effectively
    means MAX, so the result is **Prepare: switch to dev8, an account that really is at 100% session for another
    13 min.** The account it leaves is really at 4/89 and fully usable.
- **§5.3(b) makes this routine.** The poller "wakes on `next_check_at` even if no fetch is due" and `decide` acts
  on the projection without even trying to read the active. The model has no NeedReading for the *active*
  account.

**Fix.**
- Cap extrapolation at about `min(age, 10 min)`. Past the cap the active is "unverified-high", not Blocked:
  first try a fresh read of the active, bypassing the floor but respecting the 429 backoff.
- Never let Prepare, or a Serve onto an *unverified* target, fire from an active that is blocked only by
  estimate.
- Define the rate as Δpct/Δt over all intervals, including zeros, so flat data gives rate 0. Use the default only
  when there are fewer than N samples. Do not count time across sleep: use a monotonic-with-sleep gap check.
- Test this as a table row: dev4 4/89 at age 24 min with zero weekly deltas. Expect Stay, no Prepare.

### B3. BLOCKER: A low rate estimate reopens the v0.8.7 blind run that §4.3 says "can't happen"

The estimate covers a fetch gap only when the rate is right. It is least reliable at the start of a burst after
idle time, which is also when the 429s cluster (TIGHT cadence near the trigger).

**Real data.** dev4 sat at 4/89 from 15:36 to 16:20, so the EWMA would go to about 0. Then 16:20:04 4/89 →
16:23:06 12/91, which is +8 session and +2 weekly in 3 min. 17:11:20 16 → 17:13:21 25 session is 4.5/min. Both
are well above any 30-minute EWMA.

In v2's world the user stays on dev4 after 16:23. The 429 pattern of that hour continues: every other poll from
16:03 to 16:21, and 8 in a row from 15:38 to 16:01. §5.3(c) proposes to *honor* the 1200 s backoff for the
active. So `est` stays near 89 to 91 while real weekly climbs at 0.47 to 0.67/min, past 95 by about 16:30 and to
100 by about 16:40. The user is blocked until a fetch succeeds.

v1's escalation did not depend on the rate. v2 removes that backstop and puts nothing in its place. §4.3 assumes
a correct 1.5/min rate.

**Fix.** Keep a rate-independent backstop for the active when it is blind inside the band (last real reading at
or above `trigger − 5` and age > 2 polls):

- Drift with `max(EWMA, DEFAULT, recent peak 5-min rate)`.
- If a Ready target exists, pre-verify it. The loss is asymmetric: an early move onto a Ready account with
  ≥ 15 min of runway costs one cache rebuild, while staying blind costs blocked minutes.
- Do not adopt §5.3(c)/(d), honoring the 1200 s backoff and a 60 s floor, until this is in place.
- Add sim scenarios: rate 0 for 30 min, then a burst of 0.6/min weekly with p(429) = 0.5 near the trigger.
  Assert ≤ 2 blocked minutes.

### B4. BLOCKER: Accounts whose window just reset ("Unknown") are never verified, so I1 is not guaranteed

§3.3 makes an account with a stale-after-reset reading `Unknown`, "never a target until verified". §3.4 emits
NeedReading only for a *chosen* target, and Unknown is never chosen. Nothing in `decide` asks for the reading
that would show an account freed up.

The poller does not reliably fill the gap: `active_account_is_fetched_first_and_its_429_spares_the_inactive_ones`
skips every inactive fetch in a cycle where the active 429s. v1's stale rescue (`stale_swap_candidate`, 0.8.7:
"re-checked when the active account needs to be left, even while it's being rate-limited") exists for exactly
this case. §3.7 claims it maps to NeedReading, but it does not.

**Scenario (real resets from `state.json`).** Active dev8 at 100 weekly until 11-01. dev6 resets at
10-26 04:59:59 and dev7 at 07:59:59. At 05:00 dev6 is Unknown. Prepare picks the earliest Blocked account, which
is **dev7 (blocked 3 more hours)**. While the active is in a 429 streak, dev6 is never read. The user sits on a
blocked account while a fresh one is idle.

The same gap appears on credits: I7's "leave credits once a plan frees up" never fires, because the freed plan is
Unknown, so money keeps being spent.

**Related gaps.**
- `decide` has **no arm for an Unknown *active***. A session reset at 19:30 makes dev4's 17:28 reading
  (47/100) "stale-after-reset". If the whole account becomes Unknown, its weekly 100 is ignored and it falls
  through. Staleness must be per window: a reset window becomes an upper bound, and the other window's Blocked
  status still holds.
- The reset-proximity rule ("runway +∞ if the reset comes first") applies only to candidate ranking. An active
  at 95% session with a session reset 2 min away is still forced off.

**Fix.**
- When the active is Blocked, Draining or Credits, NeedReading must rank Unknown accounts whose reset has passed
  *first*, within `MAX_VERIFY_FETCHES`, even during an active 429.
- Unknown is per window.
- Add an explicit arm for an Unknown active: re-read it, keep the Blocked status of any non-reset window, and
  never Stay silently.

### B5. MAJOR: The replay corpus and the shadow rollout cannot run on the data that exists

- **The `history.YYYY-MM.ndjson` rows have no `active` flag, no `resets_at`, no fetch outcome (429), and no
  Codex rows.** All 16,992 October rows are Claude. Rates "on whichever account was active" and the
  stale-after-reset check cannot be rebuilt from them. `burn_rate::reset_for` already returns `None` for the same
  reason.
- **`usagio.log` keeps about 18 h.** `usagio.log.1` starts at 2026-10-24T21:18. So "replay over the last two
  months with 429 events" (§5.2.4, stage 2) has no input. `history.jsonl*` has `active` but covers only from
  2026-10-24 04:51.
- **Shadow mode logs only the first disagreement per episode.** B1, B2 and B3 are multi-step failures that start
  after v2 would have diverged, so shadow mode can never show them. It is also unspecified whether v2's
  NeedReading fetches run in shadow. If they do, they add requests and 429s. If they don't, v2 decides on data it
  would never accept live.

**Fix.**
- Ship a 0.12.x logging patch first. Record per sample: `active`, `session_reset`, `weekly_reset`,
  `fetched_at`, and the fetch outcome (ok/429/err). Add a separate, longer-retained `decisions.ndjson` with one
  line per decision and switch events, including manual switches.
- Let it collect for at least 2 weeks, then build the replay as a simulator driven by those rows.
- Gate the switch to v2 on simulator scenarios (B1 to B4), not on shadow lines.

### B6. MAJOR: The §4 replays are not replays; corrected, they show the boundary is fragile

- **§4.1 uses rates from v1's world.** It applies 0.47/min, which was measured on dev4 *after v1's 17:11 move*,
  to v2's path. On v2's path the user stays on dev8, at about 0.18 to 0.21/min weekly. At that rate dev4 at 92
  has 3/0.18 ≈ 16.7 min, which is **Ready**, and at 0.21 it is Draining. The decision flips on a 0.03 pts/min
  EWMA wobble.
- The 16:40 / 17:21 / 17:30 / 17:45 times are hand-placed, and the 17:45 row contradicts the pseudo-code (B1).
  "0 blocked minutes, ≤ 4 swaps" is unproven.
- **§4.2 does not match history.** On 2026-10-01, between 0.8.7 at 04:03 and 0.8.8 at 18:11, dev@ was at 82 to
  85 weekly from 07:25 to 09:47.
  - At that time dev4 was 39 to 44 weekly and **58 to 75 session**. At 09:42 dev4 was 75/44. With trigger 85,
    its session runway is about 10/0.65 to 1.0 ≈ 10 to 15 min, so it is not "Ready, ~2 h runway".
  - The interleaving also involved dev3 and the matthew accounts, not just two accounts.
- **"No flip" comes from the wrong rule.** With the measured Oct-1 weekly rate (05:12 26 → 05:57 33, ≈ 0.16/min),
  dev@ at 82 has (85−82)/0.16 ≈ 19 min, so it *is* Ready. The flip-back is prevented only because dev@'s reset
  (about 10-02 18:00) was more than 24 h away, not by `MIN_LANDING` as the doc says.

**Fix.** Re-do §4 with a sim driven by real samples and the rates v2 would itself observe. State which rule
causes each outcome.

### B7. MAJOR: Hysteresis disappears when the user is idle

Ready is measured as "≥ 15 min of runway at the current rate", and §3.2 says idle pulls the EWMA toward 0. At a
rate near 0, `soft_runway` goes to ∞, so an account at 94.9/95 is "Ready".

Overnight, Rebalance, once its reset is within 24 h, or the first forced move after idle can land on an account
with under 1 point of room. When work resumes, it is forced off within a minute. That is the "flip into the band"
behavior that §3.7 says `MIN_LANDING` removes.

Rate = 0 also gives `0/0 = NaN` for an account sitting exactly at its limit. That breaks `min_by` ordering, or
panics if the code uses `partial_cmp().unwrap()`.

**Fix.**
- For *candidate* runway, use `max(EWMA, DEFAULT_RATE)`, and require a points floor as well (for example, at
  least 3 points under the limit in each window).
- Guard division: rate ≤ ε means runway = ∞ only when `est < limit`.

### B8. MAJOR: The I4 churn bound is not proven; the estimate is non-monotone

I4 assumes forced switches happen only on monotone tier transitions. But `est` drops when a fresh reading replaces
drift: for example, 94 + drift crosses 95, then the next read is 94.

- **With a Ready target:** I3 forces a switch on the transient crossing. That is acceptable, since it is what
  drift is for.
- **Without one:** the active flaps Ready↔Draining. The §3.7 stuck notice re-arms on "any tier change", so it
  notifies on every flap. This is real: from 16:03 to 16:21, 429s and successes alternated at 89%.
- **Phantom drift plus a per-account 429 streak** (dev4 had 8 consecutive while verify fetches on *other*
  accounts succeeded, as at 16:23:38 and 17:11:14) can produce A→B→A. A crosses by phantom drift, so the switch
  goes to B. B crosses by phantom drift. A has a fresh real reading 4 points under the trigger with
  16 min at 0.25/min, so it is Ready, and the switch goes back to A.
- I1 exits (B1) are not tier transitions at all.

**Fix.**
- Latch per epoch: once an account is left by Trigger, it is not a Trigger target again until a reading shows it
  ≥ X points under the trigger.
- Re-arm the stuck notice only on a switch or a *real-reading* tier change.
- Make I4 a proptest that includes 429 masks *per account*.

### B9. MAJOR: Manual pick: estimates move picks, I6 contradicts `decide`, and the unsettled state is undefined

- **0.10.4 dropped without saying so.** 0.10.4's requirement was "a manual pick only leaves at a *real reading*
  at the trigger". The doc says that special case "disappears with D1". It does not: long blind gaps (B2) let
  the *estimate* move a pick on no new information, which is the same class of bug.
- **Holds end earlier than today.** `hold_lets_go_exactly_at_99_5` is kept in §5.1. But in §3.4 the I1 arm runs
  before the held check, and it fires on `Draining && hard_runway < 2`. So a hold ends at an *estimated* 98 to
  99, not at a real 99.5.
- **I6 vs `decide`.** I6 says "forced only when the pick's tier gets worse than `settled_tier`". The I1 arm
  ignores `held`, so a pick settled at Draining leaves while still Draining.
- **No definition while unsettled.** `ManualPick.settled_tier: Option<Tier>` is `None` until the first fresh
  reading, which 429s can delay for minutes. The `held < Draining or no pick` branch would treat None as "no
  pick". That moves the user straight off a pick of a 96% account, which brings back 0.8.8's "undone within a
  second". v1 avoids this because `pick_holds` decides from the cached reading at switch time.

**Fix.**
- While unsettled, use the tier of the switch-time reading.
- A pick leaves only on a real reading, or on an estimate whose age is under the cap from B2.
- Put the held check inside the I1 arm for Draining actives: leave a held Draining pick only when it is really
  Blocked.

### B10. MAJOR: The Evidence rule (I5) is weaker under Serve; candidates ignore usage up to `left_at`

- §3.4 lets an *unverified* target through when the reason is Serve. Because the trigger is
  `fetched_at < max(left_at, last_reset)`, this covers **pre-leave and pre-reset** readings. 0.8.7 never trusts
  those, even for urgent moves: `fresh_since_left` and the reset filter sit in front of the unverified-headroom
  fallback.
- Candidates are assessed with `drift = false` from their last reading. That ignores everything used between
  that reading and `left_at`. If we left A after a 24-minute blind period (15:37 to 16:01), A's candidate row
  still says 4/89, which hides a possibly blocked account.
- **Combined:** a Serve move lands on a blocked account, its estimate marks it blocked, Serve picks the other
  blocked account, and so on. There is no cooldown on either move.

**Fix.**
- Under Serve, allow unverified targets only when the reading is post-leave and post-reset (the 0.8.7 rule).
- For an account we left, carry drift forward to `left_at`: `est = pct + rate × (left_at − fetched_at)`.

### B11. MAJOR: The snapshot lacks inputs the design assumes

- **`left_at`, `last_switch_at`, and any per-epoch latch live in the in-memory `SwapGuard`.** The daemon
  restarted today at 17:31:26, right after the manual switch. With `NO_RETURN` deleted, Evidence and the I4 bound
  depend on state that disappears on restart. Persist them in `state.json`, next to `manual_picks` /
  `manual_pick_at`.
- **Rates.** `usage_log` has no active flag (see B5), so rates exist only in memory and start from defaults after
  every restart. Codex is not in `usage_log` at all, so Codex always runs on Claude-tuned defaults.
- **Windows and resets are optional.**
  - Codex has `session_pct: None`.
  - Every 100%-weekly Claude row has `session_reset: None`, and dev8 had `plan: None`.
  - `Window` must be `Option` with defined semantics. `blocked_until` with an unknown reset must not be
    treated as MAX for the *active* (see B12).
- **Missing fields.**
  - "Unverified" (a failed NeedReading) has to be a field in `AccountIn` for `decide` to stay pure.
  - Credits ranking "normalized to the provider's unit" is not possible: `Money` has unit and exponent (USD/2
    vs credits/10), and there is no exchange rate. Rank within a unit, or by `limit_reached` and count only.
- **Existing burn-rate code is ignored.** `burn_rate::estimate` already fits per-account weighted regressions
  with a confidence floor, and the poll floor already projects with it (`floor_with_projection`,
  `loop_interval_is_remembered_for_the_projection_horizon`). It is also reset-unaware, since the weekly fit spans
  resets. Either reuse it and fix it, or retire it. Do not run two different rate models.

### B12. MAJOR: The Prepare arm is looser than v1, and v1's rules are flipped without saying so

v1 `earliest_blocked_target` returns None in two cases:

- **Any switchable row lacks data.** "Unknown usage/reset data is not proof of an all-blocked state."
- **The active's own blocked-until is unknown.** `active_until … .max()?`.

§3.4's Prepare arm keeps neither rule. It prepares while Unknown rows exist (B4) and treats an active with
`until = None` as MAX, so anything qualifies. It can also fire from an active that is blocked only by estimate
(B2). This flips `blocked_preparation_requires_known_future_resets_and_valid_login`, which §5.1 lists as
"keep".

**Fix.** Restore both preconditions, and require a real reading of the active at or above `BLOCKED_PCT`.

### B13. MAJOR: Silently lost or contradicted requirements (tests and release notes)

**Tests listed as "keep" that assert the removed behavior.** Both sit under §5.1 wildcards:

- `provider_swap_cycle_labels_urgent_and_prepared_moves_as_non_proactive` (main_tests.rs:6897) records a 429 at
  93/95 and asserts a swap with `reason=trigger`.
- `watch_cycle_reports_the_claude_429_and_swaps_claude_accounts` (:6973) asserts "the 429 on the active account
  counts as being at the trigger: moved".

Both need explicit flips.

**Not mentioned at all**, so their fate in v2 is undefined:

- **Cadence:** `next_interval_*` (849, 868, 900, 969, 1181, 1201), `floor_with_projection_*` (1012),
  `per_account_fetch_floor_matches_the_cadence_ramp` (1161), `cadence_keeps_one_providers_429_from_slowing_another`
  (3193), `cadence_restarts_backoff_tight_after_a_switch` (3209, 0.8.5/0.8.7),
  `note_polled_active_reports_only_a_change_of_active_account` (4304).
- **Fetch budget:** inactive backoff (3035, 3056, 3111), active-first and 429 sparing (5199), locked skip (5272),
  active polled when locked (5295), active polled when `needs_relogin` (0.8.7).
- **Codex polling:** 5540, 5548, 5566, 5576.
- **Wake handling:** the 0.8.8 sleep-resume wake.
- **Logging and notices:** stuck-notice re-arm semantics (5843 clears it when the active is healthy or held, not
  only on a switch); a flip-back that is not announced (5749); the `action=blocked reason=cooldown` log (5891).
- **Switching:** a switch refreshes the new active immediately (0.8.5).
- **Per-provider isolation of guard and backoff (0.8.8):** the design has no per-provider guard struct.

**Dropped without a reason in §3.7:**

- 0.8.8's 5-point real-gain rule for the fallback, so two near-full accounts can't trade places for a point. B1
  is what happens without it.
- 0.8.7's 10-point unverified-headroom rule, replaced by a runway rule with a Serve exemption (B10).

**Fix.** Add §3.7 rows for each item above, with keep/drop and a reason. B1 is the argument for keeping a
"real gain" rule.

### B14. MINOR: I2 contradicts Rebalance

Rebalance lands on a Ready account with ≥ 15 min of runway while the active may have hours left, which is a
shorter runway. Either exempt Rebalance in I2 or the I2 proptest fails by construction.

### B15. MINOR: Readings taken right after leaving still lag

After leaving dev4 at 16:23:38 (last read 12/91 at 16:23:06), its next reading at 16:33:48 was 16/92. After
leaving dev8 at 17:11:14 (40/90), it later read 41/90. A "post-leave" reading taken seconds after a switch can
still miss in-flight usage. Require `fetched_at ≥ left_at + 60 s` for Evidence.

### B16. MINOR: Clock skew

`fetched_at` is the local clock and `resets_at` is the server's. If the local clock runs slow, a post-reset
reading looks pre-reset, so the account stays Unknown and keeps being re-read until the skew passes. If it runs
fast, a pre-reset 100 is trusted after the reset. This is the same as v1, but B4 makes Unknown costlier. Compare
with a small tolerance, or use the `Date` header of the usage response.

### B17. MINOR: `expiring_reset` is a hard step at 24 h

The horizon makes ranking jump at exactly now + 24 h. `no_flip_back_into_the_429_escalation_band` uses
`now + 1 day`, which lands on the boundary and can make the test flaky. Use a `<` comparison with a margin, or
fix `now` in the test.

### B18. MINOR: The cost of the `MIN_LANDING` threshold is understated

Q2 says the accepted cost of the threshold is "< 15 min of work per account" expiring unused. At today's
0.47/min that is up to 7 weekly points per account at each reset. Measure it over a cycle before accepting it.

---

## C. Answers to the six questions

1. **Replays.** The diagnosis of today holds. The §4.1 decisions depend on rates taken from v1's world and
   contradict the pseudo-code at 17:45. §4.2 does not match history, and its "no flip" outcome comes from the
   24 h horizon, not `MIN_LANDING` (B6). The sharpest counter-scenarios from real data are the 15:37–16:01 429
   streak, which gives a phantom Prepare onto session-locked dev8 (B2), and the 16:20 burst after idle (B3).
2. **Burn rate.**
   - No samples: defaults.
   - Idle: hysteresis collapses (B7).
   - Burst: blind, under-estimate (B3).
   - Long 429 streak or sleep: unbounded over-estimate (B2).
   - Positive-only deltas: inflated rate (B2).
   - Clock skew: B16.
   - The weekly window moving slower than the session window is handled correctly per window.
   - Can an estimate make a healthy account "blocked"? Yes (B2). Can it hide a blocked one? Yes, for the active
     (B3) and for candidates up to `left_at` (B10).
3. **Runway and hysteresis.** Oscillation at the drain exit (B1) and with phantom drift (B8). Stranding with
   Unknown targets (B4) and with an Unknown active (B4). The I1 invariant is not guaranteed for Unknown targets.
   For unverified targets, the danger is that it lets *too much* through (B10).
4. **Requirements.** See B13. Two "keep" tests assert the removed 429 swap. About 25 cadence, fetch-budget,
   Codex and notification requirements are unaddressed. The 5-point gain and 10-point unverified rules are
   dropped without a reason.
5. **Interactions.**
   - Credits: stuck on credits behind an Unknown plan account (B4); Draining-over-Credits churn (B1); unit
     normalization (B11).
   - Manual: B9. All-blocked: B12.
   - Codex: no usage_log rows, `session: None` (B11).
   - `needs_relogin`: the active's fetch errors feed B2.
   - `no_subscription` and env override are fine as written.
6. **Implementability.**
   - Missing: persistent `left_at` / last switch, rates with an active flag, Codex samples, reset history, an
     "unverified" flag, optional windows (B11).
   - Shadow and replay cannot validate the risky multi-step behavior (B5).
   - Recommended order:
     1. Logging patch.
     2. Simulator with B1–B4 scenarios.
     3. v2 behind a flag.
     4. Shadow as a sanity check only.

---

# Round 2: review of design v2 (0.12.2, v2 on by default, rollback switch, binding-window display)

New live data used here. The daemon kept running overnight, and `usagio.log` / `history.2026-10.ndjson` now run to
2026-10-26 05:09.

- **17:52:43** dev8 read 70/98. **17:55:46** it read 76/100. The weekly reading went 98→100 in one 3-minute poll
  and **never showed 99**.
- **17:55:46** v1 prepared a switch to dev6 (blocked until 04:59:59). The log line is mislabelled
  `reason=soonest_reset_in_band` (a v1 labelling bug).
- **04:59:59** dev6's reset passed. The reset wake read it at **05:00:05 as 0/0**, 6 s after the reset. It read
  9/4 at 05:03:06.

## Verdict: **GO with listed fixes**

v2 closes B1, B2, B6–B10, B12 and B13 as designed. Three new blockers share one root cause: v2 treats every reading
as aging, so "blocked" stops counting as blocked after 180 s or 10 min. That is backwards. Within one reset window,
usage only rises, so a reading is a **lower bound** until that window resets:

- A reading that says the account is **full** stays true until its window resets.
- A reading that says the account **has room** goes out of date.

All three blockers come from the evidence tiers ignoring that difference. Each fix is a few lines in `assess.rs`
plus one table test, and none of them changes the architecture. Do not ship 0.12.2 without R2-B1 to R2-B3. With
the shadow gate removed, the simulator is the only gate.

## 1. Status of round-1 findings (checked against code and data, not §9)

| # | Status | Notes |
|---|---|---|
| B1 drain ping-pong | **Closed** | Serve needs a Real ≥ 99.5 active. API readings are integers, so a 99 never counts as blocked. A drain move needs ≥ 5 min of target runway at the floored rate. The `drained` latch stops a return. There is no path back to a Real-100 account. Note: the 17:52→17:55 jump shows Real-99 drain moves will be rare at 60 s+ polling. Most end-of-epoch exits will be Serve after ≤ 1 poll blocked, which §7.1 already bounds. |
| B2 unbounded extrapolation | **Closed** | 10-minute cap. A Stale active gets `NeedReading(active)`. Prepare and unverified Serve need a Real-blocked active. Zero deltas count, gaps > 15 min are skipped. Re-running 15:36:59→16:01:08: dev4 4/89 sits below the band (90), the rate is 0, then the reading goes Stale and v2 issues NeedReading. No Prepare. T6 matches. Minor: `NeedReading(act)` in a cycle where the active just 429'd is a second request to the same throttled token. Make it obey the active's backoff. |
| B3 blind burst | **Partially closed**, see R2-B2 | The backstop's floor rate (0.25 weekly) sits below the observed bursts (0.42 to 0.67/min). The 10-minute cap ends the estimate before it crosses the trigger. |
| B4 reset-pending never read | **Partially closed**, see R2-B3 | Reset-pending accounts are read first. The Unknown-active arm exists. But a stale reading of a *locked* account is no longer Blocked, which brings a new failure (R2-B3). Also `RESET_SKEW = 120 s` throws away genuine post-reset reads, e.g. dev6 0/0 at 05:00:05. See R2-m1. |
| B5 replay data | **Partially closed** | The §6 logging is right. Owner decision: no shadow gate. The remaining risk is R2-M3: the simulator checks the design against its own assumptions. |
| B6 replays | **Closed** | §7.2 now matches the 2026-10-01 samples (dev4 55–75 session). §7.1 uses rates from v2's own path. |
| B7 idle hysteresis | **Closed** | Floored rate, ε guard, `total_cmp`. |
| B8 non-monotone churn | **Closed** | `trigger_left` latch, re-entry needs a Real reading < limit − 1, notice re-arms only on a Real tier change. |
| B9 manual | **Closed** in rules | Inherits R2-B1, since a held pick needs a "Real ≥ 99.5" reading, which expires after 180 s. |
| B10 evidence under Serve | **Closed**, now **too strict** | See R2-M1. |
| B11 snapshot inputs | **Partially closed** | See R2-M2: manual and external switches don't record `left_at`; concurrent writes are unspecified; `binding_display` is described as both in-memory and persisted. |
| B12 Prepare | **Closed** in rules | Inherits R2-B3: a Stale locked target is not Blocked, so Prepare depends on fresh re-reads. |
| B13 lost requirements | **Closed** | Fates are stated in §5 and §8.4. |

## 2. New findings

### R2-B1. BLOCKER: A real "blocked" reading expires after 180 s, breaking the hard invariant

**Rule.** §3.3 says blocked for the active account requires a `Real` reading, and Real means age ≤ 180 s. After
that the active is `Estimated`. Its estimate is still ≥ the limit, so it falls to **Draining**.

- The Draining arm moves only onto a *verified Ready* target (Trigger), or makes a drain move that again needs a
  *Real* reading of the active.
- Arm 0's "continue as Blocked if a non-pending window is Real-blocked" can never fire in the Stale case, because
  Stale (> 10 min) and Real (≤ 180 s) never overlap.

**Counterexample (real timestamps).**

- 17:28:31 dev4 reads Real 47/100 and Serve fires. dev8's reading (17:22:27) is older than 120 s, so v2 issues
  NeedReading(dev8). Suppose that read 429s, with dev8 at 93 weekly instead of 90, so it is not Ready.
- The unverified fallback needs 2×MIN_LANDING = 30 min of runway at the floored rate. dev8 has 2/0.25 = 8 min,
  so it is rejected.
- 17:30:31: dev4's fetch 429s, and so does dev8's re-read.
- 17:31:31: dev4's 100 reading is 180 s old, so dev4 is "Draining". dev8 is not Ready. A drain move needs a Real
  reading of the active. Result: **Stay(draining)** on an account at a real 100.
- When dev8's next read succeeds, dev8 is verified and can serve, and v2 still stays.

The invariant "never stay on an exhausted account while a verified account can serve" is broken until dev4's own
fetch succeeds. The active fetched 8 consecutive 429s from 15:38 to 16:01 today, so a stretch of about 20 minutes
on an exhausted account is plausible.

**Fix.**
- A reading with `pct_w ≥ BLOCKED_PCT` makes window w Blocked until `resets_at_w`, at any age. Then apply the
  reset-pending check.
- In the same way, a Real ≥ `DRAIN_EXIT_PCT` reading keeps the account drain-eligible until reset.
- Age limits only apply to claims that an account has room.
- Add T25: dev4 100 read 400 s ago, every active fetch 429, dev8 verified 93. Expect Switch to dev8 (Serve).

### R2-B2. BLOCKER: The blind backstop stops after 10 minutes, so v0.8.7's blind run returns for long 429 streaks

The backstop drifts at `max(rate, DEFAULT, peak) × 1.25`, but only within `EST_MAX_AGE = 600 s`. After that the
active is Stale and only gets `NeedReading(active)`. Trigger, the "always safe" move, can no longer fire.

**Scenario.**
- Last Real reading: 90 weekly (inside the band). The user had been idle, so peak ≈ 0, and then starts a burst at
  0.47/min, the rate observed 17:11–17:28.
- The backstop drifts at 0.25 × 1.25 = 0.31/min. At 10 minutes the estimate is 93.1, below the trigger, and the
  reading goes Stale.
- Meanwhile the real usage is 94.7 at 10 min and **100 at about 21 min**.
- A 23-minute active-429 streak has a precedent today (15:38–16:01). S4 tests only 6 minutes.

**Fix.**
- When the active is Stale and its last Real reading was inside the band, or its estimate at the cap is at or
  above `trigger − BLIND_BAND`, treat it as Draining-urgent. Allow the safe Trigger move onto a verified Ready
  target.
- Raise the backstop's floor to the observed burst rate, about 0.5 weekly / 2.0 session, rather than the idle
  default.
- Extend S4/S5 to a 25-minute streak.

### R2-B3. BLOCKER: Stale locked accounts are not Blocked, which breaks Prepare/Credits and amplifies 429s

**Rule.** §3.3: `blocked_w` needs "the reading is not Stale". The poller never re-reads locked inactive accounts
(`refresh_skip_locked`, kept per §5). So all 8 locked accounts today, read hours ago, become **Draining**
candidates: not blocked, not Ready.

**Consequences.**
- **Fetch amplification.** Arm 1 treats them as "stale-but-maybe-serving" and issues NeedReading, 2 per cycle,
  each account every ≥ 120 s, all night. That is roughly 1 extra request per minute against the endpoint whose
  429s this release is trying to reduce, and it undoes the locked-skip rate-limit guard. Overnight today v1 made
  **zero** such requests: 02:11 to 04:58 shows only `refresh_skip_locked`.
- **Prepare** needs every row Blocked with a known `blocked_until`. **Credits** need no plan account still
  awaiting verification. Both therefore need all 9 other accounts to be non-Stale (read within 10 min) at the
  same moment.
  - At 2 reads per cycle, 9 accounts take 5 cycles. With the 120 s band cap that is 10 min, so the first read is
    Stale again before the last one lands.
  - Credits and Prepare may then never fire. A user with credits enabled stays on a blocked account. §7.1's
    "Prepare → dev6 at 17:51" works only because dev6 happened to be read at 17:43:38.
- **Unspecified.** "Stale-but-maybe-serving" is never defined in the spec.

**Fix.** The same lower-bound rule as R2-B1: a candidate with `pct_w ≥ BLOCKED_PCT` and `resets_at_w > now` is
`Blocked{until}` at any age. Only reset-pending windows trigger NeedReading. Add T26: overnight all-blocked, 9
locked rows read 6 h ago. Expect 0 NeedReading, Prepare allowed.

### R2-M1. MAJOR: Serve rejects targets that can only improve on staying put

**Rule.** The unverified fallback needs 2×MIN_LANDING (30 min at the floored rate) even when the active account is
Real-blocked. dev8 at 41/90 read 17:22:27 (post-leave) has 20 min, so if its 17:28 verify 429s the user stays on
dev4 at 100.

Staying on a Real-blocked account has zero value. Landing on a target with a post-leave, post-reset reading under
99.5 is either better or, at worst, the same: if the target is also full, nothing is lost.

**Fix.** For Serve from a Real-blocked active, accept any target that is unverified but has a post-leave
(+60 s), post-reset reading under 99.5, regardless of its age. Keep 2×MIN_LANDING for Trigger.

### R2-M2. MAJOR: Persisted swap memory has gaps: manual and external switches, concurrent writes, rollback

- **`left_at` and `last_switch_at` aren't recorded for every active change.** The shell records them only after
  `switch_if_still_active`. The spec doesn't cover menu or `usagio switch` changes, or
  `sync_active_from_keychain outcome=adopted`, which fires when Claude Code itself logs into another account.
  Without `left_at`, I5's post-leave check and the drift-to-`left_at` rule don't apply to an account the user
  left by hand. Its pre-leave reading, taken while it was burning, counts as current.
  **Fix:** a single `memory.note_active_change(from, to, now, kind)` called from every path that changes `active`.
- **Concurrent writers.** The daemon decides, fetches outside the state lock (NeedReading), then switches.
  `switch_if_still_active` compare-and-sets only `active`. If the memory update is written from the pre-fetch
  copy, it overwrites a manual pick or `left_at` that the CLI wrote in the meantime.
  **Fix:** memory changes are reload-merge-save under `with_state_lock` (merge the maps key by key; take the max
  timestamp per key).
- **`binding_display`** is listed in `SwapMemory` (§3.5) but described as "kept in memory" (§4). Keep it in
  memory. Persisting it adds a state save, and a backup rotation, on every display flip.
- **Rollback via the setting or env var in the same binary.**
  - v2 stores `manual_pick_tier` and *reads* legacy `manual_holds`, but nothing says it still **writes**
    `manual_holds`. If it doesn't, rolling back to v1 drops every hold, and v1 immediately switches off a held
    96% pick. That is 0.8.8's bug, triggered by the escape hatch.
  - `USAGIO_AUTOSWAP_ENGINE=v1` doesn't reach the menu-bar app started by launchd or a login item. Only
    `launchctl setenv` would pass it through. Document the Settings toggle as the rollback.

  **Fix:** v2 writes both `manual_holds` and `manual_pick_tier` while v1 exists. v1 ignores `swap_memory`. That
  is safe, and stale latches are pruned by epoch when v2 resumes. A 0.12.1 downgrade drops unknown fields on save,
  which is harmless.

### R2-M3. MAJOR: With no shadow gate, the simulator only checks the design against its own assumptions

The §8.2 simulator models these assumptions:

- only the active account accrues usage;
- the endpoint returns integers;
- each account has its own independent 429s;
- there is no lag after a switch.

Real data breaks each one:

- **Post-leave lag.** dev4 went +4 session / +1 weekly after the 16:23:38 leave.
- **Skipped values.** dev8 jumped 98→100 at 17:55:46.
- **Correlated 429s.** v1's own "active 429 ⇒ skip inactive" rule exists because 429s were correlated across
  accounts. The "limit is per token" claim in §5.6 rests on two data points (16:23:38, 17:11:14).
- **Sleep and external events.** Wake gaps, network down right after wake, keychain adoption by Claude Code.

If the simulator leaves these out, the gate passes by construction.

**Fix.** Add them to the simulator:
- post-leave lag (+1 to 4 points within 60 s);
- integer readings sampled at the poll time, so values can be skipped;
- a correlated-429 mode (p(429) on every token at once);
- sleep gaps;
- an external active change.

S1–S11 must pass in correlated mode too.

The §6 `v1_action` comparison is an artifact. v1's `evaluate_swap` reads a `SwapGuard` and a 429 tracker that v2
doesn't maintain, so every no-return or escalation case will show a spurious disagreement. Label it as such or
drop it.

### R2-M4. MAJOR: Under a 429 storm, the reset-pending reads and §5 cadence spend more requests on the throttled token

- §5.2 caps the active's 429 backoff at 120 s in the band. On top of that come up to 2 verification reads per
  cycle plus `NeedReading(active)`.
- In the 15:38–16:01 streak, v1 made 8 active attempts. v2 would make about 12 active attempts plus NeedReading,
  if the limit is per token.
- If the limit is per IP or per client, every extra read prolongs the streak, which is the self-inflicted 429
  pattern this release targets (owner Q7).

**Fix.**
- A per-provider request budget: for example ≤ 3 usage requests per 60 s across active + verification reads.
- Verification reads skipped when the previous verification read on *any* token 429'd within 60 s.
- Log `outcome=429` per token so the per-token claim can be confirmed from the §6 data within days.

## 3. Display (binding window)

- **R2-m2 (MINOR).** The absolute margin of `max(10 min, 20%)` keeps the wrong window on screen exactly at the
  end of the week. Example: the tray shows session at 70% (15 min to block at 2/min) while weekly is at 96
  (8.5 min at 0.47). The gap is 6.5 min, which is under the 10-minute margin, so the tray says `70%` until weekly
  blocks. That repeats the 17:13–17:25 defect the feature is meant to fix.
  **Fix:** override the hysteresis whenever the other window has `time_to_block < 15 min` or `pct ≥ trigger`, and
  use a relative margin (20%) only.
- **R2-m3 (MINOR).** The display uses each account's own rate, while the engine uses the floored rate. The tooltip
  "blocks in ~Nm" will disagree with what the engine does (for example "blocks in 40m" on an account the engine
  calls Draining). Show both, or label the display figure "at current pace".

## 4. Gaps in the implementation plan (decisions still open)

1. The lower-bound rule from R2-B1/B3. Without it, §3.3 is self-contradictory: Arm 0's "continue as Blocked" can
   never run.
2. "Stale-but-maybe-serving" in Arm 1 is undefined.
3. The tier of a candidate that is roomy but latched by `trigger_left`. It is implicitly Draining, so it can still
   be a drain or Serve target. Say so.
4. The ordering within Arm 1 when both NeedReading and Credits apply: does a pending NeedReading delay Credits by
   one cycle or indefinitely? This interacts with R2-B3.
5. Whether `NeedReading(active)` counts against `MAX_VERIFY_FETCHES`, and whether it respects the active's backoff.
6. `note_active_change` for manual and external switches, and the memory merge under the state lock (R2-M2).
7. `manual_holds` dual-write for the rollback (R2-M2).
8. Which process computes rates for the tray if the menu bar and the poller ever differ. Today they share the
   app process. `usagio watch` running alongside the app would mean two engines persisting the same memory.
   Specify single-writer (the existing lock) or refuse to run both.
9. `RESET_SKEW` vs the reset wake (R2-m1).

## 5. Minor

- **R2-m1.** `RESET_SKEW = 120 s` classes dev6's genuine post-reset read at 05:00:05 (0/0) as reset-pending. That
  forces a second read, and when the active is blocked it adds up to 2 min blocked.
  **Fix:** count a reading as post-reset if `fetched_at ≥ resets_at + skew` **or** its pct dropped below the
  pre-reset reading.
- **R2-m4.** The account left by a phantom-estimate Trigger, really at 91, stays latched until its weekly reset,
  possibly days away. It is still usable through drain or Serve, so this is a capacity-ordering cost only. It
  should show in the §6 stranding log.
- **R2-m5.** Fix v1's mislabelled Prepare log line (`reason=soonest_reset_in_band` at 17:55:46) in the v1 code
  kept for rollback, so the §6 decision logs from the two engines can be compared.

## 6. Can the hard invariant be broken?

**Yes, as written**, in three ways:

- **R2-B1.** A Real-blocked reading ages out. Combined with 429s on the active and one failed verify, the engine
  stays on a 100% account while a verified account can serve.
- **R2-B3.** At large fleet size and slow cadence, Credits and Prepare never fire.
- **R2-M1.** A target that would serve is rejected only because one verification read failed.

With the lower-bound rule and R2-M1's Serve relaxation, I could not construct another counterexample. Serve then
fires on the first cycle with any Real ≥ 99.5 reading since the window's reset, onto any verified or
post-leave/post-reset target under 99.5. Accounts whose reset has passed are read first, within 2 reads per cycle.
The one remaining exposure is a cycle in which every read fails. That is the information-theoretic floor and is
correctly accepted.

---

# Round 3: review of design v3 (narrow scope: R2-B1..B3, R2-M1..M4, §11.3, regressions)

## Verdict: **NO-GO as written; GO once three spec lines change**

The monotone-usage principle closes R2-B1 and R2-B3. The rest of v3's §11 changes are sound. But two of the new
mechanisms, plus the wording of the lower-bound rule, each add a fresh way to strand the user or undo their
choice. All three are checked against code and live data, and each fix is one line. No other change to the
architecture is needed.

## 1. Status of the Round 2 items

| # | Status | Check |
|---|---|---|
| R2-B1 | **Closed**, subject to R3-B3 | T25 re-run: dev4 lb 100 (17:28:31) is still Blocked at 400 s. Arm 0 hands it to arm 1, and dev8 (post-leave 41/90) is servable. **Switch dev8 (Serve)** with no reads needed. |
| R2-B2 | **Closed** for Trigger; see R3-M1 for the projection-driven Drain | Re-run (last Real 90, burst 0.47/min, 25-minute streak): proj = 90 + 0.84t reaches 95 at about 6 min, when the real usage is about 92.8. Trigger onto a verified Ready target fires, and the 10-minute cutoff is gone. A single 429 at 91 does not move anyone: blindness needs more than 120 s, and the next successful read clears it. That keeps D1 fixed. |
| R2-B3 | **Closed** | lb-blocked rows count as Blocked at any age, and they are never read. The 02:11–04:58 overnight log, where only `refresh_skip_locked` appears, maps to 0 extra requests. |
| R2-M1 | **Closed in arm 1**; the post-pass contradicts it (build note 2) | T28: dev8 41/90 was read post-leave, and its failed verify still allows Serve. |
| R2-M2 | **Not closed**: the External hook is wired to the wrong event | See R3-B1. Merge-under-lock, dual-writing `manual_holds`, the in-memory `BindingShown`, Settings as the primary rollback, and the `engine.lock` single owner are all fine. |
| R2-M3 | **Closed** | The realism knobs match the observed behavior. |
| R2-M4 | **Closed** for the budget numbers; introduces R3-B2 | ≤ 3 requests/min, ≤ 20 verification reads per hour, and 0 extra reads overnight check out against the 15:38–16:01 and 02:11–04:58 logs. |

## 2. New blockers in v3

### R3-B1. BLOCKER: The "External" active change fires on every poll, wiping manual picks and the cooldown

**What the design says.** §3.5 and T32 call `note_active_change(External)` on `sync_active_from_keychain`
outcome `adopted`. That call clears the manual pick, sets `left_at[from]`, and resets the cooldown.

**What the code does.** In the code, `adopted` means "re-adopt Claude Code's rotated token for the **same**
active account". `src/main.rs` `sync_active_from_keychain` *returns early* on `identity_mismatch`, so `adopted`
never changes `active`. It fires every cycle:

- 324 `outcome=adopted` lines in the current `usagio.log`.
- dev6 alone has 227 of them, at 04:58:27, 05:00:04, 05:03:05 and so on.

`refresh_provider_active_account`'s `active_refresh_adopted` (Codex) is the same kind of event: a token rotation.

**Consequences as written.**
- The manual pick is cleared on every poll. A pick of a 96% account loses its hold within one cycle, and Trigger
  moves the user off it. That is 0.8.8's "manual switch undone" bug. When the user picks it again, the user and
  the engine fight.
- `last_switch_at` is set to now on every poll, so the cooldown never runs out. Prepare and Rebalance never fire.
- `left_at[active]` is refreshed constantly.

**Fix.**
- Call `note_active_change(External)` only when the identity actually changes: the `identity_mismatch` path, once
  it learns to adopt the other account. Never call it on token adoption.
- Today no code path changes `active` because of an external login. Either add one, or drop External and S16 from
  0.12.2.
- Rewrite T32 as: token adoption changes nothing.

### R3-B2. BLOCKER: The 60 s storm guard blocks the reset-pending read behind the active account's own 429

**The rules involved.**
- §5.7 allows no verification reads for 60 s after a 429 on *any* token.
- §5 keeps the active-first fetch order, and keeps polling the active account even when it is locked.
- So whenever the active fetch 429s, every NeedReading in that cycle is suppressed. The active is fetched at the
  start of every cycle (every 120 s while 429ing), so the guard can stay up for every cycle.

**Counterexample (from real state).**
- `state.json` has `manual_picks.claude = dev8`, from the user's 17:30:55 switch. I6/§3.6 forbid Prepare away
  from a pick.
- So v3 does **not** prepare to dev6 at 17:55:46. The §11.1 replay and T36 are wrong on this point. The user
  stays on dev8, which is lb-blocked until 11-01.
- At 04:59:59 dev6 resets. Its only route to Serve is a reset-pending read.
- If dev8's fetch 429s at the start of each cycle (8 in a row happened today, 15:38–16:01), dev6 is never read.
  The user is stranded on a blocked account while a fresh one sits idle. That breaks §0's "verified first, even
  while the active account is in a 429 streak", and S8.

**Fix.** Either one closes this. Doing both also saves budget.
- (a) Don't fetch an lb-blocked active account. A full reading is already known. Fetch it only at its own reset,
  or at the 12 h staleness cap.
- (b) Exempt reset-pending reads (≤ 1 per account per reset) and Serve-target reads from the storm guard, and run
  them *before* the active fetch when the active is lb-blocked.

Add sim scenario S8b: S8 with the overnight pick on dev8 and dev8's fetch always 429ing. Assert the user is on
dev6 by 05:05.

### R3-B3. BLOCKER: "Highest pct seen since the last reset" ignores early resets, limit boosts and claimed banked resets

**Rule.** §3.3 defines `lb_w` as the **highest** pct seen since `resets_at` passed. The post-reset drop rule
(R2-m1) applies only once `resets_at` has passed.

**Why that breaks.**
- The code already documents early resets and temporary limit boosts: `main.rs` around line 2904, "weekly reset
  early / a 50%-boost freed it, website 0% but menu 100%". That's why `MAX_LOCKED_STALENESS` exists.
- 0.11.0 lets the user **claim a banked reset**, and alerts them to do it exactly when they're blocked.

**Consequence.** After such a claim, the fresh reading is 0%, but `lb_w` stays at 100 until the *old*
`resets_at`.
- If anything is servable, Serve moves the user off the account they just reset.
- The account then stays "Blocked" as a candidate for up to 7 days.
- So the one-shot reset is wasted.

**Fix.** Set `lb_w` to the pct of the **latest successful reading**, at any age. Within one window usage only
rises, so the latest reading is already the maximum. Any drop in pct, or any change in `resets_at`, starts a new
epoch. This also removes state that has no home: `AccountIn` / `SwapMemory` have no field for a running maximum.
Add T37: dev8 lb 100, then a fresh 0/0 reading with the old `resets_at` still in the future. Expect Ready, not
Blocked.

## 3. Major (not blocking, fix before ship)

### R3-M1. Projection-driven Drain (arm 3P, second clause) is a forced downgrade made on a guess

**Rule.** A worst-case projection (0.67/min weekly × 1.25) can reach 99 while the active account is really at 93.
v3 then Drain-switches to any verified target with 5 min more hard runway *at the floored rate*, for example one
at 97. That is a real downgrade (6 points left vs 2), and I2 does not exempt it.

**Chain under a correlated storm.** The target blocks, and Serve back to A needs a post-leave reading of A, which
R3-B2's storm guard can block.

**Fix.** Delete the projection-Drain clause. Keep the projection-driven Trigger, which only lands on a verified
Ready account with at least 15 min of runway.

## 4. Does §11.3 leave no open decisions? No

Beyond R3-B1 to R3-B3, these contradictions in the text would force the engineer to make design calls. They're
listed under build notes below because each is a mechanical edit.

## 5. Build notes (each trivially fixable in implementation or the doc)

1. **Tier table (§3.3)** still says the active account is `Blocked` only "by a Real reading" and is `Unknown`
   when "Stale". The §0 summary and §3.6 Prepare ("Real-blocked active") say the same. All must say "lb-blocked"
   per the v3 principle. Otherwise an engineer who follows the table rebuilds R2-B1.
2. **The evidence post-pass (end of §3.4)** still requires an unverified Serve target to have 2×MIN_LANDING of
   runway. That contradicts arm 1's R2-M1 rule. Exempt Serve.
3. **T26** expects "Prepare allowed (earliest until)" with dev6 active, but dev6 is itself the earliest (04:59:59).
   The expected result should be no Prepare.
4. **T36 and the §11.1 17:55:46 row** must account for the live manual pick on dev8. With the pick, the result is
   no Prepare, and dev6 is reached by the reset read at about 05:00 (needs R3-B2).
5. Fix v1's `reason=soonest_reset_in_band` label on Prepare moves, as already noted under R2-m5.
