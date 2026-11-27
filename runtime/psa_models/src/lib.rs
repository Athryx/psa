//! LLM provider credentials and the persistent model catalog.
//!
//! [`ProviderRegistry`] owns provider clients built from `auth.toml` (whose
//! format lives in `psa_config::auth`) and keeps credentials server-side. [`ModelCatalog`] discovers models per provider,
//! stores them through `psa_store`, and refreshes them in the background.

mod catalog;
mod codex_auth;
mod providers;

pub use catalog::{CatalogError, CatalogSource, ModelCatalog, ModelCatalogRuntime};
pub use codex_auth::OAuthError;
pub use providers::{AuthError, ProviderRegistry};
