// One file, two bindings. Each binding gets a carrier of its own, so what
// this fixture is for is showing that they get budgets of their own too.
function onAround(call) {
  return { action: "proceed", timeoutMs: 30000 };
}

function onResult(result) {
  return "SCRIPT-TRACE-TWO-POINTS " + result.text;
}
