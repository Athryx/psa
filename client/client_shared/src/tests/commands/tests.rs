use super::*;

#[test]
fn sandbox_command_parses_actions() {
    assert_eq!(
        parse_composer_input("/sandbox stop", Frontend::Tui),
        ComposerInput::Command {
            id: CommandId::Sandbox,
            arguments: "stop".into()
        }
    );
    assert_eq!(SandboxAction::parse("stop"), Some(SandboxAction::Stop));
    assert_eq!(SandboxAction::parse(" Start "), Some(SandboxAction::Start));
    assert_eq!(SandboxAction::parse(""), None);
    assert_eq!(SandboxAction::parse("restart"), None);
    assert!(CommandId::Sandbox.is_available(CommandContext::DraftRoot));
    assert!(!CommandId::Sandbox.is_available(CommandContext::NoSession));
}

#[test]
fn commands_are_not_model_prompts_and_literals_are_preserved() {
    assert_eq!(
        parse_composer_input("/model provider/model", Frontend::Gui),
        ComposerInput::Command {
            id: CommandId::SessionModel,
            arguments: "provider/model".into()
        }
    );
    assert_eq!(
        parse_composer_input("//model", Frontend::Gui),
        ComposerInput::Text("/model".into())
    );
    assert_eq!(
        parse_composer_input("/unknown", Frontend::Gui),
        ComposerInput::Text("/unknown".into())
    );
    assert_eq!(complete_commands("/mo", Frontend::Gui).len(), 1);
    assert!(complete_commands("//mo", Frontend::Gui).is_empty());
}

#[test]
fn select_commands_are_distinct_from_agents_and_probes() {
    assert_eq!(
        parse_composer_input("/select agent  worker  ", Frontend::Gui),
        ComposerInput::Command {
            id: CommandId::SelectAgent,
            arguments: "worker".into(),
        }
    );
    assert_eq!(
        parse_composer_input("/select probe", Frontend::Gui),
        ComposerInput::Text("/select probe".into())
    );
    assert_eq!(
        parse_composer_input("/agents", Frontend::Gui),
        ComposerInput::Command {
            id: CommandId::Agents,
            arguments: String::new(),
        }
    );
    assert_eq!(
        parse_composer_input("/select probe builder", Frontend::Tui),
        ComposerInput::Command {
            id: CommandId::SelectProbe,
            arguments: "builder".into(),
        }
    );
    assert_eq!(
        parse_composer_input("/select", Frontend::Tui),
        ComposerInput::Text("/select".into())
    );
    assert!(complete_commands("/select agent worker", Frontend::Gui).is_empty());
    let names: Vec<_> = complete_commands("/select ", Frontend::Tui)
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["select agent", "select probe"]);
}

#[test]
fn an_exact_name_completes_first() {
    let names: Vec<_> = complete_commands("/agents", Frontend::Gui)
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["agents"]);
    let names: Vec<_> = complete_commands("/pinned-models", Frontend::Gui)
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["pinned-models"]);
    let names: Vec<_> = complete_commands("/s", Frontend::Tui)
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(
        names,
        [
            "select agent",
            "sandbox",
            "sessions",
            "select probe",
            "server",
            "settings"
        ]
    );
}

#[test]
fn every_command_runs_in_both_clients_except_probe() {
    for spec in COMMANDS {
        let both = spec.supports(Frontend::Gui) && spec.supports(Frontend::Tui);
        assert_eq!(both, spec.id != CommandId::SelectProbe, "/{}", spec.name);
    }
}

#[test]
fn scopes_follow_the_composer_context() {
    use CommandContext::*;
    assert!(CommandId::Compact.is_available(Agent));
    assert!(!CommandId::Compact.is_available(DraftRoot));
    assert!(!CommandId::Goal.is_available(DraftRoot));
    assert!(!CommandId::Fork.is_available(DraftRoot));
    assert!(CommandId::SelectAgent.is_available(DraftRoot));
    assert!(CommandId::SelectAgent.is_available(SessionDraft));
    assert!(!CommandId::SelectAgent.is_available(Agent));
    assert!(CommandId::SelectProbe.is_available(SessionDraft));
    assert!(!CommandId::SelectProbe.is_available(DraftRoot));
    for id in [
        CommandId::SessionModel,
        CommandId::Rename,
        CommandId::Info,
        CommandId::Refresh,
    ] {
        assert!(id.is_available(SessionDraft), "{id:?}");
    }
    for id in [CommandId::Sandbox, CommandId::Agents, CommandId::Compact] {
        assert!(!id.is_available(SessionDraft), "{id:?} needs a session");
    }
    assert!(CommandId::Rename.is_available(DraftRoot) && CommandId::Rename.is_available(Agent));
    assert!(!CommandId::Rename.is_available(NoSession));
    assert!(CommandId::Sessions.is_available(NoSession));
    assert!(CommandId::NewSession.is_available(NoSession));
    assert!(!CommandId::Agents.is_available(NoSession));
}

#[test]
fn only_commands_that_change_the_agent_or_session_are_saved() {
    let saved: Vec<_> = COMMANDS
        .iter()
        .filter(|spec| spec.transcript != TranscriptRecord::No)
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        saved,
        [
            "model", "refresh", "effort", "compact", "goal", "rename", "fork", "sandbox"
        ]
    );
    assert_eq!(CommandId::Goal.spec().transcript, TranscriptRecord::Server);
    assert_eq!(
        CommandId::Compact.spec().transcript,
        TranscriptRecord::Server
    );
}

#[test]
fn records_are_built_only_for_client_saved_commands() {
    assert!(build_command_record(CommandId::Info, "", Ok(())).is_none());
    assert!(build_command_record(CommandId::Compact, "", Ok(())).is_none());
    let record = build_command_record(CommandId::Rename, " new name ", Err("no".into())).unwrap();
    assert_eq!(record.name, "rename");
    assert_eq!(record.args, "new name");
    assert_eq!(describe_command(&record), "/rename new name\nFailed\nno");
}

#[test]
fn completions_and_usage_describe_arguments() {
    assert_eq!(CommandId::Refresh.spec().completion(), "/refresh");
    assert_eq!(CommandId::NewSession.spec().completion(), "/new");
    assert_eq!(CommandId::Compact.spec().completion(), "/compact ");
    assert_eq!(CommandId::Compact.spec().usage(), "/compact [focus]");
    assert_eq!(CommandId::Sandbox.spec().usage(), "/sandbox start | stop");
    assert_eq!(
        CommandId::Goal.spec().usage(),
        "/goal [new goal | pause [reason] | resume | clear [reason]]"
    );
}

#[test]
fn compact_goal_and_fork_keep_their_text() {
    assert_eq!(
        parse_composer_input("/compact focus on tests", Frontend::Gui),
        ComposerInput::Command {
            id: CommandId::Compact,
            arguments: "focus on tests".into(),
        }
    );
    assert_eq!(
        parse_composer_input("/goal pause waiting on review", Frontend::Tui),
        ComposerInput::Command {
            id: CommandId::Goal,
            arguments: "pause waiting on review".into(),
        }
    );
    assert_eq!(
        parse_composer_input("/fork try another approach", Frontend::Gui),
        ComposerInput::Command {
            id: CommandId::Fork,
            arguments: "try another approach".into(),
        }
    );
}

#[test]
fn selection_wraps_and_stays_in_range() {
    let mut selection = CommandSelection::default();
    selection.next(0);
    selection.previous(0);
    assert_eq!(selection.index(0), 0);
    selection.previous(3);
    assert_eq!(selection.index(3), 2);
    assert_eq!(selection.index(1), 0);
    selection.dismiss();
    assert!(selection.dismissed);
    selection.reset();
    assert!(!selection.dismissed && selection.selected == 0);
}
