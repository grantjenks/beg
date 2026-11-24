# beg

**Run English scripts as Unix processes.**

`beg` is a proposed interpreter for scripts written in natural English. Put `#!/usr/bin/env beg` on the first line, describe the task, and run the file from your shell.

The script gets an argument array, streaming standard input, separate standard output and standard error, an exit status, and a process lifecycle. Its instructions can describe how to use those facilities, including how to respond to signals.

The implementation language is Rust. The preferred first backend is an embedded Codex runtime. The process interface belongs to `beg`, so another agent backend can implement the same contract later.

**Status: project design.** There is no released implementation yet. The examples below describe intended behavior. The process model is the starting commitment; CLI flags, internal tool names, and implementation milestones are proposals.

## Quick start

Save this as `topwords`:

```text
#!/usr/bin/env beg

Read text from stdin and print its most frequent words.

Accept zero or one argument. With no argument, print up to 10 words.
Otherwise, the argument must be a positive decimal integer giving the limit.
For any other arguments, write a short usage message to stderr and exit 2.

A word is a maximal run of ASCII letters A through Z or a through z.
Treat uppercase and lowercase as equivalent.
Count every occurrence exactly.

Print one line per word: the decimal count, a tab, and the lowercase word.
Sort by decreasing count, breaking ties alphabetically.
Print no heading, explanation, or other text.
Empty input produces no output.

Use local tools or generated code to do the counting.
Exit 0 after successful completion, or 1 if the input cannot be processed.
```

With `beg` installed on `PATH`, the intended usage is:

```sh
chmod +x topwords
./topwords 10 < book.txt > counts.tsv
```

A small example:

```sh
printf 'Pear pear apple. APPLE pear!\n' | ./topwords 2
```

Expected stdout:

```text
3	pear
2	apple
```

The model can choose a shell pipeline or write a counting program. The data can flow directly through that program without placing the entire input in the model context. The result must be computed, and the surrounding process must behave as specified.

## Why beg

English is a useful way to state a task together with its operating rules. A short script can describe its input format, output format, argument handling, failure conditions, and shutdown behavior without requiring the author to implement each mechanism.

`beg` makes that description executable within the shell. A caller should be able to pipe into it, redirect its output, inspect its status, and terminate it using familiar process operations.

This is also a way to explore the Unix process interface. Students can write useful filters and small services, then change their instructions to experiment with arguments, EOF, buffering, signals, and exit values. Their scripts remain task-oriented programs. Learning happens through running and composing them.

The short name fits a shell command and carries a small joke about asking a machine to do something.

## Running scripts

The proposed command shape is:

```text
beg [OPTIONS] SCRIPT [ARG...]
```

Examples:

```sh
beg topwords 10 < book.txt
beg --model MODEL_ID --timeout 60s topwords 10 < book.txt
beg -- ./-unusual-name --literal-argument
./topwords 10 < book.txt > counts.tsv 2> errors.log
```

Options before `SCRIPT` configure the interpreter. Every argument after `SCRIPT` belongs to the script, including values that look like `beg` options. A `--` before the script ends interpreter option parsing.

The initial option set should stay small:

| Option | Purpose |
|---|---|
| `--model MODEL_ID` | Select the model used by the configured backend. |
| `--timeout DURATION` | Bound the total invocation, including model and tool work. |
| `--trace FILE` | Write execution events to a separate file. |
| `--help` and `--version` | Describe the interpreter without starting the agent. |

The script is a UTF-8 text file. Remove its initial shebang line before supplying the instructions to the agent. No front matter, special extension, or embedded programming syntax is required.

For the first version, the program source comes from a file path. Standard input remains the program's input stream. A separate source descriptor can be considered later.

Model authentication and permission policy are configured before execution. A piped invocation must never consume program input to answer a login prompt or an approval question.

## Process contract

The runtime owns the operating-system resources. The agent receives structured access to those resources and implements the script's intent.

| Facility | Contract |
|---|---|
| Arguments | Preserve boundaries, order, empty arguments, and native OS values. |
| Standard input | A live byte stream, available on demand until EOF. |
| Standard output | Program output only, written explicitly or by a connected child process. |
| Standard error | Program diagnostics and concise interpreter errors. |
| Environment | Inherited process environment, available through controlled lookup. |
| Working directory | The caller's working directory unless an explicit operation changes it. |
| Exit status | An explicit process result, distinct from the agent's conversational completion. |
| Signals | Host-managed delivery, default actions, and registered script handlers. |
| Children | Explicit stream wiring, tracked lifetimes, and reaping. |

### Arguments and paths

Expose script-level `argv` as an array. `argv[0]` is the script path as supplied to `beg`; `argv[1..]` contains the script's arguments. Keep the interpreter's original launch vector separately for diagnostics. Do not claim to recover an original `argv[0]` that the operating system discarded while processing a shebang.

For example:

```sh
./inspect "two words" "" "--help" "*"
```

The script receives four arguments after its name. The empty string remains an argument, and the quoted asterisk remains a literal asterisk.

Use Rust's OS-native argument and path types. On Unix, argument values and paths can contain bytes that are not valid UTF-8. Preserve those bytes in the host. Tool responses can expose readable text where possible and a lossless byte representation or opaque handle otherwise.

A readable rendering is not the authoritative value. Passing an argument or path to a child should be able to reuse its host handle without having the model reconstruct it.

### Streaming input

Do not read stdin to completion during startup or append it wholesale to the prompt.

The process interface should support bounded byte reads, incremental text decoding, EOF detection, and cancellation. A short read does not imply EOF. UTF-8 sequences may cross chunk boundaries.

The agent can inspect selected chunks or connect the stream to a child program. Large inputs should normally travel through native tools or generated code. Input size is therefore not inherently limited by the model's context window.

Each stream has one defined reader at a time. Transferring stdin to a child must prevent competing reads by the agent. If the agent has already consumed a prefix, the child receives the remaining input unless the script explicitly requests a replay arrangement.

Backpressure and bounded buffers matter: a slow reader must not cause unlimited buffering. A blocked input read must remain cancellable while the host continues servicing signals and deadlines.

### Output and diagnostics

Only explicit output operations and connected child descriptors write program stdout. Assistant commentary, tool envelopes, startup banners, progress indicators, and final conversational messages must stay off stdout.

Program stderr is independently writable. Normal execution should add no interpreter chatter there. Fatal host errors may write a concise diagnostic prefixed with `beg:`. Optional traces go to the configured trace file, so scripts can use stderr as part of their own interface.

Preserve bytes, including NULs, non-UTF-8 data, and missing final newlines. Do not wrap output in Markdown or append a newline automatically.

Writes are ordered within each stream. There is no promised total ordering between stdout and stderr. Prefer one writer per stream; connecting multiple children to the same descriptor permits the same interleaving concerns as ordinary Unix programs.

Output is incremental and cannot be rolled back. A failed run may already have produced output. Scripts that need an all-or-nothing file should write a temporary file and rename it after success.

### Environment and working directory

Inherit the caller's working directory and OS environment. Keep environment values lossless in the host and let the agent request the values it needs.

Do not automatically dump the entire environment into the prompt. Subprocess inheritance and disclosure to the model are separate operations.

Start a fresh agent session for each invocation. Keep any configuration that can influence execution explicit and inspectable. Repository instructions, plugins, prior conversations, and user preferences must not silently change the meaning of an otherwise identical script invocation.

## Exit status and lifecycle

The agent ends a script through an explicit host operation such as `process_exit(code)`. A script author need not mention this operation: the runtime's instructions require the agent to choose a status, with zero for successful completion when the script defines no other convention.

A completed assistant message alone is not proof that the program succeeded. If the backend stops without a valid terminal result, treat that as a runtime failure.

The proposed host defaults are:

| Condition | Result |
|---|---|
| Successful script completion | `0`, unless the script specifies another result |
| Script-defined failure or outcome | The script's explicit status |
| Invalid interpreter invocation | `64` |
| Script file cannot be read | `66` |
| Authentication or model service unavailable | `69` |
| Backend protocol failure or missing terminal result | `70` |
| Interpreter deadline exceeded | `124` |
| Unhandled terminating signal | Preserve signal termination where the platform permits |

Accept explicit exit values from 0 through 255. Do not silently truncate an out-of-range value. These host defaults are conventions, not a promise that application statuses occupy a disjoint range.

A normal invocation proceeds through startup, execution, shutdown, and exit. During execution, tool calls, child processes, stream readiness, and signal events can all require attention. Finishing one model turn does not necessarily finish the process.

Shutdown stops new work, resolves or cancels pending operations, flushes accepted output within a bounded deadline, closes descriptors, terminates remaining managed children as needed, reaps them, and shuts down the backend. The first version does not offer detached child processes.

A script can request a child's exact exit code or signal result and decide how to propagate it. Preserve those two cases separately in the host API.

### Signals

The host handles OS signal reception independently of model latency. Never call a model or perform complex work inside a low-level signal handler.

Provide prompt-independent defaults for interruption and termination. Script instructions may cause the agent to register a handler that receives signal events and performs application-specific cleanup.

A handler takes effect after registration. Signals received during startup use the default behavior; prose cannot retroactively install a handler. Cleanup has a bounded grace period, and a repeated interrupt can force termination.

For an unhandled signal, preserve native termination semantics where possible. For example, a shell commonly displays 130 for SIGINT, but an ordinary `exit(130)` is distinguishable from termination by SIGINT to a parent inspecting the wait status.

Handle a closed output pipe deliberately. If a downstream reader such as `head` exits, cancel unnecessary work and use Unix-style SIGPIPE behavior without a traceback.

SIGKILL and SIGSTOP cannot be handled by the script. Abrupt host death may require an external supervisor or OS containment to guarantee child cleanup. Full interactive job control and terminal emulation are later work; the first target is well-behaved filters and batch processes.

## Demonstrations

### The ten most frequent words

The leading demonstration revisits the word-frequency problem in Jon Bentley's 1986 Programming Pearls column, with Donald Knuth's literate program and Douglas McIlroy's Unix pipeline.

| Approach | What the author supplies |
|---|---|
| Knuth | An implementation developed and explained through literate programming. |
| McIlroy | A composition of existing text-processing commands. |
| beg | A task specification that an agent realizes using the process interface and available tools. |

The progression is an implementation, a composition, then an executable statement of intent. `beg` might choose essentially McIlroy's pipeline internally. That is part of the demonstration: the author can state the desired result while the runtime supplies an implementation.

Use the `topwords` script above as the first acceptance example. Its token rules, tie ordering, output format, and error behavior are explicit so implementations can be compared.

Check empty input, mixed case, punctuation, tied counts, fewer than ten distinct words, invalid arguments, and input much larger than a context window. Compare exact output and status against a deterministic reference program. Measure wall time, model calls, tokens, and how much input actually reaches the model.

The expected tradeoff is lower authoring effort with additional latency and model cost. Correctness and performance are measurements to make, not assumed benefits.

### Learning the shell interface

A student can write a useful filter before learning a programming language's argument parser, stream library, and error-handling syntax.

For example, `findlines`:

```text
#!/usr/bin/env beg

Accept exactly one argument: a literal, case-sensitive search string.
With any other number of arguments, write usage to stderr and exit 2.

Read stdin and copy matching lines to stdout unchanged.
Preserve a final matching line even if it has no terminating newline.
Do not print anything for nonmatching lines.

Exit 0 if at least one line matched.
Exit 1 if no line matched.
Exit 2 if an input error prevents completion.
Use the default broken-pipe behavior if the output reader closes.
```

Run it with quoted arguments, redirected files, a pipe, empty input, and input that never closes. Inspect its exit status and change the instructions to alter its behavior.

Further exercises can specify when to flush output, what to write to stderr, whether to stop after the first match, and what to do when SIGTERM arrives. For instance: finish the current complete record, report the number processed to stderr, then exit zero.

The lesson is the observable process behavior. The script is still carrying out a task.

### A CGI program

A CGI-style server provides request metadata through the environment, a request body through stdin, and expects the response on stdout. That makes it a compact demonstration of the process contract.

```text
#!/usr/bin/env beg

Serve one CGI request.

For GET, take the first name parameter from the URL-decoded query string.
Use "world" when it is absent.
Return status 200 with a plain-text body saying "Hello, " followed by
the name and a newline.

For other methods, return status 405, an Allow: GET header,
and a short plain-text error body.

Write CGI headers to stdout using CRLF line endings, followed by a
blank line and the body. Use Content-Type: text/plain; charset=utf-8.
Put diagnostics only on stderr.

Exit 0 if the response was emitted successfully, including a 405 response.
Exit 1 if the response could not be emitted.
```

Each request starts an invocation, so model startup and reasoning make this deliberately slow. Begin with a local demonstration under a constrained execution policy. HTTP response status and Unix process exit status are separate concepts.

### A toy virtual machine

Another demonstration is a small simulated computer: read commands from stdin, maintain an imaginary filesystem and process table, and write responses to stdout. An `exit` command ends the session with the requested status.

The script can specify that all files and processes are simulated and that commands must never operate on the host. Keep simulated state in an explicit structure as the session grows.

This is a toy emulator. A model's simulation is not an isolation boundary; actual isolation comes from the host's sandbox and permissions.

## Rust implementation

Start with a single Rust binary and clear module boundaries. The first implementation should prove the process contract before adding many backends or a configuration language.

| Component | Responsibility |
|---|---|
| CLI and script loader | Parse interpreter options, preserve script arguments, and load the instructions. |
| Process supervisor | Own state transitions, deadlines, signals, terminal results, and child lifetimes. |
| Stream broker | Manage descriptors, byte reads and writes, ownership transfer, EOF, and backpressure. |
| Backend adapter | Start the agent, service tool calls, receive events, cancel work, and shut down. |
| Execution policy | Enforce filesystem, network, subprocess, and tool permissions. |
| Trace writer | Record operational events separately from program output. |

The backend interface should cover start, event delivery, tool responses, cancellation, and shutdown. Keep OS resources behind host-owned handles so backend replacement does not change process semantics.

Use an asynchronous event loop for concurrent I/O and lifecycle work. Preserve Rust's OS-native argument and environment values until a deliberate conversion is needed. Represent child exit codes and terminating signals as different outcomes.

### Embedding Codex

The preferred initial integration is Codex's Rust app-server client facade. The upstream `codex-app-server-client` crate exposes `InProcessAppServerClient`, which wraps an in-process app server and is used by Codex's CLI surfaces. Its request and event interface avoids parsing terminal output.

Pin an upstream commit and place the integration behind the backend adapter. The [upstream facade source](https://github.com/openai/codex/blob/44308610ef5cbc03d149707735878b0c8743b2df/codex-rs/app-server-client/src/lib.rs) is an implementation reference, not a promised stable third-party SDK.

The [app-server protocol](https://developers.openai.com/codex/app-server) supports experimental dynamic tools supplied at thread creation, with tool calls routed back to the client. They require experimental API opt-in and are a candidate integration point for `beg`'s process operations.

The integration prototype must establish:

- How to start a fresh thread with the intended instructions, model, and configuration.

- How to register process tools and service them while consuming backend events.

- How to prevent built-in execution paths from bypassing stream ownership or host policy.

- How to interrupt pending work and shut down without stray output or orphaned children.

- Which upstream helper executables, sandbox entry points, and configuration facilities must accompany the embedded runtime.

Embedding the agent does not make model inference local. Authentication and model connectivity still depend on the chosen backend configuration.

A separate app-server process is a fallback transport if embedding proves impractical. It must use private control pipes, leaving the script's stdin, stdout, and stderr under `beg`'s control. A Claude Code adapter can follow once the contract is demonstrated with Codex.

### Process tools

The following names illustrate the host API; they are not syntax that script authors must learn.

| Operation | Purpose |
|---|---|
| `process_context` | Obtain script argv, process IDs, working directory, descriptor and terminal metadata, and capabilities. |
| `env_get` | Retrieve a named environment value without dumping the environment. |
| `stream_read` | Read a bounded amount of data and distinguish EOF from a short read. |
| `stream_write` | Write bytes to an authorized stream and report the amount accepted or the error. |
| `stream_flush` and `stream_close` | Flush buffered writes or close an owned stream end. |
| `spawn` | Start a child with an executable, argument array, environment, and explicit stream bindings. |
| `wait` | Wait for a child and retrieve its exit or signal result. |
| `signal_subscribe` | Register script-level interest in supported signals. |
| `signal_send` | Send an allowed signal to a managed child or child process group. |
| `event_next` | Wait for a lifecycle event without blocking the host's control loop. |
| `process_exit` | Commit the script's terminal status and begin shutdown. |

Pass child arguments as an array by default. A shell command is an explicit execution mode when shell syntax is useful.

Control messages use the backend protocol. Program bytes use descriptors or byte buffers. If binary data crosses a JSON tool boundary, encode it losslessly; do not treat arbitrary bytes as text.

Long-running children need asynchronous handles. The host must continue draining relevant pipes and servicing cancellation while waiting. Avoid deadlocks where a child waits for output to drain while the agent is waiting for that child to exit.

Any write or externally visible action can have consequences before the final exit. A failed model request must not automatically replay a whole invocation. Track tool calls and reject duplicate terminal operations; define retry behavior explicitly.

## Permissions and reproducibility

A `beg` script is executable code expressed in English. Filesystem access, network access, and subprocess execution must be governed by a host policy. Host-defined tools must enforce that policy themselves; adding a tool to an agent does not automatically place its implementation inside the agent's sandbox.

Keep script instructions separate from stdin, argument values, file contents, and subprocess output. Those inputs are data, including when they contain sentences that look like instructions. This separation supports correct interpretation, but the model is not a security boundary.

The runtime should make execution inspectable. An optional trace can record the script digest, backend and model identifiers, relevant configuration, tool calls, child results, and terminal status. Avoid copying credentials or all input bytes into traces by default.

The host can enforce byte preservation, stream ownership, allowed operations, and status handling. The agent's interpretation and chosen implementation can vary. Pinning a backend revision and model improves comparability but does not establish deterministic behavior.

For a stable task that becomes frequent, the generated implementation may eventually be worth reviewing and maintaining directly. Persisting or compiling that implementation is a possible later feature, separate from the initial interpreter.

## Development plan

The initial platform target is Linux and macOS. The process contract is Unix-oriented; Windows support needs an explicit treatment of arguments, signals, handles, and process groups.

No installation command or repository URL is published yet. The first code milestone should establish a Cargo project, a pinned Codex integration, and one runnable example.

| Milestone | Deliverable | Completion criterion |
|---|---|---|
| 1. Process host | Script loading, exact argv, stream operations, child supervision, and explicit exit. | Tests pass using real pipes and subprocesses without paid model calls. |
| 2. Embedded agent | A pinned Codex adapter and process tools. | A prose script produces exact output, keeps stdout clean, and returns its specified status. |
| 3. Cancellation | Signals, deadlines, broken pipes, and bounded shutdown. | Interrupts work during blocked reads and model waits; managed children are reaped. |
| 4. Demonstrations | Top words, a status-driven filter, CGI, and a toy emulator. | Each example has a documented contract and repeatable acceptance cases. |
| 5. Packaging | Reproducible builds, configuration guidance, and a first release. | Installation and behavior are verified on the supported platforms. |

### Testing

Test the process host independently of language-model interpretation. Use a controlled backend driver to issue tool operations, with actual OS pipes, files, and child processes exercising the boundaries.

Important cases include:

- Arguments containing spaces, empty strings, leading hyphens, wildcard characters, and non-UTF-8 bytes on Unix.

- Binary streams, NUL bytes, partial UTF-8 sequences, missing final newlines, short reads, and EOF.

- A producer that keeps stdin open, a slow consumer, and a consumer that closes its pipe early.

- Separate stdout and stderr capture, including backend startup and failure.

- Environment lookup, working-directory inheritance, and lossless argument forwarding to children.

- Exit codes, child signal results, duplicate exit requests, and backend completion without a terminal result.

- SIGINT and SIGTERM during startup, input reads, model requests, and child execution.

- Deadline expiration, repeated interruption, child cleanup, and refused operations under the execution policy.

Live end-to-end tests then exercise the prose scripts with a recorded model configuration. Assert output bytes, exit status, and relevant side effects. Repeat important cases to measure reliability rather than treating one successful model run as proof.

For the word-frequency demonstration, compare against a deterministic reference and include large files. Report latency, model usage, and input transferred to the model separately from correctness.

### Decisions to settle during the prototype

The main open choices are the exact upstream Codex revision and embedding dependencies, the public configuration and permission interface, signal-handler registration and grace periods, trace format and retention, packaging, and the project license.

Keep the script format simple while those choices are tested. The first useful result is one English script that behaves correctly as a process.

## Related work

- [Using LLM in the shebang line of a script](https://til.simonwillison.net/llms/llm-shebang), by Simon Willison, is the direct inspiration for putting English instructions behind a shebang.

- [Andi AIRun](https://github.com/andisearch/airun) is a close relative: executable Markdown, shebangs, Unix pipes, and output redirection using agent runtimes.

- [Programming Pearls: A Literate Program](https://www.cs.tufts.edu/~nr/cs257/archive/don-knuth/pearls-2.pdf), with Knuth's program and McIlroy's review, supplies the historical word-frequency demonstration.

- [Codex](https://github.com/openai/codex) supplies the preferred initial agent runtime.

`beg` explores this existing direction through a small, explicit Unix process contract and a Rust host that owns its implementation.
