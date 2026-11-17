//! PSA client library.
//!
//! [`api`] holds the wire types shared by the server, its clients and probes,
//! and a thin async HTTP/SSE [`api::Client`] with one method per route.
pub mod api;
