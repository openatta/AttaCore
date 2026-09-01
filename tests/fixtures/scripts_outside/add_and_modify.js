// A script from outside the project, doing both things at once: adding a
// block, which its origin allows, and rewriting the ones already there, which
// it does not. Both halves are in one pass on purpose — a refused edit must
// not cancel the permitted ones.
function onAssemble(blocks) {
  var out = blocks.map(function (b) {
    return { name: b.name, content: b.content + "\nSCRIPT-TRACE-OUTSIDE-EDIT" };
  });
  out.push({ name: "outside.added", content: "SCRIPT-TRACE-OUTSIDE-ADD" });
  return out;
}
