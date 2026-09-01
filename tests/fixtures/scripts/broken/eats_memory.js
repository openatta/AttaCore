// Grows until the runtime's memory ceiling stops it. Given a generous clock,
// so that what fails the call is the ceiling and not the deadline.
function boom() {
  var held = [];
  for (;;) {
    held.push(new Array(4096).join("x"));
  }
}
