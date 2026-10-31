//! Markdown is rendered as Dioxus nodes, never as HTML. Links are deliberately
//! inert until the platform has an explicit external-URL opening boundary.
use dioxus::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;

#[derive(Debug)]
enum Node {
    Container(Kind, Vec<Node>),
    Text(String),
    Code(String),
    Break,
    Rule,
}

#[derive(Debug)]
enum Kind {
    Root,
    Paragraph,
    Heading(HeadingLevel),
    Quote,
    CodeBlock(Option<String>),
    Table,
    TableHead,
    TableRow,
    TableCell { header: bool },
    List(Option<u64>),
    Item,
    Emphasis,
    Strong,
    Strike,
    Link(Option<String>),
    Image,
    Other,
}

fn safe_url(destination: &str) -> Option<String> {
    // Reject whitespace/control characters even where the URL parser would normalize them.
    if destination
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let url = url::Url::parse(destination).ok()?;
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
        Some(url.into())
    } else {
        None
    }
}

fn parse_markdown(text: &str) -> Vec<Node> {
    let mut stack = vec![(Kind::Root, Vec::new())];
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => {
                let kind = match tag {
                    Tag::Paragraph => Kind::Paragraph,
                    Tag::Heading { level, .. } => Kind::Heading(level),
                    Tag::BlockQuote(_) => Kind::Quote,
                    Tag::CodeBlock(code) => Kind::CodeBlock(match code {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().map(str::to_owned)
                        }
                        CodeBlockKind::Indented => None,
                    }),
                    Tag::Table(_) => Kind::Table,
                    Tag::TableHead => Kind::TableHead,
                    Tag::TableRow => Kind::TableRow,
                    Tag::TableCell => Kind::TableCell {
                        header: stack
                            .iter()
                            .any(|(kind, _)| matches!(kind, Kind::TableHead)),
                    },
                    Tag::List(start) => Kind::List(start),
                    Tag::Item => Kind::Item,
                    Tag::Emphasis => Kind::Emphasis,
                    Tag::Strong => Kind::Strong,
                    Tag::Strikethrough => Kind::Strike,
                    Tag::Link { dest_url, .. } => Kind::Link(safe_url(&dest_url)),
                    Tag::Image { .. } => Kind::Image,
                    _ => Kind::Other,
                };
                stack.push((kind, Vec::new()));
            }
            Event::End(_) => {
                if stack.len() > 1 {
                    let (kind, children) = stack.pop().unwrap();
                    stack
                        .last_mut()
                        .unwrap()
                        .1
                        .push(Node::Container(kind, children));
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => stack
                .last_mut()
                .unwrap()
                .1
                .push(Node::Text(text.into_string())),
            Event::Code(text) => stack
                .last_mut()
                .unwrap()
                .1
                .push(Node::Code(text.into_string())),
            Event::SoftBreak => stack.last_mut().unwrap().1.push(Node::Text("\n".into())),
            Event::HardBreak => stack.last_mut().unwrap().1.push(Node::Break),
            Event::Rule => stack.last_mut().unwrap().1.push(Node::Rule),
            Event::TaskListMarker(checked) => stack
                .last_mut()
                .unwrap()
                .1
                .push(Node::Text(if checked { "☑ " } else { "☐ " }.into())),
            Event::FootnoteReference(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                stack
                    .last_mut()
                    .unwrap()
                    .1
                    .push(Node::Text(text.into_string()))
            }
        }
    }
    stack.pop().unwrap().1
}

fn render_node(node: &Node) -> Element {
    match node {
        Node::Text(text) => rsx! { "{text}" },
        Node::Code(text) => rsx! { code { "{text}" } },
        Node::Break => rsx! { br {} },
        Node::Rule => rsx! { hr {} },
        Node::Container(kind, children) => {
            let rendered = children.iter().map(render_node);
            match kind {
                Kind::Root | Kind::Other => rsx! { {rendered} },
                Kind::Paragraph => rsx! { p { {rendered} } },
                Kind::Heading(HeadingLevel::H1) => rsx! { h1 { {rendered} } },
                Kind::Heading(HeadingLevel::H2) => rsx! { h2 { {rendered} } },
                Kind::Heading(HeadingLevel::H3) => rsx! { h3 { {rendered} } },
                Kind::Heading(HeadingLevel::H4) => rsx! { h4 { {rendered} } },
                Kind::Heading(HeadingLevel::H5) => rsx! { h5 { {rendered} } },
                Kind::Heading(HeadingLevel::H6) => rsx! { h6 { {rendered} } },
                Kind::Quote => rsx! { blockquote { {rendered} } },
                Kind::CodeBlock(language) => rsx! {
                    div { class: "markdown-code-block",
                        if let Some(language) = language { div { class: "markdown-code-language", "{language}" } }
                        pre { code { {rendered} } }
                    }
                },
                Kind::Table => rsx! { div { class: "markdown-table", table { {rendered} } } },
                Kind::TableHead => rsx! { thead { tr { {rendered} } } },
                Kind::TableRow => rsx! { tr { {rendered} } },
                Kind::TableCell { header: true } => rsx! { th { scope: "col", {rendered} } },
                Kind::TableCell { header: false } => rsx! { td { {rendered} } },
                Kind::List(Some(start)) => rsx! { ol { start: "{start}", {rendered} } },
                Kind::List(None) => rsx! { ul { {rendered} } },
                Kind::Item => rsx! { li { {rendered} } },
                Kind::Emphasis => rsx! { em { {rendered} } },
                Kind::Strong => rsx! { strong { {rendered} } },
                Kind::Strike => rsx! { del { {rendered} } },
                Kind::Link(Some(url)) => {
                    rsx! { span { class: "markdown-link", {rendered} " (" "{url}" ")" } }
                }
                Kind::Link(None) => rsx! { span { {rendered} } },
                Kind::Image => rsx! { span { class: "markdown-image", "[Image: " {rendered} "]" } },
            }
        }
    }
}

#[component]
pub(crate) fn Markdown(text: String) -> Element {
    let nodes = parse_markdown(&text);
    rsx! { div { class: "markdown", {nodes.iter().map(render_node)} } }
}
