// `prompt.variable` — what `{{name}}` expands to.
//
// The identity call names the variable; later calls answer with its value.
// Only a string is a value: anything else leaves the placeholder visible,
// which is the point's own rule about unresolved variables being bugs to see.
function onVariable(ctx) {
  if (ctx === null) {
    return { name: "script_trace_var" };
  }
  return "SCRIPT-TRACE-VARIABLE(" + ctx.os + ")";
}
