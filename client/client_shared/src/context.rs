//! Estimated makeup of the context sent to the model for one inference.
//!
//! Providers report only a total input size. Each context item is weighed by
//! the length of its model-facing text and that weight is scaled to the
//! reported total, so shares are estimates rather than token counts.
use llm_api::api::{ContentBlock, HistoryItem, Usage};
use llm_api::providers::ModelConfig;
use psa_api::events::{CompactionSummary, Event, EventType, MessageType};
use std::collections::HashMap;

/// Assumed weight of an attachment whose token cost is not derivable from text.
const MEDIA_BLOCK_CHARS: u64 = 4000;
const LABEL_CHARS: usize = 72;
/// Runs narrower than this fraction of the context are merged with neighbors.
const ORDER_RESOLUTION: u64 = 150;
/// Individually drawn items when sorting by size; the rest share one segment.
const SIZE_SEGMENTS: usize = 48;

/// The kind of content occupying context. The order is fixed: presentation
/// assigns each category a stable color by its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContextCategory {
    System,
    ToolDefinitions,
    User,
    Harness,
    Agent,
    Reasoning,
    ToolCalls,
    ToolResults,
}

impl ContextCategory {
    pub const ALL: [Self; 8] = [
        Self::System,
        Self::ToolDefinitions,
        Self::User,
        Self::Harness,
        Self::Agent,
        Self::Reasoning,
        Self::ToolCalls,
        Self::ToolResults,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System prompts",
            Self::ToolDefinitions => "Tool definitions",
            Self::User => "User messages",
            Self::Harness => "Harness & summaries",
            Self::Agent => "Agent messages",
            Self::Reasoning => "Reasoning",
            Self::ToolCalls => "Tool calls",
            Self::ToolResults => "Tool results",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextItem {
    pub event_id: u64,
    pub category: ContextCategory,
    pub label: String,
    /// Characters of model-facing text, or an assumed weight for media.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategoryTotal {
    pub category: ContextCategory,
    pub items: usize,
    pub size: u64,
}

/// How the context diagram and its item list are arranged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextSort {
    /// Transcript order, showing where in the conversation the weight sits.
    #[default]
    Order,
    /// Largest items first.
    Size,
    /// One block per category, largest category first.
    Type,
}

/// One block of the context diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSegment {
    /// The category drawn, or `None` for a remainder spanning all categories.
    pub category: Option<ContextCategory>,
    pub label: String,
    pub items: usize,
    pub size: u64,
    /// Event ID of the segment's first item in the arrangement drawn.
    pub first_item: u64,
}

#[derive(Debug, Clone)]
pub struct ContextMakeup {
    /// Context items in transcript order.
    pub items: Vec<ContextItem>,
    /// The event behind each item, by event ID, for showing its contents.
    pub events: HashMap<u64, Event>,
    /// The inference's reported usage; its input tokens measure this context.
    pub usage: Usage,
    pub model_config: ModelConfig,
}

fn build_label(text: &str, fallback: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback);
    match line.char_indices().nth(LABEL_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

fn json_size(value: &impl serde::Serialize) -> u64 {
    serde_json::to_string(value).map_or(0, |json| json.len() as u64)
}

fn content_size(content: &[ContentBlock]) -> u64 {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.len() as u64,
            ContentBlock::Refusal(refusal) => refusal.text.len() as u64,
            ContentBlock::Audio(audio) => audio
                .transcript
                .as_ref()
                .map_or(MEDIA_BLOCK_CHARS, |text| text.len() as u64),
            ContentBlock::Image(_) | ContentBlock::File(_) => MEDIA_BLOCK_CHARS,
            ContentBlock::ProviderContent(content) => json_size(&content.raw),
        })
        .sum()
}

fn first_text(content: &[ContentBlock]) -> &str {
    content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

fn history_item_size(item: &HistoryItem) -> u64 {
    json_size(&item.kind)
}

/// Describe one event's contribution to the model's context, if it has any.
fn build_context_item(event: &Event, tool_names: &HashMap<&str, &str>) -> Option<ContextItem> {
    let (category, label, size) = match &event.kind {
        EventType::Message(message) => {
            let (category, fallback) = match message.message_type {
                MessageType::System => (ContextCategory::System, "System prompt"),
                MessageType::Human => (ContextCategory::User, "User message"),
                MessageType::Agent => (ContextCategory::Agent, "Agent message"),
                MessageType::Harness => (ContextCategory::Harness, "Harness message"),
                MessageType::Subagent => (ContextCategory::Harness, "Subagent message"),
            };
            let label = match (&message.description, message.message_type) {
                (Some(description), MessageType::System) => build_label(description, fallback),
                _ => build_label(first_text(&message.content), fallback),
            };
            (category, label, content_size(&message.content))
        }
        EventType::Thinking(thinking) => {
            let summary: u64 = thinking
                .contents
                .summary
                .iter()
                .map(|text| text.len() as u64)
                .sum();
            let opaque = thinking
                .contents
                .opaque_state
                .as_ref()
                .map_or(0, |state| json_size(&state.data));
            let preview = thinking.contents.summary.first().map_or("", String::as_str);
            (
                ContextCategory::Reasoning,
                build_label(preview.trim_matches(['*', '#', ' ']), "Reasoning"),
                summary + opaque,
            )
        }
        EventType::ToolDefinition(tool) => (
            ContextCategory::ToolDefinitions,
            format!("{} definition", tool.definition.name),
            json_size(&tool.definition),
        ),
        EventType::ToolCall(call) => (
            ContextCategory::ToolCalls,
            format!("{} call", call.tool_name),
            (call.tool_name.len() + call.arguments.len()) as u64,
        ),
        EventType::ToolResult(result) => {
            let label = match tool_names.get(result.call_id.as_str()) {
                Some(name) => format!("{name} result"),
                None => "Tool result".to_owned(),
            };
            (
                ContextCategory::ToolResults,
                label,
                content_size(&result.content),
            )
        }
        EventType::Compaction(compaction) => {
            let size = match &compaction.summary {
                CompactionSummary::Text(summary) => summary.len() as u64,
                CompactionSummary::ProviderItem(item) => history_item_size(item),
            };
            (ContextCategory::Harness, "Context summary".to_owned(), size)
        }
        EventType::ProviderItem(item) => (
            ContextCategory::Harness,
            "Provider-native item".to_owned(),
            history_item_size(item),
        ),
        _ => return None,
    };
    Some(ContextItem {
        event_id: event.id.0,
        category,
        label,
        size,
    })
}

/// Reconstruct what one inference was sent from an agent's ordered events.
///
/// `turn` is the event ID of the inference's end. The events must reach back
/// to the start of its context, as a `from_context_start` events query does:
/// the context spans from the last preceding `NewContext`, or else the first
/// event, to the inference's start.
pub fn build_context_makeup(events: &[Event], turn: u64) -> Option<ContextMakeup> {
    let end_index = events.iter().position(|event| event.id.0 == turn)?;
    let EventType::InferenceResultEnd(end) = &events[end_index].kind else {
        return None;
    };
    let (start_index, start) =
        events[..end_index]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match &event.kind {
                EventType::InferenceResultStart(start) if start.id == end.id => {
                    Some((index, start))
                }
                _ => None,
            })?;
    let context_start = events[..start_index]
        .iter()
        .rposition(|event| matches!(event.kind, EventType::NewContext));
    let context = &events[context_start.map_or(0, |index| index + 1)..start_index];

    let tool_names: HashMap<_, _> = context
        .iter()
        .filter_map(|event| match &event.kind {
            EventType::ToolCall(call) => Some((call.call_id.as_str(), call.tool_name.as_str())),
            _ => None,
        })
        .collect();

    let mut items = Vec::new();
    let mut item_events = HashMap::new();
    for event in context {
        let Some(item) = build_context_item(event, &tool_names) else {
            continue;
        };
        items.push(item);
        item_events.insert(event.id.0, event.clone());
    }
    Some(ContextMakeup {
        items,
        events: item_events,
        usage: end.usage.clone(),
        model_config: start.model_config.clone(),
    })
}

impl ContextMakeup {
    pub fn total_size(&self) -> u64 {
        self.items.iter().map(|item| item.size).sum()
    }

    /// Scale an estimated size to the provider's reported input tokens.
    pub fn estimate_tokens(&self, size: u64) -> Option<u64> {
        let total = self.total_size();
        let tokens = self.usage.input_tokens?;
        if total == 0 {
            return None;
        }
        Some((size as u128 * tokens as u128 / total as u128) as u64)
    }

    /// Totals for every category, in category order, including empty ones.
    pub fn category_totals(&self) -> [CategoryTotal; 8] {
        let mut totals = ContextCategory::ALL.map(|category| CategoryTotal {
            category,
            items: 0,
            size: 0,
        });
        for item in &self.items {
            let total = &mut totals[item.category.index()];
            total.items += 1;
            total.size += item.size;
        }
        totals
    }

    /// Items arranged as the diagram is: by position, by size, or by category
    /// (largest category first) and then size.
    pub fn sorted_items(&self, sort: ContextSort) -> Vec<&ContextItem> {
        let mut items: Vec<_> = self.items.iter().collect();
        match sort {
            ContextSort::Order => {}
            ContextSort::Size => items.sort_by_key(|item| std::cmp::Reverse(item.size)),
            ContextSort::Type => {
                let totals = self.category_totals();
                items.sort_by_key(|item| {
                    (
                        std::cmp::Reverse(totals[item.category.index()].size),
                        item.category,
                        std::cmp::Reverse(item.size),
                    )
                });
            }
        }
        items
    }

    pub fn segments(&self, sort: ContextSort) -> Vec<ContextSegment> {
        match sort {
            ContextSort::Order => self.build_order_segments(),
            ContextSort::Size => self.build_size_segments(),
            ContextSort::Type => {
                let mut totals = self.category_totals().to_vec();
                totals.retain(|total| total.size > 0);
                totals.sort_by_key(|total| std::cmp::Reverse(total.size));
                let items = self.sorted_items(ContextSort::Type);
                totals
                    .into_iter()
                    .map(|total| ContextSegment {
                        category: Some(total.category),
                        label: total.category.label().to_owned(),
                        items: total.items,
                        size: total.size,
                        first_item: items
                            .iter()
                            .find(|item| item.category == total.category)
                            .map_or(0, |item| item.event_id),
                    })
                    .collect()
            }
        }
    }

    fn build_size_segments(&self) -> Vec<ContextSegment> {
        let items = self.sorted_items(ContextSort::Size);
        let (largest, rest) = items.split_at(items.len().min(SIZE_SEGMENTS));
        let mut segments: Vec<_> = largest
            .iter()
            .filter(|item| item.size > 0)
            .map(|item| ContextSegment {
                category: Some(item.category),
                label: item.label.clone(),
                items: 1,
                size: item.size,
                first_item: item.event_id,
            })
            .collect();

        let rest_size: u64 = rest.iter().map(|item| item.size).sum();
        if rest_size > 0 {
            segments.push(ContextSegment {
                category: None,
                label: format!("{} smaller items", rest.len()),
                items: rest.len(),
                size: rest_size,
                first_item: rest.first().map_or(0, |item| item.event_id),
            });
        }
        segments
    }

    /// Consecutive items of one category form a run. Runs too narrow to see
    /// pool with their neighbors under the pool's heaviest category, which
    /// bounds the number of segments however long the context is.
    fn build_order_segments(&self) -> Vec<ContextSegment> {
        let threshold = (self.total_size() / ORDER_RESOLUTION).max(1);
        let mut segments: Vec<ContextSegment> = Vec::new();
        let mut pool = SegmentPool::default();

        let mut runs = self
            .items
            .chunk_by(|left, right| left.category == right.category);
        loop {
            let run = runs.next();
            let run_size = run.map_or(0, |run| run.iter().map(|item| item.size).sum());
            let is_wide = run.is_some() && run_size >= threshold;
            if pool.items > 0 && (run.is_none() || is_wide || pool.size >= threshold) {
                push_merged(&mut segments, std::mem::take(&mut pool).into_segment());
            }
            let Some(run) = run else {
                break;
            };
            if !is_wide {
                pool.add(run);
                continue;
            }

            let category = run[0].category;
            push_merged(
                &mut segments,
                ContextSegment {
                    category: Some(category),
                    label: match run {
                        [item] => item.label.clone(),
                        _ => category.label().to_owned(),
                    },
                    items: run.len(),
                    size: run_size,
                    first_item: run[0].event_id,
                },
            );
        }
        segments
    }
}

#[derive(Default)]
struct SegmentPool {
    sizes: [u64; 8],
    items: usize,
    size: u64,
    first_item: u64,
}

impl SegmentPool {
    fn add(&mut self, run: &[ContextItem]) {
        if self.items == 0 {
            self.first_item = run[0].event_id;
        }
        for item in run {
            self.sizes[item.category.index()] += item.size;
            self.size += item.size;
        }
        self.items += run.len();
    }

    fn into_segment(self) -> ContextSegment {
        let category = ContextCategory::ALL
            .into_iter()
            .max_by_key(|category| self.sizes[category.index()])
            .unwrap_or(ContextCategory::System);
        ContextSegment {
            category: Some(category),
            label: format!("Mostly {}", category.label().to_lowercase()),
            items: self.items,
            size: self.size,
            first_item: self.first_item,
        }
    }
}

/// Append a segment, joining it to a preceding one of the same category.
fn push_merged(segments: &mut Vec<ContextSegment>, segment: ContextSegment) {
    if segment.size == 0 {
        return;
    }
    match segments.last_mut() {
        Some(last) if last.category == segment.category => {
            if last.label != segment.label {
                last.label = segment
                    .category
                    .map_or_else(|| segment.label.clone(), |c| c.label().to_owned());
            }
            last.items += segment.items;
            last.size += segment.size;
        }
        _ => segments.push(segment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::{Outcome, Reasoning, RequestConfig, StopReason};
    use llm_api::providers::{Effort, Model, ProviderType};
    use psa_api::AgentId;
    use psa_api::events::{
        EventId, InferenceResultEnd, InferenceResultStart, Message, Thinking, ToolCall, ToolResult,
    };

    fn config() -> ModelConfig {
        ModelConfig {
            model: Model::new(ProviderType::Openrouter, "test").unwrap(),
            effort: Effort::High,
        }
    }

    fn build_events(kinds: Vec<EventType>) -> Vec<Event> {
        kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| Event {
                id: EventId(index as u64 + 1),
                source_agent_id: AgentId(1),
                timestamp: time::OffsetDateTime::UNIX_EPOCH,
                kind,
            })
            .collect()
    }

    fn message(role: MessageType, text: &str) -> EventType {
        EventType::Message(Message::text(role, text))
    }

    fn call(id: &str, tool: &str, arguments: &str) -> EventType {
        EventType::ToolCall(ToolCall {
            call_id: id.into(),
            tool_name: tool.into(),
            arguments: arguments.into(),
            metadata: Default::default(),
        })
    }

    fn result(id: &str, text: &str) -> EventType {
        EventType::ToolResult(ToolResult {
            delivery: None,
            call_id: id.into(),
            error: false,
            metadata: Default::default(),
            content: Message::text(MessageType::Agent, text).content,
        })
    }

    fn start(id: u64) -> EventType {
        EventType::InferenceResultStart(InferenceResultStart {
            id,
            model_config: config(),
            request_config: RequestConfig::default(),
        })
    }

    fn end(id: u64, input_tokens: Option<u64>) -> EventType {
        EventType::InferenceResultEnd(InferenceResultEnd {
            id,
            outcome: Outcome::Completed {
                reason: StopReason::ToolCalls,
            },
            usage: Usage {
                input_tokens,
                ..Default::default()
            },
            request_time: std::time::Duration::ZERO,
            response_id: None,
            provider_metadata: Default::default(),
        })
    }

    fn two_turn_events() -> Vec<Event> {
        build_events(vec![
            message(MessageType::Human, "discarded by the new context"),
            EventType::NewContext,
            EventType::Message(Message::system("s".repeat(100), "Base instructions")),
            message(MessageType::Human, &"u".repeat(50)),
            start(5),
            EventType::Thinking(Thinking {
                contents: Reasoning {
                    summary: vec!["**Plan**".into()],
                    opaque_state: None,
                },
                metadata: Default::default(),
            }),
            call("c1", "read", &"a".repeat(16)),
            end(5, Some(600)),
            result("c1", &"r".repeat(222)),
            start(10),
            message(MessageType::Agent, "output of the second turn"),
            end(10, Some(1000)),
        ])
    }

    #[test]
    fn makeup_spans_the_context_before_the_turn_and_excludes_its_own_output() {
        let events = two_turn_events();
        let first = build_context_makeup(&events, 8).unwrap();
        assert_eq!(first.model_config, config());
        let labels: Vec<_> = first
            .items
            .iter()
            .map(|item| (item.category, item.label.as_str(), item.size))
            .collect();
        assert_eq!(
            labels,
            [
                (ContextCategory::System, "Base instructions", 100),
                (ContextCategory::User, &*"u".repeat(50), 50),
            ]
        );
        assert_eq!(first.estimate_tokens(50), Some(200));

        let second = build_context_makeup(&events, 12).unwrap();
        let labels: Vec<_> = second
            .items
            .iter()
            .map(|item| (item.event_id, item.label.as_str(), item.size))
            .collect();
        assert_eq!(
            labels[2..],
            [
                (6, "Plan", 8),
                (7, "read call", 20),
                (9, "read result", 222)
            ]
        );
        assert_eq!(second.total_size(), 400);
        assert!(matches!(second.events[&9].kind, EventType::ToolResult(_)));
        assert_eq!(second.events.len(), second.items.len());
        assert_eq!(second.estimate_tokens(222), Some(555));
        let totals = second.category_totals();
        assert_eq!(totals[ContextCategory::ToolResults.index()].items, 1);
        assert_eq!(totals[ContextCategory::Agent.index()].size, 0);
    }

    #[test]
    fn makeup_requires_a_loaded_inference_with_its_start() {
        let events = two_turn_events();
        assert!(build_context_makeup(&events, 11).is_none());
        assert!(build_context_makeup(&events, 99).is_none());
        assert!(build_context_makeup(&events[5..], 8).is_none());

        // Without a NewContext marker the context begins with the first event.
        let makeup = build_context_makeup(&events[2..], 12).unwrap();
        assert_eq!(makeup.items.len(), 5);
        let unreported = build_events(vec![start(1), end(1, None)]);
        let makeup = build_context_makeup(&unreported, 2).unwrap();
        assert_eq!(makeup.estimate_tokens(10), None);
        assert!(makeup.segments(ContextSort::Order).is_empty());
    }

    fn makeup_of(items: &[(ContextCategory, u64)]) -> ContextMakeup {
        ContextMakeup {
            items: items
                .iter()
                .enumerate()
                .map(|(index, (category, size))| ContextItem {
                    event_id: index as u64,
                    category: *category,
                    label: format!("item {index}"),
                    size: *size,
                })
                .collect(),
            events: HashMap::new(),
            usage: Usage::default(),
            model_config: config(),
        }
    }

    fn summarize(segments: &[ContextSegment]) -> Vec<(Option<ContextCategory>, usize, u64)> {
        segments
            .iter()
            .map(|segment| (segment.category, segment.items, segment.size))
            .collect()
    }

    #[test]
    fn sorts_arrange_items_and_segments_by_order_size_and_type() {
        use ContextCategory::*;
        let makeup = makeup_of(&[
            (System, 300),
            (ToolCalls, 100),
            (ToolResults, 500),
            (ToolResults, 200),
            (ToolCalls, 150),
            (User, 0),
        ]);
        assert_eq!(
            summarize(&makeup.segments(ContextSort::Order)),
            [
                (Some(System), 1, 300),
                (Some(ToolCalls), 1, 100),
                (Some(ToolResults), 2, 700),
                (Some(ToolCalls), 1, 150),
            ]
        );
        assert_eq!(makeup.segments(ContextSort::Order)[0].label, "item 0");
        assert_eq!(makeup.segments(ContextSort::Order)[2].label, "Tool results");

        let sizes = |sort| -> Vec<u64> {
            makeup
                .sorted_items(sort)
                .iter()
                .map(|item| item.size)
                .collect()
        };
        assert_eq!(sizes(ContextSort::Order), [300, 100, 500, 200, 150, 0]);
        assert_eq!(sizes(ContextSort::Size), [500, 300, 200, 150, 100, 0]);
        assert_eq!(sizes(ContextSort::Type), [500, 200, 300, 150, 100, 0]);
        assert_eq!(
            summarize(&makeup.segments(ContextSort::Type)),
            [
                (Some(ToolResults), 2, 700),
                (Some(System), 1, 300),
                (Some(ToolCalls), 2, 250),
            ]
        );
        assert_eq!(makeup.segments(ContextSort::Size).len(), 5);
    }

    #[test]
    fn segments_name_the_first_item_they_cover_in_each_arrangement() {
        use ContextCategory::*;
        let makeup = makeup_of(&[
            (System, 300),
            (ToolResults, 200),
            (ToolResults, 500),
            (System, 50),
        ]);
        let first_items = |sort| -> Vec<u64> {
            makeup
                .segments(sort)
                .iter()
                .map(|segment| segment.first_item)
                .collect()
        };
        assert_eq!(first_items(ContextSort::Order), [0, 1, 3]);
        assert_eq!(first_items(ContextSort::Size), [2, 0, 1, 3]);
        assert_eq!(first_items(ContextSort::Type), [2, 0]);

        // A merged remainder starts at its first item in size order.
        let mut items = vec![(System, 1_000); SIZE_SEGMENTS];
        items.extend([(User, 5), (User, 3)]);
        let makeup = makeup_of(&items);
        let segments = makeup.segments(ContextSort::Size);
        assert_eq!(segments.last().unwrap().first_item, SIZE_SEGMENTS as u64);
    }

    #[test]
    fn long_contexts_stay_bounded_and_conserve_their_total() {
        use ContextCategory::*;
        let mut items = vec![(System, 40_000)];
        for _ in 0..5_000 {
            items.push((ToolCalls, 3));
            items.push((ToolResults, 9));
        }
        items.push((User, 20_000));
        let makeup = makeup_of(&items);
        let total = makeup.total_size();

        let order = makeup.segments(ContextSort::Order);
        assert_eq!(
            summarize(&order),
            [
                (Some(System), 1, 40_000),
                (Some(ToolResults), 10_000, 60_000),
                (Some(User), 1, 20_000),
            ]
        );
        assert_eq!(order[1].label, "Mostly tool results");

        let size = makeup.segments(ContextSort::Size);
        assert_eq!(size.len(), SIZE_SEGMENTS + 1);
        assert_eq!(size.last().unwrap().category, None);
        assert_eq!(size.last().unwrap().items, items.len() - SIZE_SEGMENTS);
        for segments in [order, size, makeup.segments(ContextSort::Type)] {
            assert_eq!(segments.iter().map(|s| s.size).sum::<u64>(), total);
        }

        // Alternating wide runs stay distinct however many there are.
        let striped: Vec<_> = (0..40)
            .map(|index| (if index % 2 == 0 { Agent } else { User }, 1_000))
            .collect();
        assert_eq!(makeup_of(&striped).segments(ContextSort::Order).len(), 40);
    }
}
