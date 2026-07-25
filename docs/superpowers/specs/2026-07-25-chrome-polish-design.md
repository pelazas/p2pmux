# Chrome polish: footer contrast, top bar brand+tabs, pane title spacing

## Footer
- Non-accent text: **white** (replace DarkGray muted)
- Accent keys: **strong red** `Rgb(220, 50, 47)` (same family as active tab)
- Apply to status/join suffix in the footer as white/dim-white consistently

## Top bar
- One strip matching footer background `Rgb(30,30,30)`
- Left brand: `p2pmux` in white
- Subtle separator ` │ `
- Tabs after brand:
  - Inactive: white/soft white text, compact `Tab #N`, light ` · ` between tabs
  - Active: bold white on strong red pill/segment `Rgb(220,50,47)` with one-cell horizontal padding inside the segment (` Tab #2 `)
- Keep click hit-testing aligned with rendered tab segments (include brand offset)

## Pane titles
- Format with **single** spaces: `Pane #1 host: name control: free`
- Pad title against border: leading+trailing space in the Block title (` Pane #1 host: … `)
- Do not use double/triple gaps between fields

## Verify
- Update unit tests for title strings, footer colors, top-bar brand presence
- cargo fmt, clippy -D warnings, cargo test
- Commit per logical chunk; open PR
