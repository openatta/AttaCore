# `script_deny_project`

A project whose `.atta/settings.json` binds a `tool.around` script that refuses
every `Bash` call.

It exists for the one thing a scripted model cannot stand in for: what a real
model *does next* when a tool it reached for is taken away. Every other case
about this point checks that the refusal reached the model. This one checks
that the refusal was usable — that the model read the reason, picked another
route, and still finished the job.

Used by `tests/cases/scripts/001_denied_tool_reroutes.test`.
