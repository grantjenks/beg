# Command Line Reference

```text
beg [OPTIONS] SCRIPT [ARG...]
```

`SCRIPT` is the path to a UTF-8 text file, up to 8 MiB. `beg` strips an
initial shebang before reading the description. Use `#!/usr/bin/env beg`
and mark the file executable to run it directly from the shell.

Interpreter options go before `SCRIPT`. Everything after the path belongs
to the script, including strings such as `--debug`. Put `--` before a
script path that starts with a hyphen.

```sh
beg --timeout 60s ./examples/topwords 10 < book.txt
beg -- -notes.md "two words" ""
```

Calling `beg` by itself reports a missing script path and exits `64`.
`beg -` looks for a file named `-`. Stdin is the script's input stream.

## Options

| Option | Behavior |
| --- | --- |
| `--model MODEL_ID` | Select a Codex model. The configured Codex default applies when omitted. |
| `--timeout DURATION` | Set a deadline for startup and execution. Use a duration such as `60s` or `2m`. |
| `--trace FILE` | Append execution events to a JSON Lines file. |
| `--debug` | Include Codex app-server events in the trace. Create a temporary trace if no path is set. |
| `--embed` | Store a prepared filter in the script. Takes the script path alone and returns after preparation and validation. |
| `--help` | Print usage to stdout and exit. |
| `--version` | Print the installed `beg` version to stdout and exit. |

`--model`, `--timeout`, and `--trace` also accept `--option=value`.

## Environment Variables

Environment variables configure invocations through a shebang. For example,
trace a script without adding arguments to its own command line.

```sh
BEG_DEBUG=1 BEG_TRACE=trace.jsonl ./examples/topwords 10 < book.txt
```

| Variable | Behavior |
| --- | --- |
| `BEG_DEBUG` | Include Codex app-server events, as with `--debug`. |
| `BEG_TRACE` | Append to this trace path. `--trace` takes precedence. |
| `BEG_OPEN_TRACE` | Open the trace in the default browser after the run. Create a temporary trace if needed. |
| `BEG_EXEC` | Use `inherit` to allow direct subprocesses with the caller's permissions. The default is `deny` for this process tool. |
| `BEG_ENV` | Comma-separated names the live `env_get` tool may read. The default list is empty. |
| `BEG_CODEX_HOME` | Existing directory for Codex settings and credentials. Otherwise use `CODEX_HOME` or `~/.codex`. |
| `CODEX_API_KEY`, `OPENAI_API_KEY` | Supply an API key for model requests. `beg` checks `CODEX_API_KEY` first. |
| `OPENAI_BASE_URL` | Set the Responses API base URL for explicit API-key configuration. |
| `XDG_CACHE_HOME` | Use this absolute path as the parent of the per-user `beg` cache. |

`BEG_DEBUG` and `BEG_OPEN_TRACE` accept `1` or `true` to turn them on.
Use `0` or `false` to turn them off.

For testing, `BEG_CODEX` selects an executable that serves the Codex
app-server protocol. [Development and Support][development]

The [Process Contract][process] covers stream behavior and exit statuses.
See [Caching][cache] for `--embed` and [Tracing][trace] for opening a
log after the run.

[process]: process.md
[cache]: caching.md
[trace]: tracing.md
[development]: development.md#testing
