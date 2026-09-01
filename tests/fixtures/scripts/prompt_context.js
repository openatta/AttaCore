// `prompt.context` — a block whose text is computed every assembly.
//
// The identity call gets `null` and answers with a name and a place; every
// call after that gets the environment and answers with text. The mark
// carries `cwd`, which the identity call cannot see — so a block registered
// with static content at bind time would come out without it.
function onContext(ctx) {
  if (ctx === null) {
    return { name: "project.status", order: 260 };
  }
  return "SCRIPT-TRACE-CONTEXT: working in " + ctx.cwd;
}
