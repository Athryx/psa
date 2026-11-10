use clap::Parser;
use dioxus::prelude::*;
use psa_desktop::{DesktopPlatform, load_settings, resolve_settings_path};
use psa_gui::{GuiBootstrap, GuiConfig};
use std::{cell::Cell, path::PathBuf, rc::Rc};

#[derive(Parser)]
#[command(about = "PSA desktop client")]
struct Options {
    /// PSA data directory; desktop settings live in desktop.toml here.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Server executable used when starting a managed local server.
    #[arg(long, default_value = "psa_server")]
    server_binary: PathBuf,
}

fn main() {
    if let Err(error) = launch(Options::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn launch(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    let path = resolve_settings_path(options.data_dir)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let settings = runtime.block_on(load_settings(&path))?;
    drop(runtime);

    let platform = Rc::new(DesktopPlatform::with_server_binary(
        path,
        options.server_binary,
    ));
    let bootstrap = GuiBootstrap {
        config: GuiConfig { settings },
        platform: platform.clone(),
    };
    let dom = VirtualDom::new(DesktopRoot)
        .with_root_context(bootstrap)
        .with_root_context(platform);
    let window = dioxus_desktop::tao::window::WindowBuilder::new()
        .with_title("PSA")
        .with_inner_size(dioxus_desktop::tao::dpi::LogicalSize::new(1280.0, 860.0))
        .with_min_inner_size(dioxus_desktop::tao::dpi::LogicalSize::new(640.0, 480.0));
    let config = dioxus_desktop::Config::new()
        .with_window(window)
        // Keep the renderer and its async save worker alive until the close flush finishes.
        .with_close_behaviour(dioxus_desktop::WindowCloseBehaviour::WindowHides)
        .with_disable_drag_drop_handler(true)
        // No native menu bar or WebView context menu; the app provides its own row menu.
        .with_menu(None)
        .with_disable_context_menu(true);
    dioxus_desktop::launch::launch_virtual_dom(dom, config)
}

#[allow(non_snake_case)]
fn DesktopRoot() -> Element {
    use dioxus_desktop::{
        WindowCloseBehaviour,
        tao::event::{Event, WindowEvent},
    };

    let platform = use_context::<Rc<DesktopPlatform>>();
    let window = dioxus_desktop::use_window();
    let closing = use_hook(|| Rc::new(Cell::new(false)));
    dioxus_desktop::use_wry_event_handler(move |event, _| {
        let Event::WindowEvent {
            window_id,
            event: WindowEvent::CloseRequested,
            ..
        } = event
        else {
            return;
        };
        if *window_id != window.id() || closing.replace(true) {
            return;
        }
        let platform = platform.clone();
        let window = window.clone();
        let closing = closing.clone();
        spawn(async move {
            if platform.flush_pending_settings().await.is_err() {
                // The GUI callback reports the error. Keep the window usable so
                // the user can retry instead of silently discarding settings.
                window.set_visible(true);
                window.set_focus();
                closing.set(false);
                return;
            }
            window.set_close_behavior(WindowCloseBehaviour::WindowCloses);
            window.close();
        });
    });
    rsx! { psa_gui::App {} }
}
