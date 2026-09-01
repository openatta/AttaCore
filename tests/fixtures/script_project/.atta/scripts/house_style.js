// Adds a house rule to the assembled prompt.
//
// Deliberately a rule the model would not follow on its own and that is easy
// to see in an answer: the point of the real-model case is whether a script's
// edit changes what the model *does*, not whether the text arrived.
function onAssemble(blocks) {
  return blocks.concat([
    {
      name: "team.house_style",
      content:
        "House rule, absolute: end every reply with the line " +
        "'-- checked by the house style script'. No exceptions.",
    },
  ]);
}
