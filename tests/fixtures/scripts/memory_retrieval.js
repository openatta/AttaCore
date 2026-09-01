// `memory.retrieval_hook` — both halves, off the phase it is given.
//
// Before: rewrite the question, so recall finds a memory the user's own
// words never would. After: drop anything scoped `secret-`, so a memory the
// rewritten question did find still does not reach the model.
function onRetrieval(recall) {
  if (recall.phase === "before") {
    return { query: "SCRIPT-TRACE-RECALL" };
  }
  return recall.names.filter(function (name) {
    return name.indexOf("secret-") !== 0;
  });
}
