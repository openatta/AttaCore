// `model.message` — a completed message coming back.
//
// Sees the message's text blocks and the names of the tools it asked for.
// Not thinking blocks, whose signatures have to be echoed back verbatim on
// the next request; not tool arguments; not images. Returns one replacement
// per text block, or nothing at all — a shorter array is discarded rather
// than applied to a guess about which block it meant.
function onMessage(msg) {
  return msg.text.map(function (t) {
    return "SCRIPT-TRACE-MESSAGE " + t;
  });
}
