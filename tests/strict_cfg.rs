//! Guards the "no scattered `#[cfg(target_os)]` outside `src/platform/*`"
//! rule at `cargo test` time, so a violation is caught locally rather than
//! only in CI. All host-OS-specific behavior must be routed through the
//! `Platform` trait (see `src/platform/mod.rs`) and implemented in
//! `src/platform/{macos,linux,windows}.rs`.
//!
//! This mirrors the CI step in `.github/workflows/ci.yml` ("strict-cfg
//! lint"), which runs the equivalent one-liner:
//!
//! ```sh
//! grep -rn 'cfg(target_os' src/ | grep -v '^src/platform/'
//! ```

use std::fs;
use std::path::{Path, PathBuf};

/// Whether `line` mentions the `target_os` cfg predicate. Deliberately NOT
/// tied to `cfg(`/`cfg_attr(` appearing on the same line: rustfmt wraps long
/// attributes so `target_os = "..."` can sit alone on a continuation line
/// (`#[cfg(\n    target_os = "linux"\n)]`). Any non-comment line outside
/// `src/platform` containing the token is a candidate and must be allowlisted
/// by its exact text. (Plain `cfg(unix)` / `cfg(windows)` — no `target_os` —
/// are the *sanctioned* per-OS-family escape hatch and are not covered; see
/// `menubar::relaunch_via_launchd_kickstart`.)
fn is_os_cfg_line(line: &str) -> bool {
    line.contains("target_os")
}

/// Source files that are `#[cfg(test)]`-only modules: those a parent file
/// declares with `mod NAME;` directly under a `#[cfg(test)]` attribute
/// (honouring `#[path = "..."]`). Determined from the declarations, never
/// from the file name.
fn cfg_test_module_files(files: &[PathBuf]) -> std::collections::HashSet<PathBuf> {
    let mut out = std::collections::HashSet::new();
    for file in files {
        let contents = fs::read_to_string(file).unwrap_or_default();
        let lines: Vec<&str> = contents.lines().collect();
        let dir = file.parent().unwrap();
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let is_mod_root = matches!(stem, "mod" | "main" | "lib");
        let base = if is_mod_root {
            dir.to_path_buf()
        } else {
            dir.join(stem)
        };
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            let Some(name) = t
                .strip_prefix("mod ")
                .or_else(|| t.strip_prefix("pub(crate) mod "))
                .or_else(|| t.strip_prefix("pub mod "))
                .and_then(|r| r.strip_suffix(';'))
            else {
                continue;
            };
            // Contiguous attribute lines directly above the declaration.
            let mut attrs = Vec::new();
            let mut j = i;
            while j > 0 && lines[j - 1].trim().starts_with("#[") {
                j -= 1;
                attrs.push(lines[j].trim());
            }
            if !attrs.contains(&"#[cfg(test)]") {
                continue;
            }
            let explicit = attrs.iter().find_map(|a| {
                a.strip_prefix("#[path = \"")
                    .and_then(|r| r.strip_suffix("\"]"))
            });
            match explicit {
                Some(rel) => {
                    out.insert(dir.join(rel));
                }
                None => {
                    out.insert(base.join(format!("{name}.rs")));
                    out.insert(base.join(name).join("mod.rs"));
                }
            }
        }
    }
    out
}

/// Documented exceptions. Every entry is `(relative path, line number,
/// rationale)`. Additions require a comment explaining WHY the site can't
/// route through the `Platform` trait yet, and either a linked issue or a
/// note explaining what would need to happen for the exception to go away.
///
/// **menubar cross-OS wiring**: `src/menubar/` builds the tray menu ONCE for
/// every OS — a single `cross_platform::menu_tree_from_snapshot` produces the
/// generic `platform::MenuTree`, and `platform::render::render_menu` turns that
/// into a `muri::compat::tray_icon::menu::Menu` on each platform's UI thread
/// (the old macOS native `NSMenu` styler `mod mac_style` and the separate
/// per-OS menu builders were deleted in the 0.6.0 muri migration). What stays
/// macOS-gated is only the run-loop machinery macOS drives itself (in
/// `menubar/mod.rs`, with its imports grouped in one gated `use {}`): an
/// `NSApplication`/`NSTimer` loop needing `objc2`/`objc2-app-kit`/
/// `objc2-foundation`/`block2` — real
/// `[target.'cfg(target_os = "macos")'.dependencies]` in Cargo.toml that don't
/// exist in the dependency graph on Linux/Windows. The Linux/Windows
/// `MenuBackend` event loop + redraw pump (`cross_platform::run`/`redraw_loop`,
/// the `MenuHandle` channel, in `menubar/cross_platform.rs`) is the
/// mirror-image non-macOS gate. Those pieces
/// must be real `#[cfg(target_os = ...)]`, not something routable through a
/// runtime trait call; the menu *content* builders in `mod cross_platform` are
/// NOT gated (they compile everywhere and feed macOS too). Each entry below is
/// grouped to keep this list as short as the dependency-gating requirement
/// allows.
/// Each entry is `(relative path, the exact trimmed line containing
/// `target_os`, the trimmed source line the attribute guards)` — keyed on the
/// item, NOT a line number (so ordinary edits don't desync the list), AND on
/// the exact cfg text, so e.g. a lint-only `cfg_attr(..., allow(dead_code))`
/// can't be swapped for a real `cfg(target_os)` behaviour fork on the same
/// item without the gate noticing. A multi-line attribute is keyed on its
/// `target_os` continuation line. If a gated item or cfg changes, the failure
/// prints the new lines to paste here.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    // custom-popup NSPopover renderer: macOS + off-by-default feature; real macOS-only deps.
    (
        "src/main.rs",
        "#[cfg(all(target_os = \"macos\", feature = \"custom-popup\"))]",
        "mod ui;",
    ),
    (
        "src/menubar/mod.rs",
        "#[cfg(all(target_os = \"macos\", feature = \"custom-popup\"))]",
        "mod popover;",
    ),
    (
        "src/menubar/mod.rs",
        "#[cfg(target_os = \"macos\")]",
        "use std::cell::RefCell;",
    ),
    (
        "src/menubar/mod.rs",
        "#[cfg(target_os = \"macos\")]",
        "use {",
    ),
    (
        "src/menubar/mod.rs",
        "#[cfg(target_os = \"macos\")]",
        "pub(crate) fn run() -> Result<()> {",
    ),
    (
        "src/menubar/mod.rs",
        "#[cfg(not(target_os = \"macos\"))]",
        "pub(crate) fn run() -> Result<()> {",
    ),
    (
        "src/menubar/cross_platform.rs",
        "#[cfg(not(target_os = \"macos\"))]",
        "use {",
    ),
    (
        "src/menubar/cross_platform.rs",
        "#[cfg(not(target_os = \"macos\"))]",
        "fn initial_icon_bytes(snap: &Snapshot) -> &'static [u8] {",
    ),
    (
        "src/menubar/cross_platform.rs",
        "#[cfg(not(target_os = \"macos\"))]",
        "fn redraw_loop(handle: Box<dyn MenuHandle>, initial: Snapshot) {",
    ),
    (
        "src/menubar/cross_platform.rs",
        "#[cfg(not(target_os = \"macos\"))]",
        "pub(super) fn run() -> Result<()> {",
    ),
    // Lint-level only (`allow(dead_code)`): no behaviour branch.
    (
        "src/icons.rs",
        "#[cfg_attr(target_os = \"macos\", allow(dead_code))]",
        "pub(crate) fn usagio_tray_icon() -> &'static [u8] {",
    ),
    // Lint-level only (`allow(dead_code)`): no behaviour branch.
    (
        "src/menubar/tray.rs",
        "#[cfg_attr(target_os = \"macos\", allow(dead_code))]",
        "pub(super) fn tray_icon_bytes(snap: &Snapshot) -> &'static [u8] {",
    ),
    // Lint-level only (`allow(dead_code)`): no behaviour branch.
    (
        "src/menubar/tray.rs",
        "#[cfg_attr(not(target_os = \"macos\"), allow(dead_code))]",
        "pub(super) fn tooltip_for(snap: &Snapshot) -> String {",
    ),
];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect every `.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_scattered_target_os_cfg_outside_platform_module() {
    let root = src_root();
    let platform_dir = root.join("platform");

    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let allowlist: std::collections::HashSet<(PathBuf, &str, &str)> = ALLOWLIST
        .iter()
        .map(|(rel, cfg, guarded)| (manifest_dir.join(rel), *cfg, *guarded))
        .collect();

    // The first "real" source line at or after `idx` — skipping blank lines,
    // comments, and other attributes (`#[...]`). This is the item a `cfg`
    // attribute guards, and what the allowlist keys on (line-number-independent).
    let guarded_line = |lines: &[&str], idx: usize| -> Option<String> {
        lines[idx..].iter().find_map(|l| {
            let t = l.trim();
            if t.is_empty() || t.starts_with("//") || t.starts_with("#[") || t.starts_with("#!") {
                None
            } else {
                Some(t.to_string())
            }
        })
    };

    let test_modules = cfg_test_module_files(&files);

    let mut used = std::collections::HashSet::new();
    let mut violations = Vec::new();
    for path in &files {
        if path.starts_with(&platform_dir) || test_modules.contains(path) {
            continue;
        }
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let lines: Vec<&str> = contents.lines().collect();
        for (lineno, line) in lines.iter().enumerate() {
            // Skip comment lines — a doc-comment example / rationale that
            // MENTIONS `#[cfg(target_os = ...)]` is not itself a violation.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if is_os_cfg_line(line) {
                let guarded = guarded_line(&lines, lineno + 1).unwrap_or_default();
                let key = (path.clone(), line.trim().to_string(), guarded.clone());
                if allowlist.contains(&(path.clone(), line.trim(), guarded.as_str())) {
                    // Each entry covers exactly one site: a second identical
                    // site (e.g. another `mod tests {`) must be reviewed.
                    if !used.insert(key) {
                        violations.push(format!(
                            "{}:{}: {}  (guards: {}) — duplicate of an allowlisted site",
                            path.display(),
                            lineno + 1,
                            line.trim(),
                            guarded
                        ));
                    }
                    continue;
                }
                violations.push(format!(
                    "{}:{}: {}  (guards: {})",
                    path.display(),
                    lineno + 1,
                    line.trim(),
                    guarded
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "found #[cfg(target_os)] / cfg!(target_os) outside src/platform/ — \
         route platform-specific behavior through the Platform trait \
         (src/platform/mod.rs) instead:\n{}",
        violations.join("\n")
    );
}
