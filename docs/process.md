# Process Contract

A `beg` invocation is a Unix process. Its parent can supply arguments and
input, redirect its output, wait for a status, or send a signal.

## Arguments and paths

`argv[0]` is the script path supplied to `beg`. The remaining entries are
the script arguments, in order. In this example there are four.

```sh
./inspect "two words" "" "--help" "*"
```

The empty string is an argument and the quoted asterisk is literal. The
host preserves native Unix bytes in arguments, paths, and environment
values. Tools provide a lossless byte representation and handles as
well as readable text. A child can use a handle to pass on the original
value.

The working directory is the caller's working directory. Run a script
from two directories and a relative path in its arguments resolves from
the corresponding directory each time.

## Standard streams

Stdin is a live byte stream. A script can read a chunk and act on it
while the producer is still running. A short read can occur before EOF,
and a UTF-8 character can span multiple reads. For a filter that needs
the entire input, say when to wait for EOF and when to write.

Stdout and stderr carry program output and diagnostics. `beg` preserves
bytes, including NULs and an absent final newline. Each stream has its
own ordering. Data already written is visible to the reader even if the
run fails later. To replace a file only after success, write a temporary
file and rename it when ready.

A debug trace can include Codex messages and captured native tool
output. Explicit writes and connected child descriptors carry the
program's output. Interpreter errors go to stderr with a `beg:` prefix.
[Tracing][tracing]

## Environment and child processes

A script can use the caller's environment. For a live run, `BEG_ENV`
lists the names available through the `env_get` process tool. A prepared
filter can read any variable through `io.env()`. Each invocation supplies
the current values. [Caching][caching]

Native Codex commands run under the configured Codex sandbox and return
captured output to Codex. `BEG_EXEC=inherit` also makes the direct child
tool available. A direct child inherits the caller's permissions and can
inherit its environment. The script can connect the child's stdin,
stdout, and stderr to its own descriptors. Unbound child streams use
`/dev/null`.

While a child owns stdin, the agent cannot read from it. A child attached
after the agent has read a prefix gets the remaining bytes. The host
tracks the child and distinguishes an exit code from termination by
signal.

## Exit status

A script can return any integer from `0` through `255`. If you are writing
a filter, choose what a caller should see for success, an empty result,
and an error. For example, [findlines][findlines] uses `0` for a match,
`1` for no match, and `2` for invalid arguments or an input error.

The live agent commits a status through `process_exit`. Codex is
instructed to choose `0` on success when the script gives no convention.
If the turn finishes without committing a status, `beg` reports
`Codex finished without calling process_exit` and exits `70`.

| Status | Interpreter condition |
| --- | --- |
| `64` | Invalid invocation or configuration value. |
| `66` | Script source could not be read or decoded. |
| `69` | Authentication or model service failure. |
| `70` | Host or backend failure, including a missing terminal result. |
| `124` | Invocation deadline expired. |

Scripts may choose the same numbers. Use stderr or a trace to identify
an interpreter error.

## Signals and shutdown

The host handles `SIGINT`, `SIGTERM`, `SIGHUP`, and `SIGQUIT`. By
default, it interrupts work, cleans up managed children, and terminates
with the signal. A shell commonly reports `130` for `SIGINT`. A parent
using `waitpid` can tell signal termination apart from an ordinary exit
with that number.

A live script can ask to finish a record or save progress on a signal.
Once the agent subscribes, the first matching signal gives it five
seconds to finish cleanup. A repeat signal can end that grace period.
Signals during startup use the default behavior.

When a downstream reader closes a pipe, `beg` uses Unix-style
`SIGPIPE` behavior. On shutdown it cancels pending operations and reaps
managed children. Full interactive job control and terminal emulation
are future work.

[findlines]: ../examples/findlines
[tracing]: tracing.md
[caching]: caching.md
