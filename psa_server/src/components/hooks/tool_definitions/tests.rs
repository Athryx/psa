use super::*;
use crate::components::tools::tool_definition;
use llm_api::api::ContentBlock;

fn system_text(event: &NewEvent) -> &str {
    let EventType::Message(message) = &event.kind else {
        panic!("expected a system message, got {:?}", event.kind);
    };
    assert_eq!(message.message_type, MessageType::System);
    let [ContentBlock::Text(block)] = message.content.as_slice() else {
        panic!("expected one text block");
    };
    &block.text
}

#[test]
fn snapshots_are_followed_by_two_separate_system_prompt_events() {
    let events = build_tool_events([(
        ToolId::Noop,
        tool_definition(ToolId::Noop),
        ToolSystemPrompt {
            tools: Some("Do nothing.".into()),
            rules: vec!["Keep changes focused.".into(), "Use absolute paths.".into()],
        },
    )]);
    assert_eq!(events.len(), 3);
    assert!(
        matches!(&events[0].kind, EventType::ToolDefinition(snapshot) if snapshot.id == ToolId::Noop)
    );
    assert_eq!(
        system_text(&events[1]),
        "<tools>\n- noop: Do nothing.\n</tools>"
    );
    assert_eq!(
        system_text(&events[2]),
        "<rules>\n- Keep changes focused.\n- Use absolute paths.\n</rules>"
    );
}

#[test]
fn rules_and_tool_directions_are_independent() {
    for (prompt, expected) in [
        (
            ToolSystemPrompt {
                tools: None,
                rules: vec!["A shared rule.".into()],
            },
            "<rules>\n- A shared rule.\n</rules>",
        ),
        (
            ToolSystemPrompt {
                tools: Some("A tool direction.".into()),
                rules: vec![],
            },
            "<tools>\n- noop: A tool direction.\n</tools>",
        ),
    ] {
        let events = build_tool_events([(ToolId::Noop, tool_definition(ToolId::Noop), prompt)]);
        assert_eq!(events.len(), 2);
        assert_eq!(system_text(&events[1]), expected);
    }
}

#[test]
fn default_guidance_keeps_only_the_tool_snapshot() {
    let events = build_tool_events([(
        ToolId::Noop,
        tool_definition(ToolId::Noop),
        ToolSystemPrompt::default(),
    )]);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].kind, EventType::ToolDefinition(_)));
    assert!(build_tool_events([]).is_empty());
}
