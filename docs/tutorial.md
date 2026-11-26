# beg Tutorial

## Installation

`beg` builds on Linux and macOS with Rust 1.99.1 or newer and a C/C++
toolchain. From a checkout, install it with Cargo.

```sh
cargo install --path . --locked
beg --version
```

Cargo installs `beg` in its bin directory, usually `$HOME/.cargo/bin`.
Make sure that directory is on `PATH`. To use a build from the checkout,
add the build directory to `PATH` instead.

```sh
cargo build --locked
export PATH="$PWD/target/debug:$PATH"
```

On Linux, install Bubblewrap for sandboxed Codex commands. `beg` embeds
Codex `0.171.0-alpha.13` at commit `581161ab`. Download the
`codex-code-mode-host-<target>.tar.gz` archive for your OS and architecture
from that release. Put the extracted `codex-code-mode-host` beside the
`beg` executable. Prepared filters use this helper, as do models that
require Codex Code Mode. [Codex release assets][release-assets]

## Sign in

If you already use Codex, `beg` can use the same login and configuration.
To use a separate Codex home, create it and sign in there with a matching
version of the Codex CLI.

```sh
export BEG_CODEX_HOME="$HOME/.beg-codex"
mkdir -p "$BEG_CODEX_HOME"
CODEX_HOME="$BEG_CODEX_HOME" codex login
```

You can also supply a `CODEX_API_KEY` or `OPENAI_API_KEY` in the
environment. Sign in before running a script. `beg` runs Codex
noninteractively, so stdin is available for the script. The
[Command Line Reference][cli] lists the configuration variables.

## Your first script

Save the following as `topwords`. The shebang tells the shell which
interpreter to use. After that, write the rules that determine the output.

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

Exit 0 after successful completion, or 1 if the input cannot be processed.
```

Make the file executable and pipe in some text.

```console
$ chmod +x topwords
$ printf 'Pear pear apple. APPLE pear!\n' | BEG_EXEC=inherit ./topwords 2
3	pear
2	apple
$ echo "$?"
0
```

Try a limit of `0` and check `$?` again. The script should print a usage
message on stderr and exit `2`.

`BEG_EXEC=inherit` lets a script connect a child directly to its streams.
Use it for scripts you trust. A child started this way has your filesystem
and network permissions. Native Codex commands use the configured Codex
sandbox. [Process Contract][process]

## Run it again

You can pass the script path to `beg` explicitly. The terminal controls
redirection the same way in either form.

```sh
BEG_EXEC=inherit beg --timeout 60s topwords 10 < book.txt > counts.tsv
```

On the first run, Codex can prepare a word counter. With a prepared program
available, subsequent runs can count new input locally. An identical run can
reuse the earlier output and exit status. Use a fresh trace to see which
path a run took. [Caching][caching] · [Tracing][tracing]

Try [Examples][examples] for scripts that work with files, a request
environment, and long-running commands.

[release-assets]: https://github.com/openai/codex/releases/expanded_assets/rust-v0.171.0-alpha.13
[cli]: cli.md
[process]: process.md
[caching]: caching.md
[tracing]: tracing.md
[examples]: examples.md
