# City session names

Replace adjective-noun session names with random world cities. Auto-pick on every create/join; `--session-name` remains an optional override.

## Commits

1. `feat: add world-city session name list` — curated kebab-case city list + `generate_name()` picks one at random; keep `valid_name` rules.
2. `feat: avoid colliding with live session names` — if picked city is already live locally, retry other cities; if exhausted, append `-2`, `-3`, …
3. `test: cover city name generation and collisions` — unit tests for format, uniqueness retries, collision suffix.
4. `docs: describe auto city session names` — README note that create picks a city; `--session-name` optional.

## Constraints
- Work only in `/Users/pelazas/Desktop/p2pmux-city-names`
- One commit per task above
- `cargo fmt`, `clippy -D warnings`, `cargo test` green
- Do not require users to pass a session name
