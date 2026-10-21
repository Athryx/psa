# Agents

Agent defined by sets of:
- tools
- hooks
- model config (provider + model, thinking level)
- Extras (just consumed by hooks / tools, so are just fields)
  - skills
  - permitted subagents
  - extra system prompt

Low level
- tools
- hooks
  - agent create hook assembles system prompt, so consumes skills
