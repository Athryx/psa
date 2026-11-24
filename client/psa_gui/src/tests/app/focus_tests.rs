use super::*;
use std::cell::Cell;

fn target(session: u64, agent: u64) -> TranscriptTarget {
    TranscriptTarget {
        session_id: SessionId(session),
        agent_id: AgentId(agent),
    }
}

#[test]
fn only_new_tabs_and_preview_replacements_request_automatic_focus() {
    let mut ui = Ui::new(DesktopSettings::default());
    assert_eq!(ui.composer_focus_request, None);
    ui.open(target(1, 1), false);
    let first = ui.composer_focus_request.unwrap();
    assert_eq!(first.view, ui.workspace.active_tab().unwrap().id);
    assert_eq!(first.group, ui.workspace.focused_group);
    assert_eq!(first.intent, None);

    ui.open(target(1, 1), true); // Promotion of the existing preview is not a new view.
    assert_eq!(ui.composer_focus_request, Some(first));
    ui.open(target(2, 1), false);
    let second = ui.composer_focus_request.unwrap();
    assert_ne!(second.view, first.view);
    assert_eq!(ui.workspace.groups()[0].tabs.len(), 2);
    ui.open(target(3, 1), false); // Replace that preview, preserving the permanent tab.
    let replacement = ui.composer_focus_request.unwrap();
    assert_ne!(replacement.view, second.view);
    assert_eq!(ui.workspace.groups()[0].tabs.len(), 2);
    assert_eq!(replacement.view, ui.workspace.active_tab().unwrap().id);

    ui.open(target(1, 1), false); // Existing-view navigation does not replay an old request.
    assert_eq!(ui.composer_focus_request, Some(replacement));
    let restored = Ui::new(ui.persisted());
    assert_eq!(restored.composer_focus_request, None);
}

#[test]
fn a_session_open_retains_the_intent_captured_before_async_creation_or_loading() {
    let mut ui = Ui::new(DesktopSettings::default());
    let intent = ComposerFocusIntent(42);
    ui.open_session_target_with_focus(target(1, 0), true, false, Some(intent));
    let request = ui.composer_focus_request.unwrap();
    assert_eq!(request.intent, Some(intent));
    assert_eq!(request.view, ui.workspace.active_tab().unwrap().id);
    ui.draft_roots.insert(
        SessionId(1),
        DraftRoot {
            loading: true,
            ..DraftRoot::default()
        }
        .into(),
    );
    ui.draft_roots.get_mut(&SessionId(1)).unwrap().loading = false;
    ui.draft_roots.get_mut(&SessionId(1)).unwrap().ready = true;
    assert_eq!(ui.composer_focus_request, Some(request));
    ui.open(target(2, 1), true);
    assert_eq!(ui.composer_focus_request.unwrap().intent, None);
}

#[derive(Clone)]
struct FocusFixture {
    ui: Signal<Ui>,
    renders: Rc<Cell<usize>>,
}

struct RecordingDocument(Rc<RefCell<Vec<String>>>);

impl document::Document for RecordingDocument {
    fn eval(&self, js: String) -> document::Eval {
        self.0.borrow_mut().push(js);
        document::Document::eval(&document::NoOpDocument, String::new())
    }
}

fn focus_root() -> Element {
    let fixture = use_context::<FocusFixture>();
    let ui = use_context_provider(|| fixture.ui);
    use_open_composer_focus(ui);
    fixture.renders.set(fixture.renders.get() + 1);
    rsx! { div {} }
}

fn focus_fixture() -> (
    VirtualDom,
    Signal<Ui>,
    Rc<RefCell<Vec<String>>>,
    Rc<Cell<usize>>,
) {
    let mut dom = VirtualDom::new(focus_root);
    let ui = dom.in_scope(ScopeId::ROOT, || {
        Signal::new(Ui::new(DesktopSettings::default()))
    });
    let scripts = Rc::new(RefCell::new(Vec::new()));
    let renders = Rc::new(Cell::new(0));
    let document: Rc<dyn document::Document> = Rc::new(RecordingDocument(scripts.clone()));
    dom.insert_any_root_context(Box::new(document));
    dom.insert_any_root_context(Box::new(FocusFixture {
        ui,
        renders: renders.clone(),
    }));
    dom.rebuild_in_place();
    dom.render_immediate_to_vec();
    (dom, ui, scripts, renders)
}

#[test]
fn automatic_focus_effect_is_selective_and_does_not_replay_on_live_updates() {
    let (mut dom, mut ui, scripts, renders) = focus_fixture();
    assert!(scripts.borrow().is_empty());
    let baseline = renders.get();
    ui.write().open(target(1, 1), false);
    dom.render_immediate_to_vec();
    assert_eq!(scripts.borrow().len(), 1);
    let request = ui.peek().composer_focus_request.unwrap();
    assert_eq!(
        scripts.borrow()[0],
        format!(
            "window.psaFocusComposer?.({}, {});",
            request.group.0, request.view.0
        )
    );
    for index in 0..10 {
        ui.write().connection = format!("stream update {index}");
        ui.write()
            .transcript_status
            .insert(target(9, 1), index.to_string());
        dom.render_immediate_to_vec();
    }
    assert_eq!(scripts.borrow().len(), 1);
    assert_eq!(
        renders.get(),
        baseline,
        "the focus projection must not subscribe the app render"
    );

    ui.write().open(target(2, 1), false);
    dom.render_immediate_to_vec();
    assert_eq!(
        scripts.borrow().len(),
        2,
        "preview replacement requests a new focus"
    );
    ui.write().open(target(2, 1), true);
    dom.render_immediate_to_vec();
    assert_eq!(scripts.borrow().len(), 2, "promotion does not refocus");
}

#[test]
fn a_coalesced_open_cannot_focus_a_view_that_is_no_longer_active() {
    let (mut dom, mut ui, scripts, _) = focus_fixture();
    ui.write().open(target(1, 1), true);
    dom.render_immediate_to_vec();
    scripts.borrow_mut().clear();
    ui.write().open(target(2, 1), true);
    ui.write().open(target(1, 1), true);
    dom.render_immediate_to_vec();
    assert!(scripts.borrow().is_empty());
}

#[test]
fn an_async_create_to_open_handoff_does_not_capture_a_new_intent() {
    let (mut dom, mut ui, scripts, _) = focus_fixture();
    let intent = dom.in_scope(ScopeId::ROOT, begin_composer_open);
    assert_eq!(scripts.borrow().len(), 1);
    assert!(scripts.borrow()[0].starts_with("window.psaBeginComposerOpen?.("));
    ui.write()
        .open_session_target_with_focus(target(1, 0), true, false, Some(intent));
    dom.render_immediate_to_vec();
    assert_eq!(scripts.borrow().len(), 2);
    let request = ui.peek().composer_focus_request.unwrap();
    assert_eq!(
        scripts.borrow()[1],
        format!(
            "window.psaFocusComposer?.({}, {}, {});",
            request.group.0, request.view.0, intent.0
        )
    );
}
