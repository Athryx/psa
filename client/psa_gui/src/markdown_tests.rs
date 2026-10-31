use super::*;

fn render(text: &str) -> String {
    dioxus_ssr::render_element(rsx! { Markdown { text: text.to_string() } })
}

#[test]
fn renders_common_markdown_as_nodes() {
    let html = render(
        "# Heading\n\n**bold** and *em* and `code`\n\n- one\n- two\n\n> quote\n\n```rust\nlet x = 1;\n```",
    );
    for part in [
        "<h1>",
        "<strong>",
        "<em>",
        "<code>",
        "<ul>",
        "<li>",
        "<blockquote>",
        "<pre>",
    ] {
        assert!(html.contains(part), "missing {part}: {html}");
    }
}

#[test]
fn raw_html_and_untrusted_urls_are_inert() {
    let html = render(
        "<script>alert(1)</script>\n\n[x](javascript:alert(1)) [y](data:text/html,bad) [z](file:///tmp/a) [r](/relative) [good](https://example.com/a) ![tracking](https://example.com/pixel)",
    );
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&#60;script&#62;"), "{html}");
    assert!(!html.contains("<a ") && !html.contains("<img"), "{html}");
    assert!(
        !html.contains("javascript:") && !html.contains("data:text") && !html.contains("file:"),
        "{html}"
    );
    assert!(html.contains("https://example.com/a"), "{html}");
    assert!(!html.contains("example.com/pixel"), "{html}");
    for value in [
        "javascript:alert(1)",
        "https://example.com/\nabc",
        "//example.com",
        "data:text/plain,x",
    ] {
        assert!(safe_url(value).is_none(), "{value}");
    }
}

#[test]
fn tables_tasks_and_code_info_are_rendered_safely() {
    let html = render(
        "| Name | Value |\n| --- | --- |\n| one | **two** |\n\n- [x] done\n- [ ] pending\n\n```rust extra\nlet x = 1;\n```",
    );
    for part in [
        "<table>",
        "<thead>",
        "<tr>",
        "<th",
        "<td>",
        "<strong>two</strong>",
        "☑",
        "☐",
        "markdown-code-language\">rust</div>",
    ] {
        assert!(html.contains(part), "missing {part}: {html}");
    }
    assert!(!html.contains("<input"), "{html}");
    let html = render("```<script>\nx\n```");
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&#60;script&#62;"), "{html}");
}

#[test]
fn incomplete_streams_render_and_completed_text_is_reparsed() {
    let partial = render("Hello **world");
    assert!(partial.contains("world"), "{partial}");
    let complete = render("Hello **world**");
    assert!(complete.contains("<strong>world</strong>"), "{complete}");
    let unfinished_fence = render("```rust\nlet x = 1;");
    assert!(
        unfinished_fence.contains("let x = 1;"),
        "{unfinished_fence}"
    );
}
