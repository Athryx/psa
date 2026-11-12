//! Modal for creating a sandbox on a manager probe: image, name, and bind mounts.
use crate::{
    app::Ui,
    input::{BrowserInput, DraftStore, InputSnapshot},
    modal::Modal,
    network,
    path_picker::ProbePathInput,
};
use dioxus::prelude::*;
use psa_api::{CreateSandbox, Mount, ProbeId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Field {
    Image,
    Name,
    Container(u64),
}

#[derive(Debug, Clone, PartialEq)]
struct MountRow {
    id: u64,
    /// Owned by the row's path input; mirrored here for the summary and request.
    host: String,
    writable: bool,
}

#[component]
pub fn CreateSandboxButton(manager: ProbeId, manager_name: String) -> Element {
    let mut open = use_signal(|| false);
    let title = format!("Create a sandbox on {manager_name}");
    rsx! {
        button { r#type: "button", class: "profile-add sandbox-new", title, onclick: move |_| open.set(true), "＋ New sandbox" }
        if open() {
            CreateSandboxModal { manager, manager_name, open }
        }
    }
}

/// Mounted only while open, so every opening starts with a fresh form.
#[component]
fn CreateSandboxModal(manager: ProbeId, manager_name: String, open: Signal<bool>) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut fields = use_signal(DraftStore::<Field>::default);
    let mut rows = use_signal(Vec::<MountRow>::new);
    let mut next_row = use_signal(|| 0u64);
    let mut submitting = use_signal(|| false);
    let mut submit_error = use_signal(|| None::<String>);
    let mut default_image = use_signal(|| None::<String>);

    use_hook(move || {
        spawn(async move {
            let Ok(api) = network::client(ui) else { return };
            if let Ok(config) = api.get_config().await {
                default_image.set(config.default_sandbox_image);
            }
        });
    });

    let text = move |key: &Field| fields.read().get(key).cloned().unwrap_or_default();

    let submit = move |_| {
        if submitting() {
            return;
        }
        let request = match build_request(&fields.read(), &rows.read()) {
            Ok(request) => request,
            Err(message) => {
                submit_error.set(Some(message));
                return;
            }
        };
        if request.image.is_none() && default_image().is_none() {
            submit_error.set(Some(
                "Enter an image; the server has no default sandbox image".into(),
            ));
            return;
        }

        submitting.set(true);
        submit_error.set(None);
        spawn(async move {
            let result = network::create_sandbox(ui, manager, request).await;
            submitting.set(false);
            match result {
                Ok(_) => open.set(false),
                Err(message) => submit_error.set(Some(message)),
            }
        });
    };

    let image_placeholder = match default_image() {
        Some(image) => format!("Server default: {image}"),
        None => "Docker image, e.g. ubuntu:24.04".into(),
    };

    rsx! {
        Modal { title: "New sandbox on {manager_name}", close_label: "Close new sandbox", class: "sandbox-create", onclose: move |_| open.set(false),
            label { r#for: "sandbox-image", "Image" }
            FieldInput { id: "sandbox-image", field: Field::Image, fields, placeholder: image_placeholder, aria_label: "Docker image" }
            label { r#for: "sandbox-name", "Name (optional)" }
            FieldInput { id: "sandbox-name", field: Field::Name, fields, placeholder: "Generated when empty", aria_label: "Sandbox name" }

            h3 { class: "sandbox-mounts-heading", "Mounts" }
            div { class: "profile-list",
                for (index, row) in rows().into_iter().enumerate() {
                    {
                        let container = text(&Field::Container(row.id));
                        let summary = match (row.host.trim(), container.trim()) {
                            ("", "") => "Not configured".to_owned(),
                            (host, "") => format!("{host} → ?"),
                            ("", container) => format!("? → {container}"),
                            (host, container) => format!("{host} → {container}"),
                        };
                        let id = row.id;
                        rsx! {
                article { key: "{id}", class: "profile-card sandbox-mount",
                    div { class: "profile-card-heading",
                        div { class: "sandbox-mount-title", strong { "Mount {index + 1}" } div { class: "muted", "{summary}" } }
                        button { r#type: "button", class: if row.writable { "profile-select selected" } else { "profile-select" },
                            "aria-pressed": row.writable, title: "Toggle write access inside the sandbox",
                            onclick: move |_| { if let Some(item) = rows.write().iter_mut().find(|item| item.id == id) { item.writable = !item.writable; } },
                            if row.writable { "Writable" } else { "Read-only" }
                        }
                    }
                    label { r#for: "sandbox-host-{id}", "Host path on {manager_name}" }
                    ProbePathInput { id: "sandbox-host-{id}", probe: Some(manager), aria_label: "Host path", placeholder: "/home/user/project",
                        onchange: move |path: String| { if let Some(item) = rows.write().iter_mut().find(|item| item.id == id) { item.host = path; } },
                        onselect: move |path: String| {
                            let mut store = fields.write();
                            if store.get(&Field::Container(id)).is_none_or(|text| text.trim().is_empty()) { store.insert(Field::Container(id), path); }
                        },
                    }
                    label { r#for: "sandbox-container-{id}", "Path inside the sandbox" }
                    FieldInput { id: "sandbox-container-{id}", field: Field::Container(id), fields, placeholder: "/workspace", aria_label: "Container path" }
                    button { r#type: "button", class: "profile-delete", "aria-label": "Remove mount {index + 1}",
                        onclick: move |_| rows.write().retain(|item| item.id != id), "Remove mount" }
                }
                        }
                    }
                }
                button { r#type: "button", class: "profile-add", onclick: move |_| {
                    let id = next_row();
                    next_row.set(id + 1);
                    rows.write().push(MountRow { id, host: String::new(), writable: false });
                }, "＋ Add mount" }
            }

            if let Some(message) = submit_error() { p { class: "error", "{message}" } }
            div { class: "modal-actions",
                button { r#type: "button", onclick: move |_| open.set(false), "Cancel" }
                button { class: "primary", r#type: "button", disabled: submitting(), onclick: submit,
                    if submitting() { "Creating…" } else { "Create sandbox" }
                }
            }
        }
    }
}

#[component]
fn FieldInput(
    id: String,
    field: Field,
    fields: Signal<DraftStore<Field>>,
    placeholder: String,
    aria_label: String,
) -> Element {
    let initial = fields.peek().get(&field).cloned().unwrap_or_default();
    let mutation = fields.read().mutation(&field);
    rsx! {
        BrowserInput { id, aria_label, placeholder, initial_value: initial, mutation,
            onedit: move |snapshot: InputSnapshot| { fields.write().apply_snapshot(field.clone(), snapshot); }
        }
    }
}

fn build_request(fields: &DraftStore<Field>, rows: &[MountRow]) -> Result<CreateSandbox, String> {
    let value = |field: &Field| {
        fields
            .get(field)
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    };

    let name = value(&Field::Name);
    if let Some(name) = &name
        && !psa_api::sandboxes::is_valid_sandbox_name(name)
    {
        return Err("Names use up to 64 letters, digits, '-', '_' or '.'".into());
    }

    let mut mounts = Vec::with_capacity(rows.len());
    for row in rows {
        let host = Some(row.host.trim().to_owned()).filter(|host| !host.is_empty());
        let container = value(&Field::Container(row.id));
        let (Some(host), Some(container)) = (host, container) else {
            return Err("Every mount needs a host path and a container path".into());
        };
        if !host.starts_with('/') || !container.starts_with('/') {
            return Err("Mount paths must be absolute".into());
        }
        mounts.push(Mount {
            host_path: host.into(),
            container_path: container.into(),
            writable: row.writable,
        });
    }

    Ok(CreateSandbox {
        image: value(&Field::Image),
        name,
        mounts,
        start: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_require_complete_absolute_mounts_and_valid_names() {
        let mut fields = DraftStore::<Field>::default();
        let mut rows = vec![MountRow {
            id: 0,
            host: String::new(),
            writable: true,
        }];
        assert!(build_request(&fields, &rows).is_err());

        rows[0].host = " /src ".into();
        fields.insert(Field::Container(0), "work".into());
        assert!(build_request(&fields, &rows).is_err());

        fields.insert(Field::Container(0), "/work".into());
        fields.insert(Field::Name, "bad name".into());
        assert!(build_request(&fields, &rows).is_err());

        fields.insert(Field::Name, " dev ".into());
        let request = build_request(&fields, &rows).unwrap();
        assert_eq!(request.name.as_deref(), Some("dev"));
        assert!(request.image.is_none());
        assert_eq!(
            request.mounts[0].host_path,
            std::path::PathBuf::from("/src")
        );
        assert_eq!(
            request.mounts[0].container_path,
            std::path::PathBuf::from("/work")
        );
        assert!(request.mounts[0].writable);
    }
}
