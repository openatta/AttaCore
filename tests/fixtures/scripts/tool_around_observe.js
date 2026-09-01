// A harmless ring: every call is dispatched exactly as it would have been,
// with a deadline no call in a test can reach. It takes the script's answer
// without changing the outcome, which is what a profile of pure observers
// needs — a ring that returned nothing would be indistinguishable from a ring
// that was never called.
function onAround(call) {
  return { action: "proceed", timeoutMs: 30000 };
}
