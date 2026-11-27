use super::*;

#[test]
fn short_messages_name_the_session_as_written() {
    assert_eq!(name_from_message("Fix the tests"), "Fix the tests");
    assert_eq!(
        name_from_message("\n\n  Fix   the\ttests  \nmore"),
        "Fix the tests"
    );
    assert_eq!(name_from_message(""), "");
}

#[test]
fn long_messages_are_cut_at_a_word() {
    let name = name_from_message(
        "Fix the flaky upload test in CI and also look at why the retry loop never ends",
    );
    assert_eq!(name, "Fix the flaky upload test in CI and also look at…");
    assert!(name.chars().count() <= MESSAGE_NAME_LENGTH + 1);

    let word = "x".repeat(60);
    assert_eq!(
        name_from_message(&word),
        format!("{}…", "x".repeat(MESSAGE_NAME_LENGTH))
    );
}
