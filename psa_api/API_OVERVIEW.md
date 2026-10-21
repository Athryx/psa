# Required Features

Http endpoints, with server side events streaming to client. streams in agent events and partial completed events.

## Session and Agent

/sessions/{id}
- create session
- fork session
- update session
- view session
- list sessions

/sessions/{id}/events
- query events in a range
  - optional flag to include non persisted events
  - flag to enable sse streaming

/sessions/{id}/agents/{id}
- create agent
- list agents
- get agents
- submit requests to either interrupt or send steering message

/sessions/{id}/agents/{id}/events
- query events in a range
  - optional flag to include non persisted events
  - flag to enable sse streaming

TODO: other stuff from client like different agent definitions, configs, mcp, etc
TODO: probe api

## Implementation

Exact route templates/builders are in `src/routes.rs`; wire types are in
`src/types.rs` and `src/events.rs`. `src/client.rs` provides the async `Client`,
including typed JSON requests and SSE decoding. Event queries use `after`,
`through`, `limit`, `include_pending`, and `stream` parameters. SSE supports
scoped `Last-Event-ID` resume cursors and provisional-to-persistent promotion.

Session views include runtime `busy`; agent views include state and cumulative
token usage. See `../psa_server/README.md` for startup, `auth.toml`, client examples,
queue/shutdown semantics, and the trusted-client-only security boundary.

## Probes

`GET /probes` lists registered systems; `GET /probes/{probe_id}` fetches one UUID.
Both return flattened probe metadata (`id`, `hostname`, `os`, `architecture`,
optional `label`, absolute `data_dir`), timestamps, and live `is_active`.
`Client::list_probes` and `Client::get_probe` wrap these endpoints.

Probes authenticate `GET /probes/connect` with `Authorization: Bearer <probe_key>`.
Query parameters are `version=1` and the metadata fields above; success upgrades
to WebSocket. Invalid auth returns 401, incompatible versions/metadata return
400, and an already-connected UUID returns 409. Offline records persist.

Session creation requires `probe_id`, referencing a registered probe. It is
returned in session responses, remains fixed, and is inherited on fork. No
remote-operation HTTP endpoints are exposed yet: use server-side probe handles.
