// No shell in this project. The refusal is worded for the model, because the
// only thing it has to go on when deciding what to try next is what it is
// told the tool did.
function onAround(call) {
  if (call.tool === "Bash") {
    return {
      action: "deny",
      reason: "The Bash tool is disabled in this project by policy. Use the file tools instead.",
    };
  }
  return null;
}
