// `tool.around`, refusing instead of answering. The other around fixture
// answers in place of the tool; this one refuses, which is the path where the
// model is told the tool failed rather than told what it returned.
function onAround(call) {
  if (call.tool === "ScriptEcho") {
    return { action: "deny", reason: "SCRIPT-TRACE-DENY: refused before dispatch" };
  }
  return null;
}
