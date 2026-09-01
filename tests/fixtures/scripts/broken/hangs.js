// Never returns. The carrier's claim is that a deadline reaches inside the
// interpreter — a future that is merely abandoned would leave this running.
function boom() {
  while (true) {}
}
