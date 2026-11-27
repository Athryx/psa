use super::*;

fn item(label: &str, detail: &str, id: u64) -> PickerItem {
    PickerItem {
        label: label.into(),
        detail: detail.into(),
        value: PickerValue::Session(SessionId(id)),
    }
}

#[test]
fn filter_matches_every_word_in_label_or_detail() {
    let picker = Picker::new(
        "Sessions",
        vec![
            item("fix tests", "2h ago · laptop", 1),
            item("docs", "5m ago · laptop", 2),
            item("Fix CI", "1d ago · server", 3),
        ],
    )
    .with_filter("fix laptop");
    let labels: Vec<_> = picker.visible().iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["fix tests"]);
}

#[test]
fn selection_wraps_and_resets_when_the_filter_changes() {
    let mut picker = Picker::new("Sessions", vec![item("a", "", 1), item("b", "", 2)]);
    picker.select_previous();
    assert_eq!(
        picker.selected_value(),
        Some(PickerValue::Session(SessionId(2)))
    );
    picker.select_next();
    assert_eq!(
        picker.selected_value(),
        Some(PickerValue::Session(SessionId(1)))
    );
    picker.select_next();
    picker.push_filter('a');
    assert_eq!(
        picker.selected_value(),
        Some(PickerValue::Session(SessionId(1)))
    );
    picker.push_filter('z');
    assert_eq!(picker.selected_value(), None);
}

#[test]
fn loading_picker_keeps_its_filter_when_items_arrive() {
    let mut picker = Picker::loading("Sessions").with_filter(" b ");
    assert!(picker.loading);
    picker.set_items(vec![item("a", "", 1), item("b", "", 2)]);
    assert!(!picker.loading);
    assert_eq!(picker.filter, "b");
    assert_eq!(picker.visible().len(), 1);
}
