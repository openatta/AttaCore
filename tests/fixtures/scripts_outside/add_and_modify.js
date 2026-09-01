// A script from outside the project, doing both things at once: adding a
// block, which its origin allows, and rewriting one that is already there,
// which it does not. Both halves are in one pass on purpose — a refused edit
// must not cancel the permitted ones.
//
// It edits `team.conventions` rather than a kernel block because that name
// belongs to exactly one block; several of the prompt's sections share the
// name `scene.skeleton`, and an edit to one of those is refused for a
// different reason (nothing can say which one), which would make this case
// pass without authority having anything to do with it.
function onAssemble(blocks) {
  var out = blocks.map(function (b) {
    if (b.name === "team.conventions") {
      return { name: b.name, content: b.content + "\nSCRIPT-TRACE-OUTSIDE-EDIT" };
    }
    return b;
  });
  out.push({ name: "outside.added", content: "SCRIPT-TRACE-OUTSIDE-ADD" });
  return out;
}
