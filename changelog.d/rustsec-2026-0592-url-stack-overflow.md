### Security

- Bumped `url` 2.5.8 → 2.5.9, closing RUSTSEC-2026-0592 (`Url::origin()`
  recursed once per nested `blob:` prefix with no depth limit, so a crafted
  URL could overflow the stack and abort the process). Reachable through
  origin comparisons on externally-influenced URLs (redirect targets, Web
  Push endpoints, parsed documents).
