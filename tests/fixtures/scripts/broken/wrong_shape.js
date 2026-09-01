// Answers in shapes its points cannot act on: one text block too many for a
// message that has one, and a number where a variable's value must be a
// string.
function onMessage(msg) {
  return msg.text.concat(["SCRIPT-TRACE-EXTRA-BLOCK"]);
}

function onVariable(ctx) {
  if (ctx === null) {
    return { name: "script_trace_var" };
  }
  return 42;
}
