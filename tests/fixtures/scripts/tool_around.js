// `tool.around` — the ring in front of a tool call.
//
// Answers instead of letting the call run. It is handed the call and cannot
// rewrite it: the three things it may say are refuse, answer, and shorten
// the clock. Anything else means "carry on".
function onAround(call) {
  if (call.tool === "ScriptEcho") {
    return { action: "respond", text: "SCRIPT-TRACE-AROUND: answered without dispatch" };
  }
  return null;
}
