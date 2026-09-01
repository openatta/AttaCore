// A ring that intervenes: no edits this session. The refusal is the whole
// declared difference a profile using this script is allowed to show.
function onAround(call) {
  if (call.tool === "Edit") {
    return { action: "deny", reason: "SCRIPT-TRACE-NO-EDITS: refused by policy" };
  }
  return { action: "proceed", timeoutMs: 30000 };
}
