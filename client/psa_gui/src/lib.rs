//! Renderer-independent Dioxus client. Native integration and secret-bearing persistence
//! belong to the executable, not this crate.
use client_shared::{DesktopSettings, LocalServerStatus};
use dioxus::prelude::*;
use std::{future::Future, pin::Pin, rc::Rc};

mod app;
mod command_prompt;
mod composer;
mod context_view;
mod creation;
mod input;
mod interaction;
mod markdown;
mod modal;
mod network;
mod path_picker;
mod persistence;
mod probes_pane;
mod reactive;
mod sandbox_modal;
mod style;
mod syntax_highlight;
#[cfg(test)]
mod tests;
mod tool_format;
mod transcript_nav;
mod transcript_view;
mod views;

/// Initial settings, loaded asynchronously by the platform before mounting.
#[derive(Clone)]
pub struct GuiConfig {
    pub settings: DesktopSettings,
}

/// Flush the GUI's latest durable snapshot before a platform closes its window.
pub type SettingsFlush = Rc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>>>>>;

/// The deliberately small platform boundary. Futures need not be `Send`:
/// web and native renderers may both own thread-local platform resources.
pub trait GuiPlatform {
    fn set_settings_flush(&self, _flush: Option<SettingsFlush>) {}

    fn save_settings(
        &self,
        settings: DesktopSettings,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>>;

    fn local_server_status(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async { Err("Local server management is unavailable".into()) })
    }
    fn local_server_start(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async { Err("Local server management is unavailable".into()) })
    }
    fn local_server_stop(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async { Err("Local server management is unavailable".into()) })
    }
    fn local_server_restart(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async { Err("Local server management is unavailable".into()) })
    }
}

#[derive(Clone)]
pub struct GuiBootstrap {
    pub config: GuiConfig,
    pub platform: Rc<dyn GuiPlatform>,
}

/// Mount with `VirtualDom::new(App).with_root_context(GuiBootstrap { .. })`.
#[allow(non_snake_case)]
pub fn App() -> Element {
    app::app()
}
