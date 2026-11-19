# Agents

Agent defined by sets of:
- tools
- hooks
- model config (`model` slug such as `openrouter/vendor/model`, plus `effort`)
- Extras (just consumed by hooks / tools, so are just fields)
  - skills
  - permitted subagents
- extra system prompts are explicit `BundleConfig::ExtraSystemPrompt { prompt }`
  construction payloads, not agent fields
- tools and hooks are configured only through bundles

Low level
- tools
- hooks
  - agent create hook assembles system prompt, so consumes skills
