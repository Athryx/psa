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
