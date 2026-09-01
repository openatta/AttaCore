// `tool.result` — prefixes the result with a marker and the tool's name, so
// the trace also proves the script was told which call it was looking at.
function onResult(result) {
  return "SCRIPT-TRACE-RESULT(" + result.tool + ") " + result.text;
}
