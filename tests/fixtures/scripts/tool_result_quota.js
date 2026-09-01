// Marks each result with the argument its call carried, so a case can see
// which calls the script reached and which it ran out of budget for.
function onResult(result) {
  return "SCRIPT-TRACE-QUOTA(" + result.input.say + ") " + result.text;
}
