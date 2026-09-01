// A second `tool.around` binding, so a case can tell which of two rings on
// one point is the outer one. Answers with a mark of its own; whichever mark
// the model sees is the ring that decided.
function onAround(call) {
  if (call.tool === "ScriptEcho") {
    return { action: "respond", text: "SCRIPT-TRACE-AROUND-SECOND: answered without dispatch" };
  }
  return null;
}
