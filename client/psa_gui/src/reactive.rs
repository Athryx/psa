//! Cheap immutable snapshots for selective UI subscriptions.
//!
//! The reducer still owns one coherent state, but renderers subscribe to small
//! projections. Unchanged collections retain their identity; mutating one uses
//! copy-on-write so an in-flight render keeps a consistent snapshot.
use crate::app::Ui;
use dioxus::prelude::*;
use std::{
    ops::{Deref, DerefMut},
    rc::Rc,
};

#[derive(Debug)]
pub(crate) struct Shared<T>(Rc<T>);

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self(Rc::new(value))
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> From<T> for Shared<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        Rc::make_mut(&mut self.0)
    }
}

impl<T> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Eq for Shared<T> {}

impl<'a, T> IntoIterator for &'a Shared<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.as_ref().into_iter()
    }
}

pub(crate) fn use_ui_selector<T: PartialEq + 'static>(
    mut select: impl FnMut(&Ui) -> T + 'static,
) -> Memo<T> {
    let ui = use_context::<Signal<Ui>>();
    use_memo(move || select(&ui.read()))
}

/// Subscribe the current component to just the supplied projection. The
/// component may then use `ui.peek()` to read the same coherent reducer state
/// without accidentally subscribing to every transient update.
pub(crate) fn use_ui_subscription<T: PartialEq + 'static>(select: impl FnMut(&Ui) -> T + 'static) {
    let selected = use_ui_selector(select);
    let _ = selected.read();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_snapshots_share_identity_and_mutation_is_copy_on_write() {
        let mut current = Shared::new(vec![1, 2]);
        let previous = current.clone();
        assert_eq!(current, previous);

        current.push(3);
        assert_ne!(current, previous);
        assert_eq!(&*previous, &[1, 2]);
        assert_eq!(&*current, &[1, 2, 3]);
    }
}
