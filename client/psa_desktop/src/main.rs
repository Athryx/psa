use clap::Parser;
use psa_desktop::{DesktopPlatform, load_settings, resolve_settings_path};
use psa_gui::{GuiBootstrap, GuiConfig};
use std::{path::PathBuf, rc::Rc};

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

    let bootstrap = GuiBootstrap {
        config: GuiConfig { settings },
        platform: Rc::new(DesktopPlatform::with_server_binary(
            path,
            options.server_binary,
        )),
    };
    let dom = dioxus::prelude::VirtualDom::new(psa_gui::App).with_root_context(bootstrap);
    let window = dioxus_desktop::tao::window::WindowBuilder::new()
        .with_title("PSA")
        .with_inner_size(dioxus_desktop::tao::dpi::LogicalSize::new(1280.0, 860.0))
        .with_min_inner_size(dioxus_desktop::tao::dpi::LogicalSize::new(640.0, 480.0));
    let config = dioxus_desktop::Config::new()
        .with_window(window)
        .with_disable_drag_drop_handler(true);
    dioxus_desktop::launch::launch_virtual_dom(dom, config)
}
