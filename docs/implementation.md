# Implementation Details

`beg` keeps the Unix process in Rust and asks Codex to work out what
the script requires. Rust owns the file descriptors, native argument
values, child lifetimes, and final status. This lets us test those
parts with real pipes even when the backend is a fixture.
[Development and Support][development]

## Embedding Codex

The backend uses `InProcessAppServerClient` from
`codex-app-server-client`, pinned at commit
`581161ab41e643d942e1e1b5f561034060bf18e3`. The app-server client
provides requests and events inside the `beg` process. Live runs
start a fresh thread in the caller's working directory.
[Rust app-server client][client]

Codex loads its usual configuration, native tools, and sandbox
settings. `beg` sets the approval policy to `never` for
noninteractive execution. At thread creation, it supplies the process
operations as dynamic tools. The thread can use those operations for
program I/O and Codex's native tools for other work. Live model
inference still requires the configured model service.
[App-server protocol][protocol]

`beg` supplies the entry points used by the native shell and
filesystem helpers. On Linux it also dispatches the Codex sandbox
helper. The local filter runner uses the separate
`codex-code-mode-host` executable. [Installation][installation]

## The Rust host

| Module | Responsibility |
| --- | --- |
| `cli` | Parse interpreter options and preserve separate script arguments. |
| `process` | Load the script, service tool calls, handle deadlines and signals, and reap children. |
| `streams` and `unix` | Manage descriptors and preserve native process behavior. |
| `backend` | Start Codex, route requests and events, and cancel the turn on shutdown. |
| `policy` | Check which environment lookups and direct subprocesses `beg` permits. |
| `filter` and `cache` | Prepare, validate, run, and reuse local filters and their results. |
| `trace` | Write trace records and open the browser viewer. |

The supervisor runs while the backend works. It can handle a signal
during a model request or a blocked read. Tool protocol messages
travel separately from program streams. Native argument and path
handles let a child use the original OS value. Bytes carried in
a JSON tool result can be decoded from Base64.
[Process Contract][process]

## Process tools

These are the operations the live agent can use. Script authors
describe the task and its Unix behavior in English.

| Tool | Purpose |
| --- | --- |
| `process_context` | Read arguments, process IDs, working directory, stream metadata, and available operations. |
| `env_get` | Look up a named environment variable permitted by `BEG_ENV`. |
| `stream_read` | Read up to 16 KiB from stdin and report whether EOF was reached. |
| `stream_write` | Write UTF-8 or Base64-encoded bytes to stdout or stderr. |
| `stream_flush` | Confirm that no write is pending on an output stream. |
| `stream_close` | Close an available program stream. |
| `spawn` | Start a child with an argument array and explicit stream bindings. |
| `wait` | Get a child's exit code or terminating signal. |
| `signal_subscribe` | Register the signals a live script wants to handle. |
| `signal_send` | Send a signal to a managed process group. |
| `event_next` | Poll or wait for a signal or child exit. |
| `process_exit` | Commit the program's status and begin shutdown. |

`spawn`, `wait`, and `signal_send` are available with `BEG_EXEC=inherit`.
A direct child runs with the caller's permissions. Codex's native
commands use its configured sandbox. The runtime tells Codex to
treat argv, stdin, environment values, file contents, and tool results
as data.

In live Codex Code Mode, `beg` tool results are JSON strings. For
example, use `JSON.parse(await tools.process_context({}))` to get the
context object in generated JavaScript. Prepared filters have a smaller
interface with byte arrays and `io.env`, `io.read`, and `io.write`.
[Caching][caching]

## Related work

- Simon Willison's example inspired the English shebang.
  [Using LLM in the shebang line of a script][shebang]
- Andi AIRun runs executable Markdown and uses Unix pipes and
  redirection. [Andi AIRun][airun]
- The `topwords` example revisits the word-frequency problem from Jon
  Bentley's Programming Pearls column. Donald Knuth supplied a
  literate program, and Douglas McIlroy reviewed it with a shell
  pipeline. [Programming Pearls: A Literate Program][pearls]

[development]: development.md
[client]: https://github.com/openai/codex/blob/581161ab41e643d942e1e1b5f561034060bf18e3/codex-rs/app-server-client/src/lib.rs
[protocol]: https://developers.openai.com/codex/app-server
[installation]: tutorial.md#installation
[process]: process.md
[caching]: caching.md
[shebang]: https://til.simonwillison.net/llms/llm-shebang
[airun]: https://github.com/andisearch/airun
[pearls]: https://www.cs.tufts.edu/~nr/cs257/archive/don-knuth/pearls-2.pdf
