// `prompt.block` — a static block the script names and places itself.
//
// Called once with `null` to ask what it is, and never again: the content is
// fixed at registration, which is what makes this the cheap contribution
// point.
function onBlock() {
  return {
    name: "team.conventions",
    order: 250,
    content: "SCRIPT-TRACE-BLOCK: keep answers short.",
  };
}
