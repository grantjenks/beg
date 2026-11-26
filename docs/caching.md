# Caching

Once Codex has written a word counter for `topwords`, why write it again?
`beg` can reuse the counter on new text and replay the previous result
when the inputs match.

## Preparing a filter

For a script that can compute its result from arguments, stdin, and the
environment, `beg` asks Codex for a deterministic JavaScript program.
This preparation uses the English description. It happens before the
program reads any input. A filter can read through EOF (`eof`) or run
without reading stdin (`unused`). `topwords` is an example of the former.
`cgi` is an example of the latter.

A compatible embedded program or a program in the per-user cache can
skip preparation. The matching `codex-code-mode-host` executes the
prepared program locally. If preparation is declined, `beg` records that
decision. On the next run, a `cached_decline` in the trace means `beg`
skipped preparation and went straight to a live Codex run. Scripts that
use files, network requests, interactive I/O, or signal handlers usually
take this path. A missing code-mode host also sends ordinary runs through
the live interpreter. [Installing the code-mode host][install]

## Reusing programs and results

A prepared program can run with different arguments, stdin, or
environment values. An exact match can save more work. `beg` can replay
the saved stdout and stderr bytes and return the saved exit status.

For replay, the program and every argument must match, including the
script path. A filter that reads through EOF also needs matching input
bytes. `beg` checks arguments before it buffers stdin for the lookup.
Inputs over 1 MiB run through the prepared filter with the buffered
prefix followed by the rest of the stream. A completed run can save a
result when stdout and stderr total at most 1 MiB.

For example, run the same input twice with a fresh cache and a separate
trace for each run.

```sh
cache_dir="$(mktemp -d)"
for run in first second; do
  printf 'Pear pear apple. APPLE pear!\n' |
    XDG_CACHE_HOME="$cache_dir" BEG_EXEC=inherit BEG_TRACE="$cache_dir/$run.jsonl" \
    ./examples/topwords 2
done
```

Look for `prepared_program` and `ran_program` in the first trace. A
replay shows `cached_program` and `replayed_result` in the second.
Change the limit or the input and the cached counter can run again.
[Tracing][tracing]

## Environment values

The prepared JavaScript defines `checkArgs(argv, io)` and
`run(argv, io)`. Both functions can look up any variable in the caller's
environment with `await io.env(name)`. A named lookup returns
`{ text, base64 }` or `null` for an unset variable. `await io.env()`
enumerates the environment. A replay checks the values the filter
looked up. If it enumerated the environment, all of those values and
names must match.

## Per-user storage

The cache is `beg/v1` under `XDG_CACHE_HOME`, when set to an absolute
path, or the platform's cache directory. It has 256 slots for preparation
decisions and another 256 for results. A slot can be replaced by a
different entry. Cache files belong to the user running `beg` and can
contain program output.

Ordinary runs use the per-user cache, including for scripts in read-only
checkouts.

## Embedding a prepared program

`--embed` stores a prepared program with the English source. Try it on a
copy of `topwords`.

```sh
script_dir="$(mktemp -d)"
cp -p ./examples/topwords "$script_dir/topwords"
beg --embed "$script_dir/topwords"
printf 'Pear pear apple. APPLE pear!\n' | "$script_dir/topwords" 2
```

`beg --embed` prepares or reuses a program and validates it before
replacing the file. It takes just the script path, preserves executable
permissions, and exits without reading stdin. The script must be an
owned, writable file with one link. A compatible `beg` and
`codex-code-mode-host` can run the resulting file offline.

The first empty line followed by a line containing exactly `---` starts
the storage block. Use that pair only to start storage. The rest is a
single YAML mapping.

| Field | Contents |
| --- | --- |
| `format_version` | Storage format version. |
| `source.sha256` | Digest of the English description. |
| `program.runner` | JavaScript runner identifier. |
| `program.preparation_version` | Version of the preparation interface. |
| `program.stdin` | `eof` or `unused`. |
| `program.code` | Prepared JavaScript source. |
| `build.model` | Model identifier, when recorded. |

Editing the description invalidates the embedded program. Replayed
results stay in the per-user cache.

[install]: tutorial.md#installation
[tracing]: tracing.md
