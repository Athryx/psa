use super::*;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub(super) struct ParserOverride(pub(super) Arc<dyn Fn(&str) -> Vec<Node> + Send + Sync>);

struct StartedParse {
    text: String,
    release: oneshot::Sender<()>,
}

fn gated_parser() -> (ParserOverride, mpsc::Receiver<StartedParse>) {
    let (started, receiver) = mpsc::channel(1);
    let parser = move |text: &str| {
        let (release, wait) = oneshot::channel();
        // If a test fails, dropping its gate/receiver still releases the CPU job.
        let _ = started.blocking_send(StartedParse {
            text: text.to_owned(),
            release,
        });
        let _ = wait.blocking_recv();
        parse_markdown(text)
    };
    (ParserOverride(Arc::new(parser)), receiver)
}

async fn next_parse(started: &mut mpsc::Receiver<StartedParse>) -> StartedParse {
    tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .expect("worker must make progress")
        .expect("worker must still exist")
}

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

#[test]
fn late_references_and_reopened_fences_reinterpret_the_whole_document() {
    let prefix = "[late]\n\n```rust\n**inside**\n";
    let partial = render(prefix);
    assert!(!partial.contains("markdown-link"), "{partial}");
    assert!(!partial.contains("<strong>inside"), "{partial}");
    let complete = render(&format!(
        "{prefix}```\n\n[late]: https://example.com/late\n\n**outside**\n\n```\n**inside again**"
    ));
    assert!(complete.contains("markdown-link"), "{complete}");
    assert!(complete.contains("https://example.com/late"), "{complete}");
    assert!(complete.contains("<strong>outside</strong>"), "{complete}");
    assert!(!complete.contains("<strong>inside"), "{complete}");
    assert!(complete.contains("**inside again**"), "{complete}");
}

#[test]
fn large_markdown_without_tokio_still_renders_completely_in_ssr() {
    assert!(Handle::try_current().is_err());
    let html = render(&format!(
        "{}\n\n**complete**",
        "x".repeat(ASYNC_PARSE_BYTES)
    ));
    assert!(html.contains("<strong>complete</strong>"));
    assert!(!html.contains("markdown-pending"));
}

#[tokio::test(flavor = "current_thread")]
async fn only_text_at_or_above_the_threshold_defers_parsing() {
    let mut text = "x".repeat(ASYNC_PARSE_BYTES - "\n\n**small**".len() - 1);
    text.push_str("\n\n**small**");
    let small = render(&text);
    assert!(small.contains("<strong>small</strong>"));
    assert!(!small.contains("markdown-pending"));
    text.push('x');
    assert_eq!(text.len(), ASYNC_PARSE_BYTES);
    let large = render(&text);
    assert!(large.contains("markdown-pending"));
    assert!(large.contains("**small**x"));
    assert!(!large.contains("<strong>small</strong>"));
}

#[tokio::test(flavor = "current_thread")]
async fn worker_bounds_pending_input_discards_stale_results_and_finishes_latest() {
    let (parser, mut started) = gated_parser();
    let (requests, receiver) = watch::channel(None);
    let (output, mut results) = mpsc::channel(1);
    let worker = tokio::spawn(run_parse_worker(
        receiver,
        Handle::current(),
        move |text| (parser.0)(text),
        move |result| output.try_send(result).unwrap(),
    ));
    requests.send_replace(Some(Arc::from("first")));
    let first = next_parse(&mut started).await;
    assert_eq!(first.text, "first");

    let mut previous = None::<std::sync::Weak<str>>;
    for revision in 0..100 {
        let text: Arc<str> = format!("**latest {revision}**").into();
        let weak = Arc::downgrade(&text);
        requests.send_replace(Some(text));
        if let Some(previous) = previous {
            assert!(
                previous.upgrade().is_none(),
                "obsolete pending text retained"
            );
        }
        previous = Some(weak);
    }
    assert!(matches!(
        started.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    first.release.send(()).unwrap();
    let latest = next_parse(&mut started).await;
    assert_eq!(latest.text, "**latest 99**");
    assert!(matches!(
        results.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    latest.release.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), results.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*result.text, "**latest 99**");
    let html = dioxus_ssr::render_element(rsx! { {result.nodes.iter().map(render_node)} });
    assert!(html.contains("<strong>latest 99</strong>"));
    drop(requests);
    worker.await.unwrap();
    assert!(
        started.recv().await.is_none(),
        "intermediate input was parsed"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn closed_worker_discards_in_flight_result_and_queued_input() {
    let (parser, mut started) = gated_parser();
    let (requests, receiver) = watch::channel(None);
    let (output, mut results) = mpsc::channel(1);
    let worker = tokio::spawn(run_parse_worker(
        receiver,
        Handle::current(),
        move |text| (parser.0)(text),
        move |result| output.try_send(result).unwrap(),
    ));
    requests.send_replace(Some(Arc::from("in flight")));
    let first = next_parse(&mut started).await;
    requests.send_replace(Some(Arc::from("never parse")));
    drop(requests);
    first.release.send(()).unwrap();
    worker.await.unwrap();
    assert!(results.recv().await.is_none());
    assert!(started.recv().await.is_none());
}

#[derive(Clone)]
struct MarkdownFixture {
    initial: String,
    controls: Rc<RefCell<Option<Signal<String>>>>,
}

fn markdown_fixture(props: MarkdownFixture) -> Element {
    let text = use_signal(|| props.initial);
    use_hook(|| *props.controls.borrow_mut() = Some(text));
    rsx! { Markdown { text: text() } }
}

async fn next_mounted_parse(
    dom: &mut VirtualDom,
    started: &mut mpsc::Receiver<StartedParse>,
) -> StartedParse {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                parse = started.recv() => return parse.expect("mounted parser"),
                _ = dom.wait_for_work() => { dom.render_immediate_to_vec(); }
            }
        }
    })
    .await
    .expect("mounted worker must make progress")
}

#[tokio::test(flavor = "current_thread")]
async fn mounted_large_markdown_shows_safe_latest_text_then_complete_parse_and_drops_worker() {
    let (parser, mut started) = gated_parser();
    let prefix = format!(
        "[late]\n\n{}\n\n```rust\n**inside**\n<script>alert(1)</script>\n",
        "x".repeat(ASYNC_PARSE_BYTES),
    );
    let controls = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(
        markdown_fixture,
        MarkdownFixture {
            initial: prefix.clone(),
            controls: controls.clone(),
        },
    );
    dom.provide_root_context(parser);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("markdown-pending"));
    assert!(html.contains("white-space: pre-wrap"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("&#60;script&#62;"));
    let first = next_mounted_parse(&mut dom, &mut started).await;

    let complete = format!(
        "{prefix}```\n\n[late]: https://example.com/late\n\n**latest**\n\n```\n**inside again**"
    );
    let mut text = controls.borrow().unwrap();
    text.set(complete.clone());
    dom.render_immediate_to_vec();
    assert!(dioxus_ssr::render(&dom).contains("**latest**"));
    first.release.send(()).unwrap();
    let latest = next_mounted_parse(&mut dom, &mut started).await;
    assert_eq!(latest.text, complete);
    assert!(dioxus_ssr::render(&dom).contains("markdown-pending"));
    latest.release.send(()).unwrap();
    let html = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            dom.render_immediate_to_vec();
            let html = dioxus_ssr::render(&dom);
            if html.contains("<strong>latest</strong>") {
                break html;
            }
            dom.wait_for_work().await;
        }
    })
    .await
    .expect("latest complete document must be published");
    assert!(!html.contains("markdown-pending"));
    assert!(html.contains("markdown-link"));
    assert!(html.contains("https://example.com/late"));
    assert!(!html.contains("<strong>inside"));
    assert!(html.contains("**inside again**"));

    text.set(format!("{complete}\n\nunmounted in flight"));
    dom.render_immediate_to_vec();
    let in_flight = next_mounted_parse(&mut dom, &mut started).await;
    text.set(format!("{complete}\n\nqueued before unmount"));
    dom.render_immediate_to_vec();
    drop(dom);
    in_flight.release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .unwrap()
            .is_none(),
        "unmounted component started another parse"
    );
}
