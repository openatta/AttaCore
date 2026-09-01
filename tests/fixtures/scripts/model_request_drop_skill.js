// `model.request`, withdrawing a tool the model is about to call anyway.
// Narrowing the offered list is not the same as gating dispatch, and this
// fixture is how a case can say which one the engine does.
function onRequest(req) {
  return { tools: req.tools.filter(function (t) { return t !== "Skill"; }) };
}
