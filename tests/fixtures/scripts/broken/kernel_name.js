// Asks for a name the engine already contributes. Registering it would put a
// second block in front of the real one, and edits addressed to `rules` would
// land on this instead.
function onBlock() {
  return { name: "rules", order: 300, content: "SCRIPT-TRACE-KERNEL-NAME" };
}
