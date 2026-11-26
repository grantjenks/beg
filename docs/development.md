# Development and Support

## Reporting issues

Search the issue tracker for an existing report. If you open a new
issue, include the script, the command that ran it, the platform, and
the output of `beg --version`. Include the expected stdout, stderr,
and exit status where they matter. A small example that reproduces
the problem makes it easier to turn the report into a test.
[GitHub Issue Tracker][issues]

For a problem with a live run, include the model and the relevant
events from its trace. [Tracing][tracing]

## Get the code

```sh
git clone https://github.com/grantjenks/beg.git
cd beg
cargo build --locked
```

See the [Tutorial][installation] for build prerequisites and the
matching code-mode host.

## Testing

Run the default suite from the checkout.

```sh
cargo test --locked
```

The process tests use a controlled app server selected with the
`BEG_CODEX` executable override. The fixture runs on private protocol
pipes. The script's stdin, stdout, and stderr are real Unix descriptors.
These tests exercise binary data, short reads, argv, signals, child
cleanup, and deadlines without paid model calls.

With `codex-code-mode-host` beside the built `beg`, include the tests
that prepare and reuse a filter, replay a CGI response, and run an
embedded program from a read-only copy.

```sh
cargo test --locked --test process_contract -- --include-ignored
```

The embedded-runtime tests use localhost listeners and synthetic
API keys to exercise native commands and dynamic tools. The filter
test in this suite also uses the matching host.

```sh
cargo test --locked --test embedded -- --ignored
```

Use live runs to test what a model makes of the English scripts.
For `topwords`, compare stdout and the exit status to a deterministic
reference. Include empty input, tied counts, invalid limits, and
files larger than the model context. Measure latency, model calls,
token usage, and bytes sent to the model separately from correctness.
Repeat the runs with the model and settings recorded so the results
can be compared.

## Contributing

1. Search the issue tracker or open an issue to discuss the change.
2. Make your changes in a branch.
3. For a bug fix, include a test that reproduces the bug.
4. Open a pull request with the test results.

## Open questions

Packaging and the project license are open. A Windows port would
need to account for its process handles and signal semantics.
Full terminal job control and other agent backends are areas for
later work.

[issues]: https://github.com/grantjenks/beg/issues
[tracing]: tracing.md
[installation]: tutorial.md#installation
