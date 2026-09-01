# Script fixtures

One script per extension point a script can be bound to, used by
`tests/runner/tests/script_carrier.rs` to prove that a `scripts` section in
`Settings` ends up changing what the model is sent.

They are committed unpackaged and read straight off disk — the same shape as
`tests/fixtures/plugins/demo-plugin`, minus the packaging step a plugin needs
and a script does not.

Each one leaves a mark no other part of the engine produces. That is the
whole design constraint: a fixture that logged, or that made a change the
engine would have made anyway, would let the test pass with the script never
running.
