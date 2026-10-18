This is a work in progress custom rust agent harness.

The focus is on centralized server, clients connect to it, and different probes can connect from different sandboxes or computers,
allowing managing agents across many different environments and seeing them in one central place.

Should also be easy to programatically interact with from a client library.

# Coding Style

- Add blank lines inside functions to separate meaningful stages. Keep related statements together; groups of roughly three to eight lines are a useful guide, not a formatting rule.
- Aim for functions of about 20 lines or fewer when that improves readability. A longer function is fine when it remains clear, such as a sequence of simple related actions. Do not split a cohesive operation solely to meet a line count.
- Keep control-flow nesting to about three levels within a function. Use early returns or extract a well-named function when deeper nesting obscures the main path.
  - Not a hard rule, but recommended.
  - Nesting is counnted by semantic control flow strucutres inside a function, not strictly whitespace (for instance, expressions split across lines are not considered nested because inndented, and in fct inndenting them to match rust style is good practice)
    - Body of functions are not considered nested
