// `model.request` — the knobs of a request on its way out.
//
// Handed the model name, the ceilings and the *names* of the tools and
// blocks — never the messages. Narrows the tool list, which is the one thing
// here a script can do that is visible in what the model receives.
function onRequest(req) {
  return { tools: req.tools.filter(function (t) { return t !== "ScriptEcho"; }) };
}
