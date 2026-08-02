# Escalation log

One `##` heading per event. `sh scripts/metrics.sh` counts them.

**What counts as an escalation:** an agent got stuck, and unsticking it needed a keyboard on a
machine that is not this laptop. Nothing else. Not "I ssh'd somewhere", not "an agent asked a
question I answered in the same terminal it was running in".

**Why it is the only number that decides the roadmap:** the whole v2 thesis is that this moment
happens often enough to build a product around. Fewer than 2 a week kills it. 2–5 means ship the
inbox and widen to ~10 hand-picked users. More than 5 means build hard and rewrite the plan that
afternoon.

Log it in the 20 seconds after it happens or it does not get logged. An entry that is one line is
worth more than one that is never written.

Template — copy, fill, keep it short:

```
## YYYY-MM-DD HH:MM — one-line summary
Machine: <where the agent was>
Agent: <claude / codex / other>
Blocked on: <what it could not do alone>
Resolved by: <what you actually typed>
Would a credential have unblocked it without you? <yes / no>
```

That last line is the one that matters most: it separates "the agent needed a human" from "the
agent needed a secret", and only the second one is the credential moment the pitch leads with.

---

<!-- entries below, newest first -->
