//! Pi's standalone summarization prompts. Conversation text is data, never live history.
//! Adapted from earendil-works/pi (MIT); see docs/PI_LICENSE.txt.

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages.";

pub(super) fn history_prompt(conversation: &str, previous_summary: Option<&str>) -> String {
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(summary) = previous_summary.filter(|summary| !summary.is_empty()) {
        prompt.push_str(&format!(
            "<previous-summary>\n{summary}\n</previous-summary>\n\n"
        ));
        prompt.push_str(UPDATE_SUMMARIZATION_PROMPT);
    } else {
        prompt.push_str(SUMMARIZATION_PROMPT);
    }
    prompt
}

/// Appended after the standard instructions, so the requested format still applies.
pub(super) fn additional_instructions(instructions: &str) -> String {
    format!("\n\nAdditional instructions for this summary:\n{instructions}")
}

pub(super) fn turn_prefix_prompt(conversation: &str) -> String {
    format!("# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_and_update_prompts_wrap_transcripts_as_data() {
        let initial = history_prompt("[User]: hello", None);
        assert!(initial.starts_with("<conversation>\n[User]: hello\n</conversation>\n\n"));
        assert!(initial.ends_with(SUMMARIZATION_PROMPT));
        assert!(!initial.contains("<previous-summary>"));
        let updated = history_prompt("new", Some("old"));
        assert!(
            updated.contains("</conversation>\n\n<previous-summary>\nold\n</previous-summary>\n\n")
        );
        assert!(updated.ends_with(UPDATE_SUMMARIZATION_PROMPT));
        assert_eq!(history_prompt("new", Some("")), history_prompt("new", None));
    }

    #[test]
    fn split_prompt_uses_the_current_checkpoint_instructions() {
        let prompt = turn_prefix_prompt("prefix");
        assert!(prompt.starts_with("# Conversation\nprefix\n\n# Instructions\n"));
        assert!(prompt.ends_with("Only summarize information explicitly present above. Do not infer or recreate later messages."));
        assert!(SUMMARIZATION_SYSTEM_PROMPT.contains("Do NOT continue the conversation."));
    }
}
