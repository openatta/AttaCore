// Refuses the tool that would otherwise stop and ask for approval. What the
// case watches for is the question that never gets asked.
function onAround(call) {
  if (call.tool === "Guarded") {
    return { action: "deny", reason: "SCRIPT-TRACE-DENY-GUARDED: refused by policy" };
  }
  return null;
}
