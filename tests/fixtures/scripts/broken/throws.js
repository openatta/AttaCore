// Throws at whatever point it is bound to. One file for every point, because
// the promise being checked is the same one everywhere: a script that fails
// leaves its point exactly as it found it.
function boom() {
  throw new Error("this fixture always throws");
}
