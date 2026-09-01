// `prompt.assemble` — appends a block nothing else in the engine emits, so
// its presence in the system prompt can only mean this ran.
function onAssemble(blocks) {
  blocks.push({
    name: "script.fixture.assemble",
    content: "SCRIPT-TRACE-ASSEMBLE"
  });
  return blocks;
}
