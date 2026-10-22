use super::*;

#[test]
fn middle_truncation_borrows_text_within_budget() {
    assert!(matches!(truncate_middle("text", 4), Cow::Borrowed("text")));
    assert_eq!(truncate_middle("", 0), "");
}

#[test]
fn middle_truncation_preserves_both_ends_and_counts_marker_bytes() {
    let input = format!("BEGIN{}END", "x".repeat(100));
    let output = truncate_middle(&input, 40);
    assert!(output.starts_with("BEGIN"));
    assert!(output.ends_with("END"));
    assert!(output.contains("[... middle truncated ...]"));
    assert_eq!(output.len(), 40);
    assert_eq!(truncate_middle("abcdef", 5), "a...f");
    assert_eq!(truncate_middle("abcdef", 2), "..");
    assert_eq!(truncate_middle("abcdef", 0), "");
}

#[test]
fn middle_truncation_never_splits_utf8_or_exceeds_budget() {
    let input = "🙂 начало e\u{301} 終わり 🚀".repeat(4);
    for budget in 0..=input.len() + 1 {
        let output = truncate_middle(&input, budget);
        assert!(output.len() <= budget);
        if input.len() <= budget {
            assert_eq!(output, input);
        }
    }
}

#[test]
fn argument_errors_are_bounded_with_field_path_and_retry_guidance() {
    let message = tool_argument_error("entries[2].count: expected an unsigned integer");
    let [ContentBlock::Text(block)] = message.as_slice() else {
        panic!("expected text");
    };
    assert!(block.text.contains("entries[2].count"));
    assert!(block.text.contains("retry"));

    let message = tool_argument_error(&"bad argument ".repeat(1000));
    let [ContentBlock::Text(block)] = message.as_slice() else {
        panic!("expected text");
    };
    assert!(block.text.len() <= 2048);
    assert!(block.text.contains("middle truncated"));
}
