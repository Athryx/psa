//! One model turn's context, drawn by what occupies it. Each turn shown is a
//! snapshot, and arrows step between the agent's turns.
use crate::{
    app::Ui,
    network::{self, AdjacentTurns},
    reactive::{Shared, use_ui_selector},
    transcript_view::{ToolArguments, blocks},
};
use client_shared::*;
use dioxus::prelude::*;
use psa::api::events::{CompactionSummary, EventType};
use std::{collections::HashSet, rc::Rc};

/// Items listed at a time beneath the diagram, which itself covers every item.
const LISTED_ITEMS: usize = 200;
/// Beyond this many blocks the gaps between them narrow.
const DENSE_SEGMENTS: usize = 60;

pub(crate) fn format_tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..99_950 => format!("{:.1}k", count as f64 / 1_000.0),
        99_950..999_500 => format!("{}k", (count + 500) / 1_000),
        _ => format!("{:.2}M", count as f64 / 1_000_000.0),
    }
}

fn format_share(size: u64, total: u64) -> String {
    if total == 0 {
        return "–".to_owned();
    }
    let percent = size as f64 * 100.0 / total as f64;
    if size > 0 && percent < 0.1 {
        return "<0.1%".to_owned();
    }
    format!("{percent:.1}%")
}

/// An item's estimated tokens, or its text length when usage was unreported.
fn format_weight(makeup: &ContextMakeup, size: u64) -> String {
    match makeup.estimate_tokens(size) {
        Some(tokens) => format!("≈{}", format_tokens(tokens)),
        None => format!("{} chars", format_tokens(size)),
    }
}

fn fill_class(category: Option<ContextCategory>) -> String {
    match category {
        Some(category) => format!("context-fill-{}", category.index()),
        None => "context-fill-rest".to_owned(),
    }
}

#[derive(Clone, PartialEq)]
enum ContextLoad {
    Loading,
    Failed(String),
    Ready {
        makeup: Shared<ContextMakeup>,
        turn: u64,
    },
}

#[component]
pub(crate) fn ContextView(target: TranscriptTarget, turn: u64) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut load = use_signal(|| ContextLoad::Loading);
    let mut attempt = use_signal(|| 0_u32);
    let mut load_task = use_signal(|| None::<dioxus::core::Task>);
    let mut adjacent = use_signal(|| None::<AdjacentTurns>);
    let mut adjacent_task = use_signal(|| None::<dioxus::core::Task>);
    let latest_turn = use_ui_selector(move |state| {
        state
            .transcripts
            .get(&target)
            .and_then(|transcript| transcript.latest_turn())
    });
    let generation = use_ui_selector(|state| state.generation);
    let models = use_ui_selector(|state| state.models.clone());
    let agent_name = use_ui_selector(move |state| {
        state
            .agents
            .get(&target.session_id)
            .and_then(|agents| agents.iter().find(|agent| agent.id == target.agent_id))
            .map(|agent| agent.name.clone())
    });

    use_effect(use_reactive((&turn,), move |(turn,)| {
        // A retry or a new server connection reloads the snapshot.
        let _ = (attempt(), generation());
        if let Some(task) = load_task.take() {
            task.cancel();
        }

        // Stepping leaves the last turn on screen until this one arrives.
        if !matches!(*load.peek(), ContextLoad::Ready { .. }) {
            load.set(ContextLoad::Loading);
        }
        load_task.set(Some(spawn(async move {
            let loaded = network::load_context_events(ui, target, turn).await;
            load.set(match loaded {
                Ok(events) => match build_context_makeup(&events, turn) {
                    Some(makeup) => ContextLoad::Ready {
                        makeup: Shared::new(makeup),
                        turn,
                    },
                    None => {
                        ContextLoad::Failed("This turn was not found in the agent's history".into())
                    }
                },
                Err(error) => ContextLoad::Failed(error),
            });
        })));
    }));

    use_effect(use_reactive((&turn,), move |(turn,)| {
        // A new turn from the agent may give the last turn a successor.
        let _ = (attempt(), generation(), latest_turn());
        if let Some(task) = adjacent_task.take() {
            task.cancel();
        }
        adjacent_task.set(Some(spawn(async move {
            if let Ok(found) = network::load_adjacent_turns(ui, target, turn).await {
                adjacent.set(Some(found));
            }
        })));
    }));

    let neighbors = adjacent().filter(|found| found.turn == turn);
    let is_stepping =
        matches!(&*load.read(), ContextLoad::Ready { turn: shown, .. } if *shown != turn);
    let agent_name = agent_name().unwrap_or_else(|| format!("Agent {}", target.agent_id.0));
    // The arrows stay reachable whether or not this turn has loaded.
    let turns = rsx! {
        div { class: "context-turns", role: "group", "aria-label": "Move between turns",
            TurnStep { glyph: "←", label: "Previous turn", destination: neighbors.map(|found| found.previous), target }
            TurnStep { glyph: "→", label: "Next turn", destination: neighbors.map(|found| found.next), target }
        }
    };
    rsx! {
        section { class: if is_stepping { "context-view stepping" } else { "context-view" }, "aria-label": "Context makeup",
            match load() {
                ContextLoad::Loading => rsx! {
                    div { class: "context-toolbar", {turns} }
                    p { class: "muted", role: "status", "Loading this turn's context…" }
                },
                ContextLoad::Failed(error) => rsx! {
                    div { class: "context-toolbar", {turns} }
                    p { class: "error", role: "alert", "{error}" }
                    button { class: "context-retry", onclick: move |_| attempt += 1, "Retry" }
                },
                ContextLoad::Ready { makeup, .. } => {
                    let context_window = models
                        .read()
                        .iter()
                        .find(|model| model.model == makeup.model_config.model)
                        .and_then(|model| model.context_window);
                    rsx! { ContextDiagram { makeup, agent_name, context_window, turns } }
                }
            }
        }
    }
}

/// An arrow to an adjacent turn. `destination` is `None` until the shown turn's
/// neighbors are known, then `Some(None)` when there is no turn that way.
#[component]
fn TurnStep(
    glyph: &'static str,
    label: &'static str,
    destination: Option<Option<u64>>,
    target: TranscriptTarget,
) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    rsx! {
        button {
            r#type: "button", title: label, "aria-label": label,
            disabled: destination == Some(None),
            onclick: move |_| {
                if let Some(Some(turn)) = destination {
                    ui.write().open_context(target, turn);
                }
            },
            "{glyph}"
        }
    }
}

#[derive(Clone, PartialEq)]
struct DiagramModel {
    segments: Vec<ContextSegment>,
    items: Vec<ContextItem>,
    unlisted_items: usize,
}

#[component]
fn ContextDiagram(
    makeup: Shared<ContextMakeup>,
    agent_name: String,
    context_window: Option<u64>,
    /// Turn navigation, kept on one pinned row with the arrangement choices.
    turns: Element,
) -> Element {
    let mut sort = use_signal(ContextSort::default);
    let mut hovered = use_signal(|| None::<usize>);
    let mut list_limit = use_signal(|| LISTED_ITEMS);
    let mut expanded = use_signal(HashSet::<u64>::new);
    // A revision lets choosing the same block again scroll back to its item.
    let mut reveal = use_signal(|| (0_u64, None::<u64>));
    let model = use_memo(use_reactive((&makeup,), move |(makeup,)| {
        let items = makeup.sorted_items(sort());
        DiagramModel {
            segments: makeup.segments(sort()),
            unlisted_items: items.len().saturating_sub(list_limit()),
            items: items.into_iter().take(list_limit()).cloned().collect(),
        }
    }));
    let reveal_makeup = makeup.clone();
    let reveal_segment = use_callback(move |(first_item, is_single): (u64, bool)| {
        let position = reveal_makeup
            .sorted_items(*sort.peek())
            .iter()
            .position(|item| item.event_id == first_item)
            .unwrap_or(0);
        if position >= *list_limit.peek() {
            list_limit.set((position + 1).next_multiple_of(LISTED_ITEMS));
        }
        if is_single {
            expanded.write().insert(first_item);
        }
        let revision = reveal.peek().0 + 1;
        reveal.set((revision, Some(first_item)));
    });
    let model = model.read();
    let total = makeup.total_size();
    let mut totals = makeup.category_totals().to_vec();
    totals.retain(|total| total.items > 0);
    totals.sort_by_key(|total| std::cmp::Reverse(total.size));

    let usage = &makeup.usage;
    let window_share = match (usage.input_tokens, context_window) {
        (Some(tokens), Some(window)) if window > 0 => Some(format!(
            "{} of the {} window",
            format_share(tokens, window),
            format_tokens(window)
        )),
        _ => None,
    };
    let model_name = &makeup.model_config.model.api_name;
    let readout = hovered().and_then(|index| model.segments.get(index).cloned());
    rsx! {
        header { class: "context-header",
            h2 { "Context · {agent_name}" }
            p { class: "context-summary",
                match usage.input_tokens {
                    Some(tokens) => rsx! { strong { "{format_tokens(tokens)}" } " input tokens" },
                    None => rsx! { "Input tokens were not reported" },
                }
                if let Some(share) = window_share { " · {share}" }
                if let Some(cached) = usage.cached_input_tokens { " · {format_tokens(cached)} cached" }
                " · {makeup.items.len()} items · {model_name}"
            }
        }
        // Pinned while the list scrolls: controls, bar and readout move as one.
        div { class: "context-diagram",
            div { class: "context-toolbar",
                div { class: "context-sorts", role: "radiogroup", "aria-label": "Arrange the diagram by",
                    for (option, label, hint) in [
                        (ContextSort::Order, "Order", "Position in the conversation"),
                        (ContextSort::Size, "Size", "Largest items first"),
                        (ContextSort::Type, "Type", "One block per kind of content"),
                    ] {
                        button {
                            r#type: "button", role: "radio", title: hint,
                            "aria-checked": sort() == option,
                            class: if sort() == option { "selected" },
                            onclick: move |_| { hovered.set(None); sort.set(option); },
                            "{label}"
                        }
                    }
                }
                {turns}
            }
            if !model.segments.is_empty() {
                div {
                    class: if model.segments.len() > DENSE_SEGMENTS { "context-bar dense" } else { "context-bar" },
                    role: "group", "aria-label": "Share of the context by content; choose a block to go to its item",
                    onmouseleave: move |_| hovered.set(None),
                    for (index, segment, target) in model.segments.iter().enumerate().map(|(index, segment)| (index, segment, (segment.first_item, segment.items == 1))) {
                        button {
                            key: "{index}", r#type: "button", class: "context-segment {fill_class(segment.category)}",
                            style: "flex-grow:{segment.size}",
                            title: "{segment.label} · {format_weight(&makeup, segment.size)} · {format_share(segment.size, total)}",
                            onmouseenter: move |_| hovered.set(Some(index)),
                            onclick: move |_| reveal_segment.call(target),
                        }
                    }
                }
                p { class: "context-readout", "aria-live": "polite",
                    match readout {
                        Some(segment) => rsx! {
                            span { class: "context-swatch {fill_class(segment.category)}" }
                            span { class: "context-readout-label", "{segment.label}" }
                            span { class: "context-figure", "{format_weight(&makeup, segment.size)} · {format_share(segment.size, total)}" }
                            if segment.items > 1 { span { class: "muted", "{segment.items} items" } }
                        },
                        None => rsx! { span { class: "muted", "Hover a block for its details, or click it to go to its item." } },
                    }
                }
            }
        }
        if model.segments.is_empty() {
            p { class: "muted", "This turn was sent an empty context." }
        }
        table { class: "context-table",
            thead { tr { th { "Content" } th { "Items" } th { "Tokens" } th { "Share" } } }
            tbody {
                for total_row in totals {
                    tr { key: "{total_row.category.index()}",
                        th { scope: "row",
                            span { class: "context-swatch {fill_class(Some(total_row.category))}" }
                            "{total_row.category.label()}"
                        }
                        td { "{total_row.items}" }
                        td { "{format_weight(&makeup, total_row.size)}" }
                        td { "{format_share(total_row.size, total)}" }
                    }
                }
            }
        }
        h3 { class: "context-items-heading",
            match sort() {
                ContextSort::Order => "Items in order",
                ContextSort::Size => "Items by size",
                ContextSort::Type => "Items by type",
            }
        }
        ol { class: "context-items",
            for item in model.items.iter() {
                ContextItemRow { key: "{item.event_id}", item: item.clone(), makeup: makeup.clone(), expanded, reveal }
            }
        }
        if model.unlisted_items > 0 {
            button {
                class: "context-more", r#type: "button",
                onclick: move |_| list_limit += LISTED_ITEMS,
                "Show more ({model.unlisted_items} not listed)"
            }
        }
        p { class: "context-note muted",
            "Sizes are estimated from each item's text length and scaled to the reported input tokens; providers do not report tokens per item."
        }
    }
}

/// One listed item: a line that expands in place to what the model was sent.
#[component]
fn ContextItemRow(
    item: ContextItem,
    makeup: Shared<ContextMakeup>,
    mut expanded: Signal<HashSet<u64>>,
    reveal: Signal<(u64, Option<u64>)>,
) -> Element {
    let event_id = item.event_id;
    // Memos keep a toggle or jump from re-rendering every other row.
    let is_open = use_memo(move || expanded.read().contains(&event_id));
    let is_revealed = use_memo(move || reveal.read().1 == Some(event_id));
    let mut node = use_signal(|| None::<Rc<MountedData>>);

    use_effect(move || {
        let (_, target) = reveal();
        let Some(node) = node() else {
            return;
        };
        if target == Some(event_id) {
            spawn(async move {
                let _ = node.scroll_to(ScrollBehavior::Instant).await;
            });
        }
    });

    let total = makeup.total_size();
    rsx! {
        li {
            "data-revealed": is_revealed().then_some("true"),
            onmounted: move |event| node.set(Some(event.data())),
            button {
                class: "context-item-row", r#type: "button", "aria-expanded": is_open(),
                onclick: move |_| {
                    let mut items = expanded.write();
                    if !items.remove(&event_id) {
                        items.insert(event_id);
                    }
                },
                span { class: "context-swatch {fill_class(Some(item.category))}", title: item.category.label() }
                span { class: "context-item-label", "{item.label}" }
                span { class: "context-figure", "{format_weight(&makeup, item.size)}" }
                span { class: "context-figure context-share", "{format_share(item.size, total)}" }
            }
            if is_open() {
                div { class: "context-item-body", ContextItemBody { makeup: makeup.clone(), event_id } }
            }
        }
    }
}

fn format_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// The item as sent to the model: raw text, not the transcript's presentation.
#[component]
fn ContextItemBody(makeup: Shared<ContextMakeup>, event_id: u64) -> Element {
    let Some(event) = makeup.events.get(&event_id) else {
        return rsx! { p { class: "muted", "The contents of this item are unavailable." } };
    };
    let text = match &event.kind {
        EventType::ToolCall(call) => {
            return rsx! {
                ToolArguments { tool_name: call.tool_name.clone(), arguments: call.arguments.clone() }
            };
        }
        EventType::Thinking(thinking) if thinking.contents.summary.iter().all(String::is_empty) => {
            return rsx! { p { class: "muted", "This reasoning was sent only in the provider's opaque form." } };
        }
        EventType::Message(message) => blocks(&message.content),
        EventType::ToolResult(result) => blocks(&result.content),
        EventType::Thinking(thinking) => thinking.contents.summary.join("\n"),
        EventType::ToolDefinition(tool) => format_json(&tool.definition),
        EventType::Compaction(compaction) => match &compaction.summary {
            CompactionSummary::Text(summary) => summary.clone(),
            CompactionSummary::ProviderItem(item) => format_json(item),
        },
        EventType::ProviderItem(item) => format_json(item),
        _ => String::new(),
    };
    rsx! { pre { "{text}" } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::Usage;
    use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};

    fn makeup(input_tokens: Option<u64>) -> Shared<ContextMakeup> {
        let item = |event_id, category, label: &str, size| ContextItem {
            event_id,
            category,
            label: label.into(),
            size,
        };
        Shared::new(ContextMakeup {
            items: vec![
                item(1, ContextCategory::System, "Base instructions", 300),
                item(2, ContextCategory::User, "fix <b>the</b> bug", 100),
                item(3, ContextCategory::ToolResults, "read result", 600),
            ],
            events: Default::default(),
            usage: Usage {
                input_tokens,
                cached_input_tokens: Some(2_000),
                ..Default::default()
            },
            model_config: ModelConfig {
                model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
                effort: Effort::High,
            },
        })
    }

    #[test]
    fn token_counts_and_shares_format_compactly() {
        let formatted: Vec<_> = [0, 999, 1_000, 48_230, 99_949, 99_950, 128_400, 999_500]
            .into_iter()
            .map(format_tokens)
            .collect();
        assert_eq!(
            formatted,
            [
                "0", "999", "1.0k", "48.2k", "99.9k", "100k", "128k", "1.00M"
            ]
        );
        assert_eq!(format_share(1, 3), "33.3%");
        assert_eq!(format_share(1, 100_000), "<0.1%");
        assert_eq!(format_share(0, 100), "0.0%");
        assert_eq!(format_share(5, 0), "–");
    }

    #[test]
    fn diagram_draws_proportional_blocks_with_a_legend_table_and_escaped_labels() {
        let html = dioxus_ssr::render_element(rsx! {
            ContextDiagram { makeup: makeup(Some(10_000)), agent_name: "Root".to_owned(), context_window: Some(200_000), turns: rsx! {} }
        });
        assert!(html.contains("Context · Root"), "{html}");
        assert!(
            html.contains("<strong>10.0k</strong> input tokens"),
            "{html}"
        );
        assert!(html.contains("5.0% of the 200k window"), "{html}");
        assert!(
            html.contains("2.0k cached") && html.contains("test-model"),
            "{html}"
        );

        // Transcript order by default, each block weighted by its size.
        let order: Vec<_> = ["flex-grow:300", "flex-grow:100", "flex-grow:600"]
            .map(|style| html.find(style).unwrap())
            .into();
        assert!(order.is_sorted(), "{html}");
        assert!(html.contains("context-segment context-fill-0"), "{html}");
        assert!(html.contains("context-segment context-fill-7"), "{html}");
        assert!(html.contains("read result · ≈6.0k · 60.0%"), "{html}");

        // The table carries the same figures without relying on color.
        let table = &html[html.find("<table").unwrap()..html.find("</table>").unwrap()];
        let rows: Vec<_> = ["Tool results", "System prompts", "User messages"]
            .map(|label| table.find(label).unwrap())
            .into();
        assert!(rows.is_sorted(), "{table}");
        assert!(table.contains("<td>≈6.0k</td><td>60.0%</td>"), "{table}");
        assert!(!table.contains("Reasoning"), "{table}");
        assert!(
            html.contains("fix &#60;b&#62;the&#60;/b&#62; bug"),
            "{html}"
        );
        assert!(!html.contains("<b>the"), "{html}");
    }

    #[component]
    fn TurnStepRoot(destination: Option<Option<u64>>) -> Element {
        let ui = use_signal(|| Ui::new(DesktopSettings::default()));
        use_context_provider(|| ui);
        let target = TranscriptTarget {
            session_id: psa::api::SessionId(1),
            agent_id: psa::api::AgentId(1),
        };
        rsx! { TurnStep { glyph: "→", label: "Next turn", destination, target } }
    }

    #[test]
    fn turn_arrows_are_disabled_only_at_a_known_end() {
        let render =
            |destination| dioxus_ssr::render_element(rsx! { TurnStepRoot { destination } });
        assert!(render(Some(None)).contains("disabled"));
        assert!(!render(Some(Some(12))).contains("disabled"));
        assert!(!render(None).contains("disabled"));
    }

    #[component]
    fn ExpandedItemRoot(makeup: Shared<ContextMakeup>, event_id: u64) -> Element {
        let expanded = use_signal(|| HashSet::from([event_id]));
        let reveal = use_signal(|| (1, Some(event_id)));
        let item = makeup.items[0].clone();
        rsx! { ContextItemRow { item, makeup, expanded, reveal } }
    }

    fn render_expanded_item(kind: EventType) -> String {
        let mut makeup = makeup(Some(10_000));
        let mut item = makeup.items[0].clone();
        item.event_id = 42;
        makeup.items = vec![item];
        makeup.events.insert(
            42,
            serde_json::from_value(serde_json::json!({
                "id": 42, "source_agent_id": 1, "timestamp": "2026-01-01T00:00:00Z", "kind": kind,
            }))
            .unwrap(),
        );
        dioxus_ssr::render_element(rsx! { ExpandedItemRoot { makeup, event_id: 42 } })
    }

    #[test]
    fn expanded_items_show_what_the_model_was_sent() {
        use psa::api::events::{Message, MessageType, ToolResult};

        let html = render_expanded_item(EventType::Message(Message::text(
            MessageType::Human,
            "fix <b>this</b>",
        )));
        assert!(html.contains("aria-expanded=true"), "{html}");
        assert!(html.contains("data-revealed=\"true\""), "{html}");
        assert!(html.contains("fix &#60;b&#62;this"), "{html}");

        let html = render_expanded_item(EventType::ToolResult(ToolResult {
            delivery: None,
            call_id: "c".into(),
            error: false,
            metadata: Default::default(),
            content: Message::text(MessageType::Agent, "file contents").content,
        }));
        assert!(html.contains("<pre>file contents</pre>"), "{html}");
    }

    #[test]
    fn the_diagram_offers_each_block_as_a_control_and_collapses_items() {
        let html = dioxus_ssr::render_element(rsx! {
            ContextDiagram { makeup: makeup(Some(10_000)), agent_name: "Root".to_owned(), context_window: None, turns: rsx! {} }
        });
        assert_eq!(
            html.matches("context-segment context-fill").count(),
            3,
            "{html}"
        );
        assert_eq!(html.matches("aria-expanded=false").count(), 3, "{html}");
        assert!(!html.contains("context-item-body"), "{html}");
    }

    #[test]
    fn unreported_usage_falls_back_to_text_length() {
        let html = dioxus_ssr::render_element(rsx! {
            ContextDiagram { makeup: makeup(None), agent_name: "Root".to_owned(), context_window: None, turns: rsx! {} }
        });
        assert!(html.contains("Input tokens were not reported"), "{html}");
        assert!(html.contains("<td>600 chars</td><td>60.0%</td>"), "{html}");
        assert!(!html.contains("window"), "{html}");
    }
}
