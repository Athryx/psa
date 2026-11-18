//! PSA client library.
//!
//! [`api`] holds the wire types shared by the server, its clients and probes,
//! and a thin async HTTP/SSE [`api::Client`] with one method per route.
//!
//! The crate root is a higher-level layer for programs that drive agents. A
//! [`Context`] scopes work to a project and, optionally, to one probe and its
//! sandboxes. From it,
//! [`SessionHandle`]s list and create agents from [`AgentSpec`]s, and
//! [`AgentHandle`]s send messages, wait for runs, and read results and events:
//!
//! ```no_run
//! # async fn example() -> psa::Result<()> {
//! use psa::{AgentLayout, AgentSpec, Context, SessionSpec};
//!
//! let context = Context::connect("http://localhost:8080", "client-key", "my-app")?
//!     .working_directory("/work/my-app")
//!     .build()
//!     .await?;
//! let session = context.ensure_session(SessionSpec::new("triage")).await?;
//! let agent = session
//!     .ensure_agent(AgentSpec::new("triager", AgentLayout::new("triager")))
//!     .await?;
//! let summary: String = agent.run_as("Summarize the open issues.").await?;
//! # Ok(())
//! # }
//! ```
pub mod api;

mod agent;
mod context;
mod error;
mod events;
mod probe;
mod session;
mod skill;
mod spec;

pub use agent::*;
pub use context::*;
pub use error::*;
pub use events::{EventEnd, EventFeed, EventPages, EventRange, EventStart, EventUpdate};
pub use probe::{ProbeHandle, ProbeSelector};
pub use session::*;
pub use skill::{Skill, SkillRef};
pub use spec::{AgentConfigRef, AgentLayout, AgentSpec, Inclusions, SessionProbe, SessionSpec};
