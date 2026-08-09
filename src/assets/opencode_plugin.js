// p2pmux — installed by `p2pmux setup opencode`. owner: p2pmux
//
// Reports what this opencode session is doing to the p2pmux inbox, so a session
// shared with other machines can say `needs you` about an agent running here.
//
// `p2pmux notify` is a no-op outside a p2pmux pane — it looks for the pane's
// socket in the environment and exits silently when there is none — so this is
// safe to leave installed for every opencode session on this machine.
//
// Edit it if you like; `p2pmux setup opencode` overwrites it, and
// `p2pmux setup opencode --uninstall` deletes it.

import { spawn } from "node:child_process"

// Never throw into opencode, and never make it wait. A plugin hook that fails
// is a plugin the user removes, and every one of these fires on the agent's own
// critical path: the child is fire-and-forget and its output goes nowhere.
function notify(status, message, cwd) {
  try {
    const child = spawn("p2pmux", ["notify", "opencode", "--status", status], {
      stdio: ["pipe", "ignore", "ignore"],
    })
    child.on("error", () => {})
    child.stdin.on("error", () => {})
    child.stdin.end(JSON.stringify({ status, message: message || "", cwd }))
    child.unref()
  } catch {
    // Deliberately empty: see above.
  }
}

// What the agent is blocked on, in the words opencode used for it.
//
// A `pending` carrying no message is dropped by `p2pmux notify` on purpose, so
// a permission event this cannot name raises nothing rather than lighting the
// badge with "an agent wants something".
// The shape, from a real `permission.asked`:
//   {permission: "bash", patterns: ["python3 stats.py"],
//    metadata: {command: "python3 stats.py"}, …}
function permissionText(properties) {
  const source = properties || {}
  const metadata = source.metadata || {}
  const patterns = Array.isArray(source.patterns) ? source.patterns : []
  const candidates = [
    metadata.command,
    metadata.filePath,
    patterns[0],
    source.title,
    source.permission,
  ]
  const named = candidates.find(
    (value) => typeof value === "string" && value.trim(),
  )
  return named ? `needs permission: ${named.trim()}` : ""
}

export const P2pmux = async ({ directory, worktree }) => {
  // `directory` first: it is where opencode was started, which is what a row in
  // the inbox is trying to tell you. `worktree` is the enclosing git checkout,
  // and outside a repository opencode walks all the way up and reports `/` —
  // which is a working directory nobody has ever been in.
  const cwd = directory || worktree || process.cwd()
  const push = (status, message) => notify(status, message, cwd)

  return {
    event: async ({ event }) => {
      switch (event && event.type) {
        case "message.updated":
          push("running", "")
          break
        // opencode has renamed this one across versions; both spellings mean
        // the same thing and only one of them will ever arrive.
        case "permission.asked":
        case "permission.updated":
          push("pending", permissionText(event.properties))
          break
        // Answered — whoever was needed has been. Back to working, so the row
        // stops asking for a human who has already come.
        case "permission.replied":
          push("running", "")
          break
        case "session.idle":
          push("done", "")
          break
        case "session.error":
          push("error", "")
          break
        case "session.deleted":
          push("idle", "")
          break
      }
    },
    "tool.execute.before": async (input) => {
      push("running", input && input.tool ? `running ${input.tool}` : "")
    },
    "tool.execute.after": async (input) => {
      push("running", input && input.tool ? `ran ${input.tool}` : "")
    },
  }
}
