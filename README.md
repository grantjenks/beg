# beg

`beg` is a Unix interpreter for scripts written in English.

How would you find the ten most common words in a file? Jon Bentley put
a version of that question to Donald Knuth for *Programming Pearls* in
June 1986. Knuth began by defining words and deciding how to break ties.
He wrote the solution in WEB, with Pascal code and an explanation of
the hash trie that kept the counts. [Knuth's program][pearls]

Doug McIlroy reviewed the program and did the job with six Unix commands
that already existed. [McIlroy's review][mcilroy]

```sh
tr -cs A-Za-z '\n' |
tr A-Z a-z |
sort |
uniq -c |
sort -rn |
sed ${1}q
```

The first `sort` puts equal words together for `uniq`. The second puts
the counts in order. What if you could write down the rules Knuth started
with and run them directly? [topwords] does that.

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

```console
$ printf 'Pear pear apple. APPLE pear!\n' | BEG_EXEC=inherit ./examples/topwords 2
3	pear
2	apple
```

Codex can put together a pipeline like McIlroy's or prepare a counter
that `beg` can reuse on the next file. You can pipe in a book, redirect
the counts, and check `$?` from the shell.

## Features

- English scripts with a `#!/usr/bin/env beg` shebang.
- Unix arguments, byte streams, exit statuses, and signals.
- Embedded Codex with its native tools and access to the program's streams.
- Prepared filters that run locally and replay results for matching inputs.
- JSON Lines traces with a local browser viewer.

## Quickstart

`beg` is a prototype for Linux and macOS. Build and install from a checkout
with Rust 1.99.1 or newer and a C/C++ toolchain. See the [Tutorial] for the
matching Codex helper and sign-in.

```sh
cargo install --path . --locked
beg --version
```

Try the command above with a different limit or pipe in a file. The
[Examples] guide has more scripts to try after `topwords`.

## [Documentation]

### User Guide

- [Tutorial] covers installation, sign-in, and writing your first script.
- [Examples] has sample inputs for filters, file operations, and shell tasks.
- [Caching] explains when `beg` reuses a program or replays its output.
- [Tracing] shows how to see what a script did.

### Reference

- [Command Line Reference] lists options and environment variables.
- [Process Contract] describes streams, child processes, exit statuses, and signals.

### Community Guide

- [Implementation Details] describes the embedded runtime and its process tools.
- [Development and Support] covers testing and reporting issues.

[pearls]: https://homepages.cwi.nl/~storm/teaching/reader/BentleyEtAl86.pdf
[mcilroy]: https://homepages.cwi.nl/~storm/teaching/reader/BentleyEtAl86.pdf#page=10
[topwords]: examples/topwords
[Documentation]: docs/README.md
[Tutorial]: docs/tutorial.md
[Examples]: docs/examples.md
[Caching]: docs/caching.md
[Tracing]: docs/tracing.md
[Command Line Reference]: docs/cli.md
[Process Contract]: docs/process.md
[Implementation Details]: docs/implementation.md
[Development and Support]: docs/development.md
