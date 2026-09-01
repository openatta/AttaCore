// `model.request` — the knobs of a request on its way out.
//
// Handed the model name, the ceilings and the *names* of the tools and
// blocks — never the messages. Narrows the tool list, which is the one thing
// here a script can do that is visible in what the model receives.
//
// The tool it drops has to be one the session actually offers: dropping a
// name that was never in the list would leave the request identical to the
// one no script touched, and the case would pass without the script running.
function onRequest(req) {
  return { tools: req.tools.filter(function (t) { return t !== "WebSearch"; }) };
}
