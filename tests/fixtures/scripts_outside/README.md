# Scripts from outside the project

One script, kept deliberately outside `tests/fixtures/scripts/` — which is the
project root the boundary cases bind against.

Authority follows the file's location, not anything the binding declares,
because a declaration is what a script from outside would lie in. So the only
way to exercise the reduced mode is to have a file that really is somewhere
else, and a directory is the whole apparatus needed to do it.
