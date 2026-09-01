// `prompt.assemble`, removing a block another script registered. Returning
// the list without a block is how a removal is expressed, so this hands back
// everything except `team.conventions`.
function onAssemble(blocks) {
  return blocks.filter(function (b) { return b.name !== "team.conventions"; });
}
