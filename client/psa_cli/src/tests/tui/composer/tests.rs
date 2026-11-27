use super::*;

#[test]
fn edits_on_character_boundaries() {
    let mut composer = Composer::default();
    for character in "héllo".chars() {
        composer.insert_char(character);
    }
    composer.move_left();
    composer.move_left();
    composer.move_left();
    composer.move_left();
    composer.delete_before();
    assert_eq!(composer.text(), "éllo");
    composer.delete_after();
    assert_eq!(composer.text(), "llo");
}

#[test]
fn deletes_the_previous_word() {
    let mut composer = Composer::default();
    composer.set_text("/compact focus on tests  ");
    composer.delete_word_before();
    assert_eq!(composer.text(), "/compact focus on ");
}

#[test]
fn moves_within_the_current_line() {
    let mut composer = Composer::default();
    composer.set_text("one\ntwo");
    composer.move_line_start();
    composer.insert_char('>');
    assert_eq!(composer.text(), "one\n>two");
    composer.move_line_end();
    composer.insert_char('!');
    assert_eq!(composer.text(), "one\n>two!");
}

#[test]
fn layout_wraps_and_places_the_cursor() {
    let mut composer = Composer::default();
    composer.set_text("abcdef\ngh");
    let (lines, cursor) = composer.layout(4);
    assert_eq!(lines, vec!["abcd", "ef", "gh"]);
    assert_eq!(cursor, (2, 2));

    composer.set_text("abcd");
    let (lines, cursor) = composer.layout(4);
    assert_eq!(lines, vec!["abcd", ""]);
    assert_eq!(cursor, (0, 1));
}

#[test]
fn paste_normalizes_line_endings() {
    let mut composer = Composer::default();
    composer.insert_str("a\r\nb\rc");
    assert_eq!(composer.text(), "a\nb\nc");
}
