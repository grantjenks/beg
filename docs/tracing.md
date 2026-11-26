# Tracing

A trace is an execution log that you can open in a browser or read as
JSON Lines. It records the script digest, backend version, lifecycle
events, and process results. It also records the byte counts and token
usage available to `beg`.

## Recording a trace

Set `--trace` before the script path. `--debug` includes Codex
app-server events, including messages, raw response items, commands,
and tool calls and results.

```sh
printf 'Pear pear apple. APPLE pear!\n' |
  BEG_EXEC=inherit beg --trace /tmp/topwords.log --debug ./examples/topwords 2
```

A trace path is opened for append. If you use the same file twice, the
viewer lets you select a run. With `--debug` and no path, `beg` creates a
private temporary log and prints its path to stderr.

When you run a script through its shebang, set `BEG_TRACE` and
`BEG_DEBUG` in the environment. `BEG_OPEN_TRACE=1` opens the viewer
when the invocation finishes. If you give it no trace path, `beg`
creates one.

```sh
BEG_EXEC=inherit BEG_DEBUG=1 BEG_TRACE=/tmp/topwords.log BEG_OPEN_TRACE=1 \
  ./examples/topwords < README.md
```

## Opening a saved trace

Run the viewer from the checkout with Python 3.

```sh
python3 tools/view_trace.py /tmp/topwords.log
```

This writes a self-contained local HTML file and opens it in your
default browser. Use `--no-open` to write the file and print its path
without opening it. The viewer can also open another JSON Lines log
from disk.

## Reading the viewer

**Steps** groups execution into JavaScript calls, shell commands, and
cache operations. Open a step to see the code and the tools it called.
Tool details show decoded arguments and returned data when the trace
contains them.

**Events** shows individual records. Select a record to see its
details and raw JSON. Turn on **All events** to see low-level
notifications as well. When focus is on either list, Up and Down
select the previous or next item.

A number in a timeline marker counts nearby events grouped into that
marker. The ruler and the `+` times on rows show seconds since the
invocation started.

## What ran?

There are a few ways for `topwords` to produce its answer. A live model
might count a short input in a tool call. It might write JavaScript to
process chunks or start a shell command. A prepared filter can use the
local JavaScript host, and a replay can return saved bytes. The Steps
view shows the path the invocation took. [Caching][caching]

A native command's captured result can appear in a debug trace. A child
with a direct descriptor connection streams bytes straight to the
program's input or output. The trace records the child and its status.
The pipe contents may be available only at the other end of the pipe.

Open a reasoning event to see a summary if Codex supplied one. Some
events carry encrypted content with no readable summary. A trace
records the app-server events that reached `beg`. If delivery fell
behind, an event reports the number skipped.

Debug logs and the HTML files made from them can contain input,
environment values, and command output. Review them before attaching
one to an issue.

[caching]: caching.md
