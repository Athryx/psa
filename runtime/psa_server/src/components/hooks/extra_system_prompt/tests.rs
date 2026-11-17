use crate::components::hooks::HookKind;
use psa::api::HookConfig;

#[test]
fn construction_owns_the_configured_prompt() {
    let mut config = HookConfig::ExtraSystemPrompt {
        prompt: "First configured prompt".into(),
    };
    let HookKind::ExtraSystemPrompt(first) = HookKind::from_config(&config) else {
        panic!("extra system prompt config must construct its hook");
    };
    let HookConfig::ExtraSystemPrompt { prompt } = &mut config else {
        unreachable!();
    };
    *prompt = "Second configured prompt".into();
    let HookKind::ExtraSystemPrompt(second) = HookKind::from_config(&config) else {
        panic!("extra system prompt config must construct its hook");
    };

    assert_eq!(first.prompt, "First configured prompt");
    assert_eq!(second.prompt, "Second configured prompt");
}
