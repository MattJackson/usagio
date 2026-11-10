//! Turning a snapshot into menu rows: titles, trailing usage, submenu info rows.

use super::binding::{binding_span, binding_title, shown_binding};
use super::cross_platform;
use super::model::{
    account_usage_for, blocking_pct, build_snapshot, locked_countdown_for, now_utc, severity_with,
    summary_pcts, weekly_only, window_is_stale_after_reset, AcctView, ProviderSection, Severity,
    Snapshot, WindowView,
};
use super::theme::{theme_from_name, tray_options};
use super::tray::tray_target;
use crate::countdown;
use crate::countdown::{BlockingWindow, DisplayState};
use crate::providers::trait_def::{Credits, ReportedUsage, ResetGrant, UsageShare, UsageWindow};
use crate::providers::SeverityBands;
use chrono::{DateTime, Utc};

/// Exact title of the disabled section row inserted when a provider's env
/// override is active. A named constant so `menu_tree_from_snapshot` and the
/// tests asserting on the visible menu content can't drift out of sync.
const ENV_OVERRIDE_ROW_TITLE: &str = "env override active — swap disabled";

/// A styling directive for one menu row, matched to its native `NSMenuItem` by
/// the plain title string. We build the muda menu with plain titles (so clicks
/// and structure work exactly as before) then style each row. Offsets are
/// **UTF-16 code units** (what `NSRange` used); all our runs are ASCII so char
/// == utf16 in practice, but the helpers stay correct if an email ever isn't.
///
/// As of the 0.6.0 muri migration NO renderer reads the styling fields any
/// more — muri (`muda-compat`) draws its own surface and takes only the plain
/// label (`.plain`, via `menu_label`/`plain_text`). The bold / color / tab-stop
/// / checkmark / icon directives are retained purely as the pure, unit-tested
/// output of `main_row`/`account_header_row` (the tests assert on them), so the
/// whole struct is `allow(dead_code)` on every target — the styler that used to
/// consume them is gone, and muri's compat layer does alignment/coloring now.
#[allow(dead_code)]
pub(super) struct RowStyle {
    /// The exact plain title set on the item; used to find it in the menu.
    pub(super) plain: String,
    /// Bold the whole row (marks the active account instead of a checkmark).
    pub(super) bold: bool,
    /// Whether the row is a section header (bold, disabled, no tab-stop).
    pub(super) section_header: bool,
    /// Colored spans: (utf16 offset, utf16 length, band).
    pub(super) colors: Vec<(usize, usize, Severity)>,
    /// If set, right-align everything after the first `\t` at a tab stop,
    /// battery-menu style. Requires the plain title to contain a `\t`. The
    /// concrete x is NOT stored here — it's a `TabX` *intent*. Vestigial now
    /// that muri's compat layer lays out the two-column `label\tvalue` form
    /// itself (see the struct doc); retained only as tested pure output.
    tab_x_kind: Option<TabX>,
    /// If set, attach the 16px provider icon (looked up by slug in
    /// `crate::icons::png16_for`) to the native menu item via `setImage:`. Only
    /// set on section-header rows so per-provider iconography appears once at
    /// the top of each section. A missing slug (no bundled PNG) is a no-op.
    pub(super) icon_slug: Option<&'static str>,
    /// If true, mark the native menu item as `NSControlStateValueOn` so a
    /// leading checkmark glyph appears — the "✓ Active" affordance for the
    /// active account row without changing the plain title text.
    checkmark: bool,
    /// If set, paint the run from this UTF-16 offset to the end of the row in
    /// `NSColor::secondaryLabelColor` (macOS "grey secondary" — the same tint
    /// disabled menu items use). Used to render "usagio vX.Y.Z" as a subdued
    /// trailing label on the enabled Quit row: right-aligned via `tab_x_kind`,
    /// grey via this field, without disabling the row's click.
    grey_tail_from: Option<usize>,
    /// v0.5.0: paint the WHOLE row in `NSColor::labelColor()` (the normal
    /// enabled-text color — white in dark mode, black in light) even though
    /// the underlying muda item is `enabled: true`. Used for the reset-window
    /// / model-breakdown informational rows inside an account submenu: they
    /// must read as normal text (not the greyed "disabled" look `enabled:
    /// false` produces) while still routing clicks to a no-op (see the
    /// `("noop", _, _)` arm in `handle_click`) since there's nothing to do
    /// when you click "Session resets in 3h".
    disabled_but_white: bool,
    /// Spans (utf16 offset, length) to embolden within the row: the active
    /// account's binding window (auto-swap v2 display, §4). Like the other
    /// styling fields, carried as tested pure output (muri's value spans
    /// carry color only).
    bold_spans: Vec<(usize, usize)>,
}

impl RowStyle {
    /// A row with no special styling beyond its plain title — construction
    /// sites override just the fields they need via struct-update syntax so
    /// adding a new `RowStyle` field never requires touching every call site.
    pub(super) fn plain_row(plain: String) -> RowStyle {
        RowStyle {
            plain,
            bold: false,
            section_header: false,
            colors: Vec::new(),
            tab_x_kind: None,
            icon_slug: None,
            checkmark: false,
            grey_tail_from: None,
            disabled_but_white: false,
            bold_spans: Vec::new(),
        }
    }
}

/// Kinds of right-align tab stop a row can request. Currently there's only
/// one: right-align at the menu's actual content edge. Kept as an enum
/// (rather than the row just carrying `bool`) so a future second alignment
/// scheme (e.g. a submenu-local edge) has somewhere to go without another
/// magic-number field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TabX {
    /// Right-align at a point computed fresh for each rendered snapshot —
    /// `max(label_width + trailing_width)` over every `MenuRight` row, plus
    /// a small safety pad — so the tab stop always clears the widest label
    /// instead of a fixed constant that silently misaligns past its bound.
    /// Vestigial: muri's compat layer now resolves the two-column layout from
    /// the `label\tvalue` title directly, so nothing reads this intent.
    MenuRight,
}

/// Length of a string in UTF-16 code units (the unit `NSRange` counts in).
pub(super) fn u16len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Human-facing label for one window row inside an account submenu. Preserves
/// the v1 "Session"/"Weekly"/"Opus" copy for the Claude windows so a menu that
/// used to read `Session  85%  · resets in 3h` still does after the refactor.
/// Unknown window ids fall through to the provider's own short `label` so a
/// future provider still surfaces something readable.
fn stat_display_label(w: &WindowView) -> &str {
    match w.id.as_str() {
        "session" => "Session",
        "weekly" => "Weekly",
        "opus" => "Opus",
        _ => w.label.as_str(),
    }
}

/// Left column width (chars) the provider name is padded to in the flat main
/// list row, so every account's email starts at the same x regardless of
/// provider-name length ("Claude" vs "Codex").
#[allow(dead_code)] // kept for `main_row`'s tests — see its doc comment.
const PROVIDER_COL: usize = 10;

/// v0.5.0 flat main-list row: `{provider}    {email}\t{n}% / {n}%`, bold +
/// checkmarked if active, high percentages colored per the provider's
/// severity bands. When the account is fully consumed
/// (`countdown::compute_display` → Locked), the `n% / n%` run is swapped
/// for `locked · <countdown>` and colored red — the user is being told *when*
/// the account is next usable, not *how used* it is.
///
/// No longer called by any production render path — `menu_tree_from_snapshot`
/// uses `account_header_row`'s `{provider} ·
/// {email}` block-header format instead of this padded flat-list format (the
/// per-account BLOCK redesign, item 1). Kept `#[allow(dead_code)]` rather
/// than deleted: it's a well-tested pure function capturing the padded
/// single-row format from the v0.5.0/v0.5.1 flat/grouped list, which a future
/// "compact mode" could plausibly reuse, and deleting it would mean deleting
/// or rewriting its whole existing test suite for zero behavior change.
#[allow(dead_code)]
fn main_row(provider_display: &str, a: &AcctView, bands: SeverityBands) -> RowStyle {
    let label = format!("{provider_display:<PROVIDER_COL$}{}", a.display);
    let base = u16len(&label) + 1; // + '\t'
    if let Some((cd, _win)) = locked_countdown_for(a, now_utc()) {
        // UX (v0.5.0): drop the "locked · " prefix — the trailing run is
        // rendered in red (visually implying locked) and the payload is a
        // time-until-reset instead of a percentage (structurally implying
        // locked, since a healthy row shows "n% / n%"). The old
        // "locked · Xh Ym" wording repeated the same fact three ways.
        let trailing = cd.clone();
        let plain = format!("{label}\t{trailing}");
        // A "locked" account is by definition red — no need to consult bands.
        let colors = vec![(base, u16len(&trailing), Severity::Red)];
        return RowStyle {
            bold: a.active,
            colors,
            tab_x_kind: Some(TabX::MenuRight),
            checkmark: a.active,
            ..RowStyle::plain_row(plain)
        };
    }
    let (pa, pb) = summary_pcts(a);
    let sa = pct(pa);
    let sb = pct(pb);
    // v0.5.1: drop the "S "/"W " label prefixes — "47% / 89%" is
    // self-explanatory without them (item raised post-v0.5.0 UX pass).
    let trailing = format!("{sa} / {sb}");
    let plain = format!("{label}\t{trailing}");
    let mut colors = Vec::new();
    let s_off = base;
    if let Some(sev) = severity_with(pa, bands) {
        colors.push((s_off, u16len(&sa), sev));
    }
    let w_off = s_off + u16len(&sa) + u16len(" / ");
    if let Some(sev) = severity_with(pb, bands) {
        colors.push((w_off, u16len(&sb), sev));
    }
    RowStyle {
        bold: a.active,
        colors,
        tab_x_kind: Some(TabX::MenuRight),
        checkmark: a.active,
        ..RowStyle::plain_row(plain)
    }
}

/// Time left on a renewable account's login, when it's inside the renew window.
fn login_expiring_in(a: &AcctView, now: DateTime<Utc>) -> Option<chrono::Duration> {
    if !a.renewable {
        return None;
    }
    let left = a.login_expires_at? - now;
    (left <= chrono::Duration::seconds(crate::login::RENEW_WINDOW_SECS)).then_some(left)
}

/// A login whose expiry has already passed (it can't be renewed in place by
/// a refresh any more; the renew flow is the only way back).
fn login_expired(left: chrono::Duration) -> bool {
    left <= chrono::Duration::zero()
}

/// Coarse "time left" for a login expiry: days, then hours.
fn login_left_label(left: chrono::Duration) -> String {
    if login_expired(left) {
        return "expired".to_string();
    }
    match (left.num_days(), left.num_hours()) {
        (d, _) if d >= 1 => format!("{d}d"),
        (_, h) if h >= 1 => format!("{h}h"),
        _ => "<1h".to_string(),
    }
}

/// The header row of one account's BLOCK — `{provider}
/// · {email}\t{sa} / {sb}` (or `\t<locked countdown>` when the account is
/// fully consumed, same rule `main_row` uses). Provider grouping is no longer
/// a separate visual concept (no section header, no per-provider HR) — each
/// account is its own top-level block, separated from its neighbors by
/// `PredefinedMenuItem::separator()` (see `menu_tree_from_snapshot`), so the provider name
/// has to live INSIDE this row instead of a shared header above a group of
/// rows. Otherwise identical to `main_row`: same locked/colored/bold/
/// checkmark rules, same `TabX::MenuRight` participation (so the block
/// headers and the Quit row all share one right-aligned column) — just a
/// different label format
/// (`main_row`'s padded `{provider:<10}{email}` was designed for a flat list
/// where the provider name was the ONLY thing distinguishing rows at a
/// glance; the block header can afford the more readable `·` separator since
/// it's the sole row establishing "whose account is this" for everything
/// beneath it).
/// The trailing run (after the `\t`) for an account's block header, plus its
/// color spans RELATIVE to the start of that trailing run (the caller adds
/// its own label-length offset). Built directly off
/// `countdown::compute_display` so the three states it can report each get
/// their own trailing shape instead of the old binary
/// "locked countdown OR `sa / sb`" split:
///   * `Locked` on the WEEKLY window → the countdown ALONE, no slash — a
///     locked weekly is the harder wall (session will free up on its own
///     schedule regardless), so showing a session percentage next to it
///     would be a healthy-looking number the user could still stall on for
///     up to a week.
///   * `Locked` on the SESSION window → `<countdown> / <weekly%>` — the
///     weekly percentage remains visible alongside the session countdown.
///     If both are locked, the countdown waits for the later reset.
///   * `StaleAfterReset` → `-% / -%`, a deliberate "don't know yet"
///     placeholder rather than re-rendering the cached (typically ~100%)
///     reading, which would misreport an account that just reset as still
///     maxed out until the next poll refreshes it (item 4).
///   * `Usage` → the existing `sa / sb`, colored per the provider's bands.
pub(super) fn trailing_for_account(
    a: &AcctView,
    bands: SeverityBands,
    now: DateTime<Utc>,
) -> (String, Vec<(usize, usize, Severity)>) {
    let (mut text, colors) = trailing_usage_for_account(a, bands, now);
    if has_reset(a) {
        text.push_str(" 🎁");
    }
    (text, colors)
}

/// 🎁 on the account row only for a reset worth claiming now; saved ones are
/// listed in the submenu.
fn has_reset(a: &AcctView) -> bool {
    a.reported.reset_grants.iter().any(ResetGrant::is_ready)
}

fn trailing_usage_for_account(
    a: &AcctView,
    bands: SeverityBands,
    now: DateTime<Utc>,
) -> (String, Vec<(usize, usize, Severity)>) {
    // No plan, no usage: a countdown to a reset that frees nothing would lie.
    // Outranks auth trouble too — a Free account is unusable whether or not
    // its login is healthy, so the plan is the headline, not "re-login".
    if a.no_subscription {
        let t = a.plan.clone().unwrap_or_else(|| "Free".to_string());
        let len = u16len(&t);
        return (t, vec![(0, len, Severity::Red)]);
    }
    // Auth trouble outranks any usage/lock display: an account whose refresh
    // token was rejected can never refresh, so its cached pct is frozen and a
    // "-% / -%" or stale countdown would misrepresent a dead login as a
    // transient poll gap. Surface the reason instead (full detail lands in the
    // submenu status row).
    if a.needs_relogin {
        let t = if a.renewable {
            "⚠ renew"
        } else {
            "⚠ re-login"
        }
        .to_string();
        let len = u16len(&t);
        return (t, vec![(0, len, Severity::Red)]);
    }
    if a.error.is_some() {
        let t = "⚠ error".to_string();
        let len = u16len(&t);
        return (t, vec![(0, len, Severity::Red)]);
    }
    // A login about to die outranks usage: once it's gone the numbers freeze
    // and the account can't be switched to. Full detail is in the submenu.
    if let Some(left) = login_expiring_in(a, now) {
        // Already past its expiry: the same treatment as a rejected login.
        if login_expired(left) {
            let t = "⚠ renew".to_string();
            let len = u16len(&t);
            return (t, vec![(0, len, Severity::Red)]);
        }
        let t = format!("renew · {}", login_left_label(left));
        let len = u16len(&t);
        return (t, vec![(0, len, Severity::Amber)]);
    }
    let display = countdown::compute_display(&account_usage_for(a), now);
    // Plan used up but usage credits still pay for requests: the account
    // works, so show what's left to spend, not a lock countdown. The unlock
    // time stays in the submenu.
    if matches!(display, DisplayState::Locked { .. }) {
        if let Some(left) = spendable_credits(a) {
            return (format!("{} credits", left.display_whole()), Vec::new());
        }
    }
    // A maxed window with unknown reset: the other window's unlock time is
    // not when the account frees up, so show the percentages, not a countdown.
    let usage = account_usage_for(a);
    let display = if countdown::has_indefinite_block(&usage) {
        DisplayState::Usage {
            session_pct: usage.session_pct,
            weekly_pct: usage.weekly_pct,
        }
    } else {
        display
    };
    match display {
        DisplayState::Locked {
            until,
            window: BlockingWindow::Weekly,
        } => {
            let cd = countdown::format_countdown(until - now);
            let len = u16len(&cd);
            (cd, vec![(0, len, Severity::Red)])
        }
        DisplayState::Locked {
            until,
            window: BlockingWindow::Session,
        } => {
            let cd = countdown::format_countdown(until - now);
            let (_, wp) = summary_pcts(a);
            let wb = pct(wp);
            let trailing = format!("{cd} / {wb}");
            let mut colors = vec![(0, u16len(&cd), Severity::Red)];
            let w_off = u16len(&cd) + u16len(" / ");
            if let Some(sev) = severity_with(wp, bands) {
                colors.push((w_off, u16len(&wb), sev));
            }
            (trailing, colors)
        }
        DisplayState::StaleAfterReset { .. } if weekly_only(a) => ("-%".to_string(), Vec::new()),
        DisplayState::StaleAfterReset { .. } => ("-% / -%".to_string(), Vec::new()),
        // No session window at all (Claude Ultra, Codex): the weekly pct is
        // the only number, so show it alone rather than `— / 66%`.
        DisplayState::Usage {
            session_pct: None,
            weekly_pct: Some(wp),
        } => {
            let t = pct(Some(wp));
            let colors = severity_with(Some(wp), bands)
                .map(|sev| vec![(0, u16len(&t), sev)])
                .unwrap_or_default();
            (t, colors)
        }
        DisplayState::Usage {
            session_pct,
            weekly_pct,
        } => {
            let sa = pct(session_pct);
            let sb = pct(weekly_pct);
            let trailing = format!("{sa} / {sb}");
            let mut colors = Vec::new();
            if let Some(sev) = severity_with(session_pct, bands) {
                colors.push((0, u16len(&sa), sev));
            }
            let w_off = u16len(&sa) + u16len(" / ");
            if let Some(sev) = severity_with(weekly_pct, bands) {
                colors.push((w_off, u16len(&sb), sev));
            }
            (trailing, colors)
        }
    }
}

/// Credits the account can still spend, if it can run on them at all.
pub(super) fn spendable_credits(a: &AcctView) -> Option<crate::providers::trait_def::Money> {
    a.reported.credits.as_ref()?.spendable()
}

/// Indent applied to each account row so it visually reads as sitting under
/// its provider-group header row (`provider_group_header_row`). Two spaces
/// works well with menu-font kerning and keeps the trailing tab column aligned
/// across every provider. The account row carries no `{provider} · ` prefix;
/// it sits under a bold provider-group header row instead.
// Empty under muri: muri reserves a leading gutter (icon/checkmark column) that
// already nests account rows under the provider header, so the old two-space
// text indent (a native-NSMenu-era hack) now stacks on top of that gutter and
// produces a large left margin. Drop it and let muri's layout do the nesting.
const ACCOUNT_INDENT: &str = "";

/// v0.5.3 menu redesign: the row for one account — `<indent><email>\t<trailing>`
/// where `<trailing>` is whatever `trailing_for_account` decides (locked
/// countdown, locked-session-with-weekly-headroom, stale-after-reset
/// placeholder, or plain `sa / sb`). Provider grouping is BACK as a visual
/// concept: this row now sits under a bold `provider_group_header_row` and
/// no longer prefixes `{provider} · ` — the group header carries the provider.
/// The row itself is also the LABEL of a submenu (see
/// `menu_tree_from_snapshot`) whose children are the details + action rows
/// the flat shape used to inline. Same `TabX::MenuRight` participation as the
/// Quit row (so the trailing column lines up).
pub(super) fn account_header_row(sec: &ProviderSection, a: &AcctView) -> RowStyle {
    let label = format!("{ACCOUNT_INDENT}{}", a.display);
    let base = u16len(&label) + 1; // + '\t'
    let now = now_utc();
    let (trailing, rel_colors) = trailing_for_account(a, sec.severity_bands, now);
    let plain = format!("{label}\t{trailing}");
    let colors = rel_colors
        .into_iter()
        .map(|(off, len, sev)| (base + off, len, sev))
        .collect();
    let bold_spans = if a.active {
        binding_span(a, &trailing, now)
            .map(|(off, len)| vec![(base + off, len)])
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    RowStyle {
        bold: a.active,
        bold_spans,
        colors,
        tab_x_kind: Some(TabX::MenuRight),
        checkmark: a.active,
        ..RowStyle::plain_row(plain)
    }
}

/// The bold provider-group header row that precedes a provider's account rows.
/// Plain title is just the provider's display name (e.g. "Claude"), bold,
/// disabled (`enabled: false` so it reads as a group heading, not a click
/// target), and carries the per-provider 16px icon — the icon belongs to the
/// GROUP, not to each account inside it. Every provider's header is the same
/// plain, non-interactive label row — never a submenu, never highlighted. A
/// provider-wide fact (the env override disabling swaps) rides inline in the
/// label so it still renders once at the group level.
pub(super) fn provider_group_header_row(sec: &ProviderSection) -> RowStyle {
    let label = if sec.env_override_active {
        format!("{} · {ENV_OVERRIDE_ROW_TITLE}", sec.display_name)
    } else {
        sec.display_name.to_string()
    };
    RowStyle {
        bold: true,
        section_header: true,
        icon_slug: Some(sec.provider_id),
        ..RowStyle::plain_row(label)
    }
}

/// The plain title of the Quit row, kept in one place so the builder and the
/// tests asserting on it can't drift. The `\t` splits "Quit" from the trailing
/// version label; muri's compat layer lays that out as a right-aligned second
/// column (see `plain_text`), so a forgotten `\t` would collapse it to a flat
/// "Quit usagio vX.Y.Z".
pub(super) fn quit_row_plain() -> String {
    // `USAGIO_BUILD_TAG`, set at compile time, is appended so a QA screenshot
    // self-identifies which build rendered it (distinguishing a dev build from
    // an installed release when both are running). Unset in normal/release
    // builds, so the label stays exactly "Quit\tusagio vX.Y.Z".
    // `MURI_BG=r,g,b,a` (runtime bg-tuning hook): surface the alpha on the Quit
    // row so phone-photo comparisons self-label which alpha is on screen.
    let bg = std::env::var("MURI_BG")
        .ok()
        .and_then(|s| s.rsplit(',').next().map(|a| a.trim().to_string()))
        .map(|a| format!(" a={a}"))
        .unwrap_or_default();
    match option_env!("USAGIO_BUILD_TAG") {
        Some(tag) if !tag.is_empty() => {
            format!("Quit\tusagio v{} [{}]{bg}", env!("CARGO_PKG_VERSION"), tag)
        }
        _ => format!("Quit\tusagio v{}{bg}", env!("CARGO_PKG_VERSION")),
    }
}

/// One window's line inside an account submenu, and whether it's a lock.
/// Percentages live on the account row, so these lines answer only what the
/// row's bare countdown can't: WHICH window binds and WHEN it frees. The two
/// states use different verbs so the words alone say which is which: a maxed
/// window is `<Label> locked · unlocks in <X>` (rendered red, like the row's
/// countdown); a healthy one `<Label> renews in <X>` ("resets in" beside a
/// lock read as if that window were locked too). `None` for a healthy window
/// with no known reset — nothing useful to say.
///
/// `stale`: the window's reset already passed and the cached reading predates
/// it (`DisplayState::StaleAfterReset`), so it is not locked, just unrefreshed.
fn window_status_row_at(w: &WindowView, stale: bool) -> Option<(String, bool)> {
    let label = stat_display_label(w);
    if stale {
        return Some((format!("{label} reset · awaiting fresh usage"), false));
    }
    let locked = w.pct.is_some_and(|p| p >= countdown::LOCKED_THRESHOLD_PCT);
    match (locked, w.reset.is_empty()) {
        (true, false) => Some((format!("{label} locked · unlocks in {}", w.reset), true)),
        (true, true) => Some((format!("{label} locked"), true)),
        (false, false) => Some((format!("{label} renews in {}", w.reset), false)),
        (false, true) => None,
    }
}

/// Show only what can be used now, never historical spending or a spend cap.
/// Nothing to spend → no row (a "Credits · 0" line is just clutter).
fn credits_row(c: &Credits) -> Option<String> {
    let status = if !c.enabled && !c.unlimited || c.limit_reached {
        return None;
    } else if c.unlimited {
        "unlimited".to_string()
    } else if let Some(left) = c.spendable().filter(|m| m.amount_minor > 0) {
        left.display()
    } else if c.balance.is_some() || c.limit.is_some() {
        return None;
    } else {
        "—".to_string()
    };
    Some(format!("Credits · {status}"))
}

/// "This week · Claude Code 82% · Chats 18%" — only when at least two
/// surfaces have a non-zero share (a lone "Claude Code 100%" is noise).
/// Sorted by share, zeros dropped.
fn breakdown_row(shares: &[UsageShare]) -> Option<String> {
    let mut nonzero: Vec<&UsageShare> = shares
        .iter()
        .filter(|s| s.percent.is_finite() && s.percent > 0.0)
        .collect();
    if nonzero.len() < 2 {
        return None;
    }
    // Stable sort keeps provider order among equal shares.
    nonzero.sort_by(|a, b| {
        b.percent
            .partial_cmp(&a.percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let parts: Vec<String> = nonzero
        .iter()
        .map(|s| format!("{} {}%", s.label, s.percent.round() as i64))
        .collect();
    Some(format!("This week · {}", parts.join(" · ")))
}

/// "Fable 7d · 42% · resets in 2d 3h" — a per-model/surface limit, only when
/// it's actually in use (0% or unreported is skipped).
fn scoped_limit_row(w: &UsageWindow) -> Option<String> {
    let pct = w.utilization.filter(|p| p.is_finite() && *p > 0.0)?;
    let mut row = format!("{} · {}%", w.label, pct.round() as i64);
    if let Some(reset) = w.resets_at {
        row.push_str(&format!(
            " · resets in {}",
            crate::display::humanize_until(reset)
        ));
    }
    Some(row)
}

/// Whether usagio can claim this provider's reset grants itself.
/// Codex redemption is not integrated; only Claude has a claim action.
fn claims_resets(provider_id: &str) -> bool {
    provider_id == "claude"
}

/// Expiry is only worth a word once it's close; months away is noise.
const RESET_EXPIRY_SHOWN_WITHIN_DAYS: i64 = 14;

/// What a grant resets, as the menu names it: "session", "weekly", both, or
/// unknown (`None`) when the provider lists neither.
fn reset_kind(g: &ResetGrant) -> Option<&'static str> {
    match (g.clears_session(), g.clears_weekly()) {
        (true, true) => Some("session + weekly"),
        (true, false) => Some("session"),
        (false, true) => Some("weekly"),
        (false, false) => None,
    }
}

/// The reset-grant rows for an account, identical grants merged into one row
/// with a count, each paired with whether it's ready to claim. Ready means a
/// claim would clear something in use (see [`ResetGrant::is_ready`]).
///
/// * `🎁 Session reset ready` — claim clears the 5-hour limit.
/// * `🎁 2 resets saved · won't clear weekly` — the provider lists what it
///   clears and the weekly limit isn't in it (a weekly-capped account).
/// * `🎁 Reset saved` — the provider doesn't say what it clears (Codex).
pub(super) fn reset_grant_rows(grants: &[ResetGrant]) -> Vec<(String, bool)> {
    // (kind, ready, won't-clear-weekly) → (count, earliest expiry), in first-seen order.
    type Key = (Option<&'static str>, bool, bool);
    let mut groups: Vec<(Key, u32, Option<DateTime<Utc>>)> = Vec::new();
    for g in grants.iter().filter(|g| g.is_available()) {
        let ready = g.is_ready();
        let not_weekly = !ready && !g.clears.is_empty() && !g.clears_weekly();
        let key = (reset_kind(g), ready, not_weekly);
        match groups.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, n, exp)) => {
                *n += g.resets_left;
                *exp = match (*exp, g.expires_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
            None => groups.push((key, g.resets_left, g.expires_at)),
        }
    }
    // Same clock as `humanize_until`, which renders the expiry.
    let now = Utc::now();
    groups
        .into_iter()
        .map(|((kind, ready, not_weekly), n, exp)| {
            let noun = if n == 1 { "reset" } else { "resets" };
            let what = match (kind, n) {
                (Some(k), 1) => capitalize(&format!("{k} {noun}")),
                (Some(k), _) => format!("{n} {k} {noun}"),
                (None, 1) => "Reset".to_string(),
                (None, _) => format!("{n} {noun}"),
            };
            let state = if ready { "ready" } else { "saved" };
            let mut row = format!("🎁 {what} {state}");
            if not_weekly {
                row.push_str(" · won't clear weekly");
            }
            if let Some(exp) =
                exp.filter(|e| *e - now < chrono::Duration::days(RESET_EXPIRY_SHOWN_WITHIN_DAYS))
            {
                row.push_str(&format!(
                    " · expires in {}",
                    crate::display::humanize_until(exp)
                ));
            }
            (row, ready)
        })
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Every provider-reported row for an account, in render order: reset grants
/// (good news up top), scoped limits, where the week went, then credits. Pure;
/// provider-agnostic.
fn reported_rows(r: &ReportedUsage) -> Vec<String> {
    let mut rows: Vec<String> = reset_grant_rows(&r.reset_grants)
        .into_iter()
        .map(|(row, _)| row)
        .collect();
    rows.extend(r.scoped_limits.iter().filter_map(scoped_limit_row));
    rows.extend(breakdown_row(&r.breakdown));
    rows.extend(r.credits.as_ref().and_then(credits_row));
    rows
}

/// Non-clickable informational rows shown inside an account submenu — reset
/// windows (or the has_data / no-usage-endpoint fallbacks). The account submenu
/// builder renders each as an `enabled: true` `noop` item so it reads as normal
/// text rather than muri's greyed-out disabled look. Kept as its own function
/// so the builder and the unit tests asserting on the row text can't drift.
///
/// Deliberately excludes the burn-rate / cost-estimate / "updated" footer rows
/// (`account_extra_info_rows` yields those separately): those touch the on-disk
/// usage log, and this function must stay pure (no disk I/O) so it's safe to
/// call from a unit test without a `ScopedConfigDir`.
#[cfg(test)]
pub(super) fn submenu_info_rows(sec: &ProviderSection, a: &AcctView) -> Vec<String> {
    submenu_info_items(sec, a)
        .into_iter()
        .map(|(r, _)| r)
        .collect()
}

/// [`submenu_info_rows`] plus whether each line is a lock (rendered red).
pub(super) fn submenu_info_items(sec: &ProviderSection, a: &AcctView) -> Vec<(String, bool)> {
    let mut rows: Vec<(String, bool)> = Vec::new();
    // A lapsed plan is the headline even over auth trouble (mirrors
    // `trailing_for_account`): the account is unusable either way.
    if a.no_subscription {
        rows.push((
            format!(
                "No subscription · {} plan",
                a.plan.as_deref().unwrap_or("Free")
            ),
            true,
        ));
        return rows;
    }
    // Auth trouble is the account's headline state — spell out the reason and
    // the fix before any (now-frozen) usage numbers. Mirrors the `⚠` trailing
    // that `trailing_for_account` puts on the row header.
    if a.needs_relogin {
        if a.renewable {
            rows.push(("⚠ Login expired — Renew login… below".to_string(), true));
        } else {
            rows.push(("⚠ Re-login required — token expired".to_string(), true));
            rows.push((
                format!(
                    "Fix: log in with `claude`, then `usagio capture` ({})",
                    a.key
                ),
                false,
            ));
        }
        return rows;
    }
    if let Some(left) = login_expiring_in(a, now_utc()) {
        let row = if login_expired(left) {
            "⚠ Login expired — Renew login… below".to_string()
        } else {
            format!(
                "Login expires in {} — Renew login… below",
                login_left_label(left)
            )
        };
        rows.push((row, true));
    }
    if let Some(err) = &a.error {
        rows.push((format!("⚠ Refresh error: {err}"), true));
        return rows;
    }
    if let Some(plan) = &a.plan {
        rows.push((format!("Plan · {plan}"), false));
    }
    if sec.supports_usage {
        if a.has_data && !a.windows.is_empty() {
            // A window at 100% is the binding lock — lead with it (that's the
            // "what's locked / when does it unlock" the header's bare countdown
            // can't convey); otherwise keep window order (session before weekly).
            // Stable sort preserves that order among the non-locked windows.
            let now = now_utc();
            // A window whose reset passed before the next refresh is not
            // locked, whatever its stale cached pct says.
            let stale = |w: &WindowView| window_is_stale_after_reset(a, &w.id, now);
            let locked_now = |w: &WindowView| {
                w.pct.is_some_and(|p| p >= countdown::LOCKED_THRESHOLD_PCT) && !stale(w)
            };
            let mut ordered: Vec<&WindowView> = a.windows.iter().collect();
            ordered.sort_by_key(|w| u8::from(!locked_now(w)));
            // When the WEEKLY window is locked, the account is fully blocked
            // regardless of the session window — so drop the "Session renews in X"
            // line (its countdown is meaningless while the weekly limit blocks all
            // use). The "Weekly locked · unlocks in X" row says everything.
            let weekly_locked = a.windows.iter().any(|w| w.id == "weekly" && locked_now(w));
            let before = rows.len();
            for w in ordered {
                if weekly_locked && !locked_now(w) {
                    continue;
                }
                if let Some((mut text, locked)) = window_status_row_at(w, stale(w)) {
                    if locked && spendable_credits(a).is_some() {
                        text.push_str(" · using credits");
                    }
                    rows.push((text, locked)); // (text, locked)
                }
            }
            // What the provider itself reported beyond the windows (credits,
            // where the week went, per-model limits) — never estimates.
            rows.extend(reported_rows(&a.reported).into_iter().map(|r| (r, false)));
            // Usage is known (the header shows it); only the reset times are
            // missing, so say that rather than "no data yet".
            if rows.len() == before {
                rows.push(("reset times not reported".to_string(), false));
            }
        } else {
            rows.push(("no data yet".to_string(), false));
        }
    } else {
        rows.push(("(no usage endpoint — headers only)".to_string(), false));
    }
    rows
}

/// The disk-derived informational rows for an account's submenu, in render
/// order: burn-rate estimate (`🔥`) and the "updated Xm ago" footer. Split out
/// from `submenu_info_rows` — which must stay disk-I/O-free so it's callable
/// from a unit test without a `ScopedConfigDir` — because the burn rate reads
/// the on-disk usage log via `crate::burn_rate`. `build_account_submenu_item`
/// appends them after the `submenu_info_rows` block.
pub(super) fn account_extra_info_rows(sec: &ProviderSection, a: &AcctView) -> Vec<String> {
    let mut rows = Vec::new();
    if sec.supports_usage && a.has_data && !a.windows.is_empty() && !a.no_subscription {
        let account_key =
            crate::usage_log::AccountKey::new(sec.provider_id.to_string(), a.key.clone());
        // The burn-rate row forecasts when the WEEKLY window empties ("Weekly ·
        // N% · ~X"). Once the weekly window is already locked (100%),
        // that reads as a redundant "· 100% · ~0m" — skip it; the
        // "Weekly locked · unlocks in X" status row already conveys the lock.
        let weekly_locked = a.windows.iter().any(|w| {
            w.id == "weekly" && w.pct.is_some_and(|p| p >= countdown::LOCKED_THRESHOLD_PCT)
        });
        if !weekly_locked {
            if let Some(row) = empty_in_row(&account_key, a, now_utc()) {
                rows.push(row);
            }
        }
        rows.push(format!("updated {}", a.updated));
    }
    rows
}

/// The compact weekly forecast row from the one rate model (auto-swap v2's
/// `rates` + `runway::time_to`), at the account's own pace.
fn empty_in_row(
    account_key: &crate::usage_log::AccountKey,
    a: &AcctView,
    now: DateTime<Utc>,
) -> Option<String> {
    use crate::autoswap::{rates::rates_for, runway::time_to, WindowId};
    let pct = a.windows.iter().find(|w| w.id == "weekly")?.pct?;
    // No pace yet → nothing to forecast; the weekly % is already on the row.
    let rate = rates_for(&account_key.provider, &account_key.account, now).own(WindowId::Weekly)?;
    let mins = time_to(pct, 100.0, rate);
    let empty_at = if mins.is_finite() {
        now + chrono::Duration::seconds((mins * 60.0).min(1.0e8) as i64)
    } else {
        now + chrono::Duration::hours(crate::burn_rate::MAX_EMPTY_IN_HOURS)
    };
    let est = crate::burn_rate::BurnRateEstimate {
        window: crate::providers::trait_def::Window::Weekly,
        current_pct: pct as f32,
        rate_pct_per_hour: (rate * 60.0) as f32,
        empty_at,
        reset_at: a.weekly_reset_at,
    };
    crate::burn_rate::format_forecast(&est)
}

/// Which optional rows `build_account_submenu_item` should append for `sec`'s
/// account `a`. Pure over `ProviderSection`/`AcctView` fields, with no
/// `muda`/`tray_icon` types involved, so this decision is unit-testable
/// without a live main-thread `NSApplication` (a bare `muda::Menu` can only
/// be constructed on the main thread, which `cargo test`'s worker threads
/// aren't). `supports_switching` used to be the only
/// gate consulted, for BOTH the Switch and Launch rows, and Remove had no
/// gate at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AccountSubmenuRows {
    /// Offer switching only for an inactive account with a subscription.
    pub(super) switch_row: bool,
    /// "Renew login…" — usagio can run this provider's login itself.
    pub(super) renew_row: bool,
    pub(super) remove_row: bool,
    /// The ready reset label is actionable for this account.
    pub(super) claim_reset_row: bool,
}

pub(super) fn account_submenu_rows(sec: &ProviderSection, a: &AcctView) -> AccountSubmenuRows {
    AccountSubmenuRows {
        switch_row: sec.supports_switching && !a.active && !a.no_subscription,
        renew_row: a.renewable,
        remove_row: sec.supports_remove,
        // Only offer the claim when spending a grant would clear something in
        // use — otherwise the reset is used up for nothing.
        claim_reset_row: claims_resets(sec.provider_id)
            && a.reported.reset_grants.iter().any(ResetGrant::is_ready),
    }
}

/// A stable fingerprint of everything the menu renders. When it's unchanged we
/// skip `set_menu`, so an open menu is never dismissed by a no-op poll.
///
/// Provider id is folded into every account fingerprint so a Codex-side change
/// doesn't collide with a stale Claude signature (or vice versa) once state
/// carries multiple providers.
pub(super) fn menu_signature(snap: &Snapshot) -> String {
    let mut s = String::new();
    for sec in &snap.sections {
        s.push_str(&format!(
            "SEC[{}={}|sw={}|us={}|env={}|",
            sec.provider_id,
            sec.display_name,
            sec.supports_switching,
            sec.supports_usage,
            sec.env_override_active,
        ));
        for a in &sec.accounts {
            // `a.provider_id` mirrors `sec.provider_id` in v1; folding both
            // keeps the signature honest once state v2 tags each account
            // with its own slug and a mis-bucketed account could exist.
            // The locked-state trailing text (`locked · Xh Ym`) doesn't come
            // from any window's `w.pct/w.reset` string — it's computed from
            // the reset instants + a wall-clock `now`. Fold the raw instants
            // plus the current `DisplayState` variant into the signature so a
            // usage→locked transition, or a change to *which* window is
            // blocking, triggers a redraw. Instants use RFC3339 for stability.
            let sr = a
                .session_reset_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            let wr = a
                .weekly_reset_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            // The `lock` marker alone is not enough: while an account stays
            // Locked the (fixed) reset instant + (fixed) variant produce a
            // static signature for hours, so `install_menu` never re-runs and
            // the header's `locked · Xh Ym` text freezes at whatever value it
            // had when the transition happened. Fold the CURRENT `format_countdown`
            // output into the signature so each minute (or hour, depending on
            // remaining time) the fingerprint changes and the tray redraws with
            // the fresh remaining time. Uses `now_utc()` — the same clock
            // `main_row` renders from.
            let now = now_utc();
            let lock = match countdown::compute_display(&account_usage_for(a), now) {
                DisplayState::Locked {
                    window: BlockingWindow::Session,
                    until,
                } => {
                    format!("L=S|cd={}", countdown::format_countdown(until - now))
                }
                DisplayState::Locked {
                    window: BlockingWindow::Weekly,
                    until,
                } => {
                    format!("L=W|cd={}", countdown::format_countdown(until - now))
                }
                DisplayState::Usage { .. } => "L=0".to_string(),
                DisplayState::StaleAfterReset { window, reset } => {
                    format!("L=SA|w={window:?}|r={}", reset.to_rfc3339())
                }
            };
            s.push_str(&format!(
                "{}@{}/{}|{}|{}|sr={}|wr={}|{}|rl={}|ns={}|plan={}|",
                a.provider_id,
                sec.provider_id,
                a.key,
                a.active,
                a.has_data,
                sr,
                wr,
                lock,
                a.needs_relogin,
                a.no_subscription,
                a.plan.as_deref().unwrap_or(""),
            ));
            if let Some(left) = login_expiring_in(a, now) {
                s.push_str(&format!("renew={}|", login_left_label(left)));
            }
            s.push_str(&format!("rn={}|err={:?}|", a.renewable, a.error));
            for w in &a.windows {
                s.push_str(&format!(
                    "{}={}:r={}|",
                    w.id,
                    // The rendered text, so a tie like 2.5 -> "2%" vs 3.0 -> "3%"
                    // can't hide behind a different rounding rule.
                    pct(w.pct),
                    w.reset,
                ));
            }
            // Provider-reported rows exactly as rendered, so a credits /
            // breakdown / scoped-limit change redraws an open menu.
            for row in reported_rows(&a.reported) {
                s.push_str(&format!("rep={row}|"));
            }
            // Deliberately EXCLUDE `a.updated` from the signature. It's the
            // human "Xs ago" / "Xm ago" string that changes every second for
            // the first minute after each poll — folding it in forced a full
            // `install_menu` on every 0.75s tick (defeating the whole cache).
            // Real underlying freshness is already captured by `a.has_data`
            // and each window's pct+reset above.
        }
        s.push_str("] ");
    }
    // Capture-submenu contents also affect the redraw: adding a new registered
    // provider must show up. Both
    // buckets fold into the signature so moving a provider between them (e.g.
    // a capture-mode change) also triggers a redraw.
    for reg in &snap.capture_creds {
        s.push_str(&format!(
            "REG[{}|{}|creds] ",
            reg.provider_id, reg.display_name
        ));
    }
    for reg in &snap.capture_api_key {
        s.push_str(&format!(
            "REG[{}|{}|apikey] ",
            reg.provider_id, reg.display_name
        ));
    }
    let nc = &snap.notification_config;
    s.push_str(&format!(
        "as={} th={:.0} uc={} ti={:?} nt={}{}{}",
        snap.autoswap,
        snap.threshold,
        snap.use_credits,
        snap.tray_icon_mode,
        nc.threshold_enabled,
        nc.reset_back_enabled,
        nc.pace_enabled,
    ));
    s
}

/// Recovery information for the selected provider. A countdown is the first
/// account that can clear *all* its blocking windows, not the first reset of
/// any kind. Expired cached limits remain pending until a successful fetch.
pub(super) fn recovery_status(
    sec: &ProviderSection,
    active: &AcctView,
    now: DateTime<Utc>,
) -> Option<(String, String)> {
    if active.no_subscription {
        return Some((
            "No plan".into(),
            format!("{} · No subscription", active.display),
        ));
    }
    if active.needs_relogin {
        return Some((
            "Re-login".into(),
            format!("{} · Re-login required", active.display),
        ));
    }
    if active.error.is_some() {
        return Some((
            "⚠ error".into(),
            format!("{} · Refresh error", active.display),
        ));
    }
    let state = countdown::compute_display(&account_usage_for(active), now);
    // Plan used up but credits still pay for requests: the account works, so
    // the title shows what's left to spend, same as the account row.
    if matches!(state, DisplayState::Locked { .. }) {
        if let Some(left) = spendable_credits(active) {
            return Some((
                format!("{} credits", left.display_whole()),
                format!("{} · Plan used up · using credits", active.display),
            ));
        }
    }
    if matches!(state, DisplayState::Usage { .. })
        && !blocking_pct(active).is_some_and(|p| p >= countdown::LOCKED_THRESHOLD_PCT)
    {
        return None;
    }
    let mut earliest: Option<(&AcctView, DateTime<Utc>)> = None;
    let mut pending = None;
    for account in &sec.accounts {
        if account.needs_relogin
            || account.error.is_some()
            || account.no_subscription
            || !account.has_data
        {
            continue;
        }
        if account.key != active.key && (!sec.supports_switching || sec.env_override_active) {
            continue;
        }
        let usage = account_usage_for(account);
        // A known reset for one limit cannot establish availability if the
        // other exhausted limit has no reset time.
        if countdown::has_indefinite_block(&usage) {
            continue;
        }
        match countdown::compute_display(&usage, now) {
            DisplayState::Locked { until, .. } => {
                if earliest.is_none_or(|(_, previous)| until < previous) {
                    earliest = Some((account, until));
                }
            }
            DisplayState::StaleAfterReset { .. } => pending = Some(account),
            DisplayState::Usage { .. } => {
                if blocking_pct(account).is_some_and(|p| p < countdown::LOCKED_THRESHOLD_PCT) {
                    return Some((
                        "Switch".into(),
                        format!("{} · Usage available", account.display),
                    ));
                }
            }
        }
    }
    if let Some(account) = pending {
        return Some((
            "Refreshing…".into(),
            format!("{} · Reset passed; awaiting fresh usage", account.display),
        ));
    }
    if let Some((account, until)) = earliest {
        return Some((
            format!("🔒 {}", countdown::format_countdown(until - now)),
            format!(
                "Next available: {} · {} · awaiting confirmation at reset",
                account.display,
                until
                    .with_timezone(&chrono::Local)
                    .format("%b %d, %H:%M %Z")
            ),
        ));
    }
    Some((
        "🔒".into(),
        "Usage blocked · reset time not reported".into(),
    ))
}

pub(super) fn title_for(snap: &Snapshot) -> String {
    let mut title = usage_title_for(snap);
    if snap
        .sections
        .iter()
        .any(|sec| sec.accounts.iter().any(has_reset))
    {
        title.push_str(" 🎁");
    }
    title
}

fn usage_title_for(snap: &Snapshot) -> String {
    match tray_target(snap, snap.tray_icon_mode.as_deref()) {
        Some((sec, a)) => {
            let now = now_utc();
            if let Some((title, _)) = recovery_status(sec, a, now) {
                return title;
            }
            binding_title(a, shown_binding(sec, a, now, snap.threshold))
        }
        None => "—".to_string(),
    }
}

/// Render the current menu (built from the live/fixture `state.json` the
/// process's config dir points at) straight to a PNG via muri's headless
/// offscreen renderer (muri #59, 0.10.9) under a forced OS theme — no tray,
/// window, display, or TCC/Accessibility. `theme_name` is
/// `windows|macos|gnome|system`; a forced theme renders that OEM look on ANY
/// host, so all three OS screenshots come from one runner. Drives the
/// deterministic website-screenshot pipeline (`usagio __render_shot`) and,
/// later, cross-OS golden tests.
///
/// `render_menu` yields a native `muri::Menu` directly (0.11 migration), so
/// this goes through the same native paint layer a live popup uses (bold
/// active row, severity tint) — a faithful capture with no
/// launch+screencapture+click harness. Renders the TOP-LEVEL menu (submenu
/// flyouts are a later muri capability).
pub(crate) fn render_menu_png_for_theme(theme_name: &str, scale: f32) -> Vec<u8> {
    let snap = build_snapshot();
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    let native = crate::platform::render::render_menu(&tree);
    let theme = theme_from_name(theme_name).unwrap_or_default();
    let opts = tray_options().theme(theme);
    muri::render_menu_to_png(&native, &opts, scale)
}

pub(super) fn pct(p: Option<f64>) -> String {
    p.map(|v| format!("{v:.0}%"))
        .unwrap_or_else(|| "—".to_string())
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
