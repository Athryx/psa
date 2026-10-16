# Crates

- llm_api: async rust bindings and types to unified make requests to llm apis
  - common trait and types
  - special impls for certain api's
- api: types and client lib for requests to server
  - maybe both sync and async, most of the code will be type defs anyway + wrapping http requests
  - PyO3 for python bindings
- server: actual server owning requests, db, and async tasks for agents
- probe: what connects to server, represents machine or permissions context where commands from agent are run

Clients maybe have a shared create, plus one for tui, desktop app, mobile app, web (rust wasm client)
rust wasm client server by server on endpoint, then it use client requets like normal

# Config

static config mostly toml
stuff like agent definitions, others, often maybe python, want dynamics
stuff agent change is python defs evaled in built in interpreter
codemode also use python persistent

# Tools

Tools defined as structs with trait:
- call method async take in context and deser args struct, return serializable result
  - codemode returns result to python object
- call_str same as above, must return a string
  - used for native tools which return a string, essentially renders result from regular call
- some description method, returning struct representing parts to inject in system prompt

NativeTool + CodeModeTool seperate traits?

## Defined Tools

- python codemode persistent session
  - maybe reset_python tool if agent messes it up
- read: read files and images
- edit: for now standard target and replace, fuzzy matching ig
- write: just write a file
- bash and interact_bash: same interface as in pi extension
- multiagents and workflows like pi extension
- interactive tools
  - ask_question
  - plan mode can just be a skill

# Agents

Maybe a few seperate agent loop structs with a common trait, for instance depending on compaction, supervisor, etc
shared code
probably simples approach

# Probe

Standard interface server calls on probe, like read file, write file, std::command like interface for processes, kill all process for agent
download upload directory tree, etc
Python is sandboxed, and runs in server not on probe
get system info

probe should bundle and extract rg and fd into path, be a fully self contained static binary
