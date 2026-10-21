//! Tool-provided directions rendered as separate system-prompt sections.

use std::collections::HashSet;

pub fn format_tools(directions: impl IntoIterator<Item = (String, String)>) -> Option<String> {
    let items = directions.into_iter().filter_map(|(name, direction)| {
        let direction = direction.trim();
        if direction.is_empty() {
            return None;
        }
        Some(format!("{name}: {direction}"))
    });
    format_section("tools", items)
}

pub fn format_rules(rules: impl IntoIterator<Item = String>) -> Option<String> {
    // Like pi, keep the first occurrence of each nonempty, trimmed rule.
    let mut seen = HashSet::new();
    let items = rules.into_iter().filter_map(|rule| {
        let rule = rule.trim();
        if rule.is_empty() || !seen.insert(rule.to_owned()) {
            return None;
        }
        Some(rule.to_owned())
    });
    format_section("rules", items)
}

fn format_section(section: &str, items: impl IntoIterator<Item = String>) -> Option<String> {
    let mut lines = Vec::new();
    for item in items {
        for (index, line) in item.lines().enumerate() {
            let prefix = if index == 0 { "- " } else { "  " };
            lines.push(format!("{prefix}{line}"));
        }
    }
    if lines.is_empty() {
        return None;
    }

    Some(format!("<{section}>\n{}\n</{section}>", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_preserve_order_markdown_and_multiline_list_items() {
        let prompt = format_tools([
            (
                "read".into(),
                " Read **files**.\nUse an absolute path. ".into(),
            ),
            ("hidden".into(), " \n ".into()),
            ("write".into(), "Create files.".into()),
        ]);
        assert_eq!(
            prompt.as_deref(),
            Some(
                "<tools>\n- read: Read **files**.\n  Use an absolute path.\n- write: Create files.\n</tools>"
            )
        );
    }

    #[test]
    fn rules_are_ordered_trimmed_and_deduplicated() {
        let prompt = format_rules([
            " Use absolute paths. ".into(),
            "\n".into(),
            "Use absolute paths.".into(),
            "Keep changes focused.\nPreserve unrelated code.".into(),
        ]);
        assert_eq!(
            prompt.as_deref(),
            Some(
                "<rules>\n- Use absolute paths.\n- Keep changes focused.\n  Preserve unrelated code.\n</rules>"
            )
        );
    }

    #[test]
    fn empty_sections_are_omitted() {
        assert_eq!(format_tools([]), None);
        assert_eq!(format_tools([("noop".into(), "  ".into())]), None);
        assert_eq!(format_rules([]), None);
        assert_eq!(format_rules(["  ".into()]), None);
    }
}
