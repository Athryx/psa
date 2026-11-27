//! A filterable list shown over the transcript, such as the `/sessions` list.
use llm_api::providers::Model;
use psa::api::{AgentId, SessionId};

/// Titles of the pickers that wait for the server's lists.
pub const SESSIONS_TITLE: &str = "Sessions";
pub const AGENTS_TITLE: &str = "Session and agents";

/// What choosing an item does.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerValue {
    Session(SessionId),
    Agent(AgentId),
    /// Pin or unpin a model; the picker stays open.
    Pin {
        model: Model,
        pinned: bool,
    },
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub value: PickerValue,
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    /// Lines shown above the filter, such as the session's details.
    pub header: Vec<String>,
    pub filter: String,
    /// What Enter does, for the hint under the list.
    pub enter_hint: &'static str,
    /// Waiting for items from the server.
    pub loading: bool,
    items: Vec<PickerItem>,
    selected: usize,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            title: title.into(),
            header: Vec::new(),
            filter: String::new(),
            enter_hint: "open",
            loading: false,
            items,
            selected: 0,
        }
    }

    pub fn loading(title: impl Into<String>) -> Self {
        Self {
            loading: true,
            ..Self::new(title, Vec::new())
        }
    }

    #[cfg(test)]
    pub fn with_filter(mut self, filter: &str) -> Self {
        self.filter = filter.trim().to_owned();
        self
    }

    pub fn with_header(mut self, header: Vec<String>) -> Self {
        self.header = header;
        self
    }

    pub fn with_enter_hint(mut self, hint: &'static str) -> Self {
        self.enter_hint = hint;
        self
    }

    /// Replace the items in place, keeping the filter and highlighted row.
    pub fn update_items(&mut self, items: Vec<PickerItem>) {
        self.items = items;
    }

    pub fn set_items(&mut self, items: Vec<PickerItem>) {
        self.items = items;
        self.loading = false;
        self.selected = 0;
    }

    /// Items whose label or detail contains every filter word, ignoring case.
    pub fn visible(&self) -> Vec<&PickerItem> {
        let words: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.items
            .iter()
            .filter(|item| {
                let text = format!("{} {}", item.label, item.detail).to_lowercase();
                words.iter().all(|word| text.contains(word))
            })
            .collect()
    }

    pub fn selected_index(&self) -> usize {
        self.selected.min(self.visible().len().saturating_sub(1))
    }

    pub fn selected_value(&self) -> Option<PickerValue> {
        let visible = self.visible();
        visible
            .get(self.selected_index())
            .map(|item| item.value.clone())
    }

    pub fn select_next(&mut self) {
        let count = self.visible().len();
        if count > 0 {
            self.selected = (self.selected_index() + 1) % count;
        }
    }

    pub fn select_previous(&mut self) {
        let count = self.visible().len();
        if count > 0 {
            self.selected = (self.selected_index() + count - 1) % count;
        }
    }

    pub fn push_filter(&mut self, character: char) {
        self.filter.push(character);
        self.selected = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }
}

#[cfg(test)]
#[path = "../tests/tui/picker/tests.rs"]
mod tests;
