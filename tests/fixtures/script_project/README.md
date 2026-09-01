# `script_project`

A fixture project whose `.atta/settings.json` binds a script, for the case
that asks a real model whether a script's edit changed its behaviour.

The other script tests stop at "the text reached the model", which a scripted
model can answer. This one exists for the question a scripted model cannot:
the house rule is one no model follows unprompted and one that is obvious in
an answer, so a reply that ends with the line is a reply the script caused.

Run it with a recording round; see `tests/cases/010.script_carrier.test`.
