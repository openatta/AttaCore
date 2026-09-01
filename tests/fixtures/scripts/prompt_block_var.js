// A block that mentions `{{script_trace_var}}`, so that the variable point
// has somewhere to be seen. On its own it proves nothing; it is here so the
// `prompt.variable` case can.
function onBlock() {
  return {
    name: "team.variable_host",
    order: 255,
    content: "trace slot: {{script_trace_var}}",
  };
}
