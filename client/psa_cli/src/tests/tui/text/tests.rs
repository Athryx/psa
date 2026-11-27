use super::*;

#[test]
fn wraps_at_word_boundaries_and_keeps_blank_lines() {
    assert_eq!(
        wrap_text("alpha beta gamma\n\ndelta", 11),
        vec!["alpha beta", "gamma", "", "delta"]
    );
}

#[test]
fn splits_words_longer_than_the_line() {
    assert_eq!(
        wrap_text("abcdefghij xy", 4),
        vec!["abcd", "efgh", "ij", "xy"]
    );
}

#[test]
fn counts_wide_characters_as_two_columns() {
    assert_eq!(wrap_text("日本語です", 4), vec!["日本", "語で", "す"]);
}

#[test]
fn truncates_to_one_line_with_an_ellipsis() {
    assert_eq!(truncate_line("short", 10), "short");
    assert_eq!(truncate_line("a longer line", 8), "a longe…");
    assert_eq!(truncate_line("first\nsecond", 20), "first…");
}
