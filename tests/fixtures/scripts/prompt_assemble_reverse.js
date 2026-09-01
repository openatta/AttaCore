// Hands the same blocks back in the opposite order and changes nothing else.
// Position comes from where a block was registered, not from where it sits in
// this array, so this is a pass that asks for nothing.
function onAssemble(blocks) {
  return blocks.slice().reverse();
}
