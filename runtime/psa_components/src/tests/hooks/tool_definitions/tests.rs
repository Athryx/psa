use super::*;
use crate::tools::{Tool, tool_definition};
use llm_api::api::ContentBlock;
use psa::api::events::MessageType;

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
        ToolId::ListSessions,
        tool_definition(ToolId::ListSessions).unwrap(),
        ToolSystemPrompt {
            tools: Some("Do nothing.".into()),
            rules: vec!["Keep changes focused.".into(), "Use absolute paths.".into()],
        },
    )]);
    assert_eq!(events.len(), 3);
    assert!(
        matches!(&events[1].kind, EventType::Message(message) if message.description.as_deref() == Some("Tool usage"))
    );
    assert!(
        matches!(&events[2].kind, EventType::Message(message) if message.description.as_deref() == Some("Tool rules"))
    );
    assert!(
        matches!(&events[0].kind, EventType::ToolDefinition(snapshot) if snapshot.id == ToolId::ListSessions)
    );
    assert_eq!(
        system_text(&events[1]),
        "<tools>\n- list_sessions: Do nothing.\n</tools>"
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
            "<tools>\n- list_sessions: A tool direction.\n</tools>",
        ),
    ] {
        let events = build_tool_events([(
            ToolId::ListSessions,
            tool_definition(ToolId::ListSessions).unwrap(),
            prompt,
        )]);
        assert_eq!(events.len(), 2);
        assert_eq!(system_text(&events[1]), expected);
    }
}

#[test]
fn codemode_history_guidance_is_published_in_the_rules_system_message() {
    use crate::tools::CodeMode;
    use psa_codemode::MontyEngine;
    use std::sync::Arc;

    let tool = CodeMode::new(Arc::new(MontyEngine::new("monty")));
    let prompt = tool.system_prompt();
    let rule = prompt.rules[0].clone();
    let events = build_tool_events([(ToolId::CodeMode, tool.tool_definition(), prompt)]);
    assert_eq!(events.len(), 3);
    assert!(system_text(&events[1]).starts_with("<tools>"));
    let rules = system_text(&events[2]);
    assert!(rules.starts_with("<rules>"));
    assert!(rules.contains(&rule));
    assert!(rules.contains("prior_results(-1)[6000:18000]"));
    assert!(rules.contains("instead of rerunning"));
}

#[test]
fn default_guidance_keeps_only_the_tool_snapshot() {
    let events = build_tool_events([(
        ToolId::ListSessions,
        tool_definition(ToolId::ListSessions).unwrap(),
        ToolSystemPrompt::default(),
    )]);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].kind, EventType::ToolDefinition(_)));
    assert!(build_tool_events([]).is_empty());
}
