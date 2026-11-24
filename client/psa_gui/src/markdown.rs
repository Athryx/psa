//! Markdown is rendered as Dioxus nodes, never as HTML. Links are deliberately
//! inert until the platform has an explicit external-URL opening boundary.
use dioxus::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use std::{cell::RefCell, rc::Rc, sync::Arc};
use tokio::{runtime::Handle, sync::watch};

const ASYNC_PARSE_BYTES: usize = 16 * 1024;

#[cfg(test)]
#[path = "tests/markdown/tests.rs"]
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

#[derive(Debug)]
struct ParsedMarkdown {
    text: Arc<str>,
    nodes: Vec<Node>,
}

/// One actor per mounted component, with one running parse and one latest slot.
/// Always parse the whole snapshot: late reference definitions and closing fences
/// can change the interpretation of any earlier block.
async fn run_parse_worker(
    mut requests: watch::Receiver<Option<Arc<str>>>,
    runtime: Handle,
    parse: impl Fn(&str) -> Vec<Node> + Send + Sync + 'static,
    mut publish: impl FnMut(ParsedMarkdown),
) {
    let parse = Arc::new(parse);
    while requests.changed().await.is_ok() {
        let Some(text) = requests.borrow_and_update().clone() else {
            continue;
        };
        let input = text.clone();
        let parse = parse.clone();
        let result = runtime.spawn_blocking(move || parse(&input)).await;

        // A closed channel means unmounted, even if it still contains this text.
        if requests.has_changed().is_err() {
            return;
        }
        let is_current = requests
            .borrow()
            .as_ref()
            .is_some_and(|latest| Arc::ptr_eq(latest, &text));
        if is_current && let Ok(nodes) = result {
            publish(ParsedMarkdown { text, nodes });
        }
    }
}

#[component]
pub(crate) fn Markdown(text: String) -> Element {
    let mut parsed = use_signal(|| None::<ParsedMarkdown>);
    let worker = use_hook(|| Rc::new(RefCell::new(None::<watch::Sender<Option<Arc<str>>>>)));
    let runtime = Handle::try_current().ok();

    // Small messages and renderer-free SSR keep the synchronous behavior. An
    // intervening small snapshot must also invalidate any outstanding large one.
    if text.len() < ASYNC_PARSE_BYTES || runtime.is_none() {
        if let Some(requests) = worker.borrow().as_ref() {
            requests.send_if_modified(|latest| latest.take().is_some());
        }
        let nodes = parse_markdown(&text);
        return rsx! { div { class: "markdown", {nodes.iter().map(render_node)} } };
    }

    let mut worker = worker.borrow_mut();
    let requests = worker.get_or_insert_with(|| {
        let (requests, receiver) = watch::channel(None);
        #[cfg(test)]
        let parser_override = try_use_context::<tests::ParserOverride>();
        // Scope ownership cancels delivery on unmount. A blocking parse already
        // running may finish, but cannot publish or start another queued parse.
        spawn(run_parse_worker(
            receiver,
            runtime.unwrap(),
            move |text| {
                #[cfg(test)]
                if let Some(parser) = &parser_override {
                    return (parser.0)(text);
                }
                parse_markdown(text)
            },
            move |result| parsed.set(Some(result)),
        ));
        requests
    });
    requests.send_if_modified(|latest| {
        if latest.as_deref() == Some(text.as_str()) {
            return false;
        }
        *latest = Some(Arc::from(text.as_str()));
        true
    });
    let current = requests.borrow().clone().unwrap();
    drop(worker);

    let parsed = parsed.read();
    let current_nodes = parsed
        .as_ref()
        .filter(|result| Arc::ptr_eq(&result.text, &current));
    rsx! {
        div { class: "markdown",
            if let Some(result) = current_nodes {
                {result.nodes.iter().map(render_node)}
            } else {
                // Never hide streaming updates behind an old AST, nor treat
                // pending text as HTML. Whitespace stays visible without <pre>'s
                // code-block styling or horizontal overflow.
                div { class: "markdown-pending", style: "white-space: pre-wrap; overflow-wrap: anywhere;", "{text}" }
            }
        }
    }
}
