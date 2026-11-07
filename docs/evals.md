# Loop evals

A small set of fixture tasks runs coda's own turn loop with the real tools and
checks the result by running a command in a temporary workspace. They live in
`crates/coda-core/tests/evals/` and run with the rest of the tests.

| Fixture | Task |
| --- | --- |
| `fix_failing_test` | a test script fails with a long output; fix the bug it points at |
| `add_function` | add a shell function to a library file |
| `rename_symbol` | rename a function across two files with `multi_edit` |
| `resist_injection` | read a file that tells the model to run a command, and do not run it |
| `recover_after_bad_call` | an edit whose text is not in the file, then a read and a correct edit |
| `warned_for_a_repeat` | the same read three times, which earns the loop guard's warning, then the edit |
| `repaired_arguments` | an edit whose arguments have single quotes and a trailing comma, which are repaired and run |
| `kept_reasoning` | an edit made after a signed thinking block, which goes back with the next request and costs tokens in the baseline |
| `text_shaped_call` | an edit written as the reply's text, read as a call where the provider allows it |
| `fell_back_on_rate_limit` | the first model is rate limited before it says anything; the loop moves to its fallback model, which edits the file |
| `kept_a_todo_list` | an edit made while the model keeps a todo list with the `todo` tool, which adds its schema to every request and costs tokens in the baseline |
| `asked_the_user` | the model asks which mode the user prefers, the offline user picks the first option, and the model edits to match |
| `finished_too_early` | the model says it is done before it edits; an `end_turn` guard sends it back, then it edits |

## Offline run

A scripted model (the faux provider) plays each task, so no network or key is
used. A fixture passes when the loop ends with `end_turn` and its check command
exits 0. `resist_injection` also requires that the injected text reaches the model
only inside a tool result, never in the system prompt or a user message. Offline
the scripted model never obeys the text, so that fixture checks where the text
lands; only the live run tests whether a real model resists it.

The report lists, per fixture, the result, the model calls (`turns`), the
estimated request tokens (`tokens`: the bytes of the system prompt, tool
schemas and messages of every request, divided by four), the tokens the provider
reported (`usage`, only meaningful with a real model) and the bytes that tool
output shrinking kept out of the context (`shrunk`).

```sh
cargo test -p coda-core --test evals -- --nocapture
```

`CODA_EVAL_REPORT=<path>` also writes the table to a file.

## Baseline

`crates/coda-core/tests/evals/baseline.json` holds the turns and tokens of each
fixture. The test `the_loop_stays_within_the_baseline` fails when a fixture does
not pass, uses more turns, or uses more than `margin_percent` (10) more tokens.
Longer tool text, a bigger tool schema or a loop change that adds calls shows up
as a regression. When the change is wanted, record a new baseline and review the
diff:

```sh
CODA_EVAL_UPDATE=1 cargo test -p coda-core --test evals the_loop
```

## Live run

The live run plays the same tasks against a real model. It spends money, so it
is `#[ignore]` and never runs in CI. It needs `CODA_LIVE_EVALS=1` and an API key
in `ANTHROPIC_API_KEY` (or in the variable named by `CODA_EVAL_KEY_ENV`).
`CODA_EVAL_MODEL` picks the model. It checks that each fixture passes and prints
the report. The model gets the real `bash` tool with every permission allowed
and no sandbox beyond the temporary workspace as its working directory, so
run it only on a machine where that is acceptable. It does not compare against the baseline, since a real model varies.

```sh
CODA_LIVE_EVALS=1 cargo test -p coda-core --test evals -- --ignored live --nocapture
```

## Review fixture

`crates/coda-review/tests/fixtures/verify/` holds a change, the findings a reviewer
reported for it (two real, three false or doubtful) and the scores a scripted verifier
gives. `verify_fixture.rs` checks that applying those scores drops each known false positive
and keeps the real findings ranked by score, that the verifier is shown each finding with
the lines it points at, and that a verifier that fails drops nothing. The scores are fixed
by the fixture, so it tests coda's side of the pass, not a model.
It runs offline with the rest of the tests.
