# beg Examples

Run these commands from the repository root with `beg` on `PATH`. The
examples that use `BEG_EXEC=inherit` can connect subprocesses directly to
their streams. [Installation][installation] · [Child processes][children]

## Filters

### Count words

How many times does each word appear? `topwords` counts words without
regard to case and breaks ties alphabetically.

```console
$ printf 'Pear pear apple. APPLE pear!\n' | BEG_EXEC=inherit ./examples/topwords 2
3	pear
2	apple
```

Try empty input or a limit of `0`. Empty input produces no output. An
invalid limit gives a usage message on stderr and exits `2`. The full
rules are in the script. [topwords][topwords]

### Find matching lines

`findlines` copies lines containing a literal, case-sensitive string.
The final matching line below has no newline. The next `printf` makes
the end of that line visible.

```sh
(
  printf 'other\ntwo words here' | BEG_EXEC=inherit ./examples/findlines 'two words'
  printf ' [exit %s]\n' "$?"
)
```

The two commands print `two words here [exit 0]` on one line. A search
with no matches exits `1`. Try an empty input, a substring containing a
space, or a pipe that stays open. [findlines][findlines]

## Everyday shell tasks

Start with a scratch directory for the sample inputs.

```sh
sample="$(mktemp -d)"
```

### [Refresh a JSON file from Wikipedia](../examples/refresh-parks)

Build a catalog of U.S. national parks and show what changed since the previous refresh.

```sh
BEG_EXEC=inherit ./examples/refresh-parks "$sample/parks.json"
BEG_EXEC=inherit ./examples/refresh-parks "$sample/parks.json"
```

### [Publish a Markdown tree under new names](../examples/publish-notes)

The copy turns `Field Notes` into `field-notes` and keeps the link to the renamed page working.

```sh
mkdir -p "$sample/notes/Field Notes"
printf '[Acadia](Field%%20Notes/Trail_Guide.md#route)\n' > "$sample/notes/Index.md"
printf '# Route\n' > "$sample/notes/Field Notes/Trail_Guide.md"
BEG_EXEC=inherit ./examples/publish-notes "$sample/notes" "$sample/site"
cat "$sample/site/index.md"
```

### [Merge two edited CSVs](../examples/merge-csv)

Alice edits the title and Bob edits the note. The merged row keeps both changes.

```sh
printf 'id,title,note\n1,Walk,Maine\n' > "$sample/original.csv"
printf 'id,title,note\n1,Hike,Maine\n' > "$sample/alice.csv"
printf 'id,title,note\n1,Walk,Acadia\n' > "$sample/bob.csv"
BEG_EXEC=inherit ./examples/merge-csv "$sample/original.csv" "$sample/alice.csv" "$sample/bob.csv" id
```

### [Follow a request across rotated logs](../examples/request-trace)

Recover the request's start from the compressed log and its full error from the current log.

```sh
mkdir -p "$sample/logs"
printf '2026-01-03T10:00:00Z request_id=req-123 started\n' |
  gzip > "$sample/logs/app.log.1.gz"
printf '2026-01-03T10:01:00Z request_id=req-123 failed\nError: missing file\n    at load()\n' > "$sample/logs/app.log"
BEG_EXEC=inherit ./examples/request-trace req-123 "$sample/logs"
```

### [Check backups for missing content](../examples/backup-gaps)

The renamed copy covers `trail.jpg`. Both copies of the missing photo appear in one group, and the script exits 1.

```sh
mkdir -p "$sample/photos" "$sample/backup-a" "$sample/backup-b"
printf 'trail' > "$sample/photos/trail.jpg"
cp "$sample/photos/trail.jpg" "$sample/backup-a/renamed.jpg"
printf 'camp' > "$sample/photos/camp.jpg"
cp "$sample/photos/camp.jpg" "$sample/photos/camp-copy.jpg"
BEG_EXEC=inherit ./examples/backup-gaps "$sample/photos" "$sample/backup-a" "$sample/backup-b"
```

### [Reconcile payouts and invoices](../examples/reconcile)

The payout names no invoice. Both invoices match its amount and date window, so the report leaves the match ambiguous.

```sh
printf 'payout_id,invoice_id,date,amount\np1,,2026-01-03,25.00\n' > "$sample/payouts.csv"
printf 'invoice_id,date,amount\ni1,2026-01-01,25.00\ni2,2026-01-02,25.00\n' > "$sample/invoices.csv"
BEG_EXEC=inherit ./examples/reconcile "$sample/payouts.csv" "$sample/invoices.csv"
```

### [Summarize test failures and show how to rerun them](../examples/test-failures)

Wrap a Cargo run or a Rust test binary. The test output streams as usual. Each failure gets a report and a command to rerun that test.

```sh
BEG_EXEC=inherit ./examples/test-failures cargo test --workspace
```

## CGI

A CGI request arrives as environment variables and an input stream. The
response goes to stdout. Set the request variables yourself to try
`cgi` without a web server.

```sh
REQUEST_METHOD=GET QUERY_STRING='name=Ada+Lovelace' ./examples/cgi
```

The response has CRLF-delimited headers and a body of
`Hello, Ada Lovelace` followed by a newline. Change `GET` to `POST` to
exercise the `405` response. Either response exits `0` when it is
written successfully. The HTTP status belongs to the response and the
exit status belongs to the process. [cgi][cgi]

## A toy virtual machine

`toyvm` reads commands one line at a time. It keeps a simulated
filesystem and process table in memory.

```console
$ printf 'write note hello\ncat note\nstart worker\nps\nexit 7\n' | ./examples/toyvm
OK
hello
1
1	worker
$ echo "$?"
7
```

Try reading a missing file and then starting a process. The script
reports the missing file on stderr and keeps reading commands. The
operating system's sandbox and permissions still control the real
host. [toyvm][toyvm]

[installation]: tutorial.md#installation
[children]: process.md#environment-and-child-processes
[topwords]: ../examples/topwords
[findlines]: ../examples/findlines
[cgi]: ../examples/cgi
[toyvm]: ../examples/toyvm
