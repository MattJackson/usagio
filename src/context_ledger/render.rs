//! Terminal renderer for the ledger — column-aligned tree with totals.

use super::{ItemKind, Ledger};

pub fn terminal(ledger: &Ledger) -> String {
    let mut out = String::new();
    let project = ledger
        .project
        .as_ref()
        .map(|p| format!(", project: {}", p.display()))
        .unwrap_or_default();
    out.push_str(&format!(
        "Context Ledger — {}{}\n",
        display_provider(&ledger.provider),
        project,
    ));

    if ledger.items.is_empty() {
        out.push_str("  (no context items discovered)\n");
        return out;
    }

    // Group by kind for readability.
    let ordered = [
        ItemKind::GlobalInstructions,
        ItemKind::ProjectInstructions,
        ItemKind::Skill,
        ItemKind::SubagentDef,
        ItemKind::PluginManifest,
        ItemKind::McpTools,
        ItemKind::Other,
    ];

    // Column width for the name so token counts align.
    let name_col = ledger
        .items
        .iter()
        .map(|i| i.name.chars().count())
        .max()
        .unwrap_or(20)
        .max(20);

    for kind in ordered {
        let group: Vec<_> = ledger.items.iter().filter(|i| i.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        let subtotal: usize = group.iter().map(|i| i.token_count).sum();
        out.push_str(&format!(
            "  {:name_col$}   {:>10} tok\n",
            display_kind(kind),
            fmt_thousands(subtotal),
            name_col = name_col,
        ));
        for item in group {
            let tool_hint = item
                .tool_count
                .map(|n| format!(" ({} tools)", n))
                .unwrap_or_default();
            out.push_str(&format!(
                "    {:name_col$}{:>10} tok{}\n",
                item.name,
                fmt_thousands(item.token_count),
                tool_hint,
                name_col = name_col.saturating_sub(2),
            ));
        }
    }

    out.push_str(&format!("  {:-<width$}\n", "", width = name_col + 20));
    out.push_str(&format!(
        "  {:name_col$}   {:>10} tok\n",
        "Total baseline per turn",
        fmt_thousands(ledger.total_tokens),
        name_col = name_col,
    ));
    if let Some(cost) = ledger.estimated_cost_per_turn {
        out.push_str(&format!(
            "  {:name_col$}   ${:>9.4}\n",
            "Est. cost per turn",
            cost,
            name_col = name_col,
        ));
    }
    out
}

fn display_provider(slug: &str) -> &str {
    match slug {
        "claude" | "claude-code" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "opencode",
        other => other,
    }
}

fn display_kind(k: ItemKind) -> &'static str {
    match k {
        ItemKind::GlobalInstructions => "Global instructions",
        ItemKind::ProjectInstructions => "Project instructions",
        ItemKind::Skill => "Skills",
        ItemKind::SubagentDef => "Subagents",
        ItemKind::PluginManifest => "Plugins",
        ItemKind::McpTools => "MCP servers",
        ItemKind::Other => "Other",
    }
}

fn fmt_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_ledger::LedgerItem;
    use chrono::Utc;
    use std::path::PathBuf;

    fn item(kind: ItemKind, name: &str, tokens: usize, tools: Option<usize>) -> LedgerItem {
        LedgerItem {
            kind,
            name: name.into(),
            path: PathBuf::from("/x"),
            mtime: Utc::now(),
            token_count: tokens,
            content_bytes: 0,
            tool_count: tools,
        }
    }

    fn ledger(provider: &str, items: Vec<LedgerItem>, cost: Option<f64>) -> Ledger {
        let total_tokens = items.iter().map(|i| i.token_count).sum();
        Ledger {
            provider: provider.into(),
            project: None,
            items,
            total_tokens,
            estimated_cost_per_turn: cost,
            captured_at: Utc::now(),
        }
    }

    #[test]
    fn fmt_thousands_groups_digits() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(7), "7");
        assert_eq!(fmt_thousands(100), "100");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(1_000), "1,000");
        assert_eq!(fmt_thousands(12_345), "12,345");
        assert_eq!(fmt_thousands(123_456), "123,456");
        assert_eq!(fmt_thousands(1_234_567), "1,234,567");
        assert_eq!(fmt_thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn provider_display_names() {
        assert_eq!(display_provider("claude"), "Claude Code");
        assert_eq!(display_provider("claude-code"), "Claude Code");
        assert_eq!(display_provider("codex"), "Codex");
        assert_eq!(display_provider("opencode"), "opencode");
        assert_eq!(display_provider("other-cli"), "other-cli");
    }

    #[test]
    fn kind_display_names() {
        assert_eq!(
            display_kind(ItemKind::GlobalInstructions),
            "Global instructions"
        );
        assert_eq!(
            display_kind(ItemKind::ProjectInstructions),
            "Project instructions"
        );
        assert_eq!(display_kind(ItemKind::Skill), "Skills");
        assert_eq!(display_kind(ItemKind::SubagentDef), "Subagents");
        assert_eq!(display_kind(ItemKind::PluginManifest), "Plugins");
        assert_eq!(display_kind(ItemKind::McpTools), "MCP servers");
        assert_eq!(display_kind(ItemKind::Other), "Other");
    }

    #[test]
    fn empty_ledger_renders_heading_and_placeholder() {
        let out = terminal(&ledger("codex", vec![], None));
        assert_eq!(
            out,
            "Context Ledger — Codex\n  (no context items discovered)\n"
        );
    }

    #[test]
    fn project_shown_in_heading() {
        let mut l = ledger("claude", vec![], None);
        l.project = Some(PathBuf::from("/tmp/proj"));
        assert!(terminal(&l).starts_with("Context Ledger — Claude Code, project: /tmp/proj\n"));
    }

    #[test]
    fn full_render_groups_by_kind_with_subtotals_and_cost() {
        let l = ledger(
            "claude",
            vec![
                item(ItemKind::Skill, "skill: a", 1_500, None),
                item(ItemKind::GlobalInstructions, "Global CLAUDE.md", 250, None),
                item(ItemKind::Skill, "skill: b", 500, None),
                item(ItemKind::McpTools, "mcp: srv", 12_345, Some(4)),
            ],
            Some(0.0123),
        );
        let out = terminal(&l);
        let expected = [
            "Context Ledger — Claude Code",
            "  Global instructions           250 tok",
            "    Global CLAUDE.md         250 tok",
            "  Skills                      2,000 tok",
            "    skill: a               1,500 tok",
            "    skill: b                 500 tok",
            "  MCP servers                12,345 tok",
            "    mcp: srv              12,345 tok (4 tools)",
            "  ----------------------------------------",
            "  Total baseline per turn       14,595 tok",
            "  Est. cost per turn     $   0.0123",
            "",
        ]
        .join("\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn long_names_widen_the_column() {
        let name = "n".repeat(30);
        let out = terminal(&ledger(
            "codex",
            vec![item(ItemKind::Other, &name, 5, None)],
            None,
        ));
        let total = out.lines().last().unwrap();
        assert_eq!(
            total,
            format!("  {:30}   {:>10} tok", "Total baseline per turn", "5")
        );
    }
}
