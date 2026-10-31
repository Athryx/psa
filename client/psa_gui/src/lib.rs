//! Renderer-independent Dioxus client. Native integration and secret-bearing persistence
//! belong to the executable, not this crate.
use client_shared::{DesktopSettings, LocalServerStatus};
use dioxus::prelude::*;
use std::{future::Future, pin::Pin, rc::Rc};

mod app;
mod composer;
mod creation;
mod input;
mod interaction;
mod markdown;
mod network;
mod persistence;
mod reactive;
mod style;
#[cfg(test)]
mod tests;
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
