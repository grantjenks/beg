#!/usr/bin/env python3
"""Open a beg trace in a self-contained local browser page."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import webbrowser


TRACE_SLOT = '<script type="application/json" id="initial-trace">null</script>'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", type=Path, help="a beg JSON Lines log")
    parser.add_argument("--no-open", action="store_true", help="write the viewer without opening a browser")
    args = parser.parse_args()

    try:
        trace = args.trace.read_text(encoding="utf-8", errors="replace")
        template = Path(__file__).with_name("trace-viewer.html").read_text(encoding="utf-8")
    except OSError as error:
        parser.exit(1, f"{parser.prog}: {error}\n")

    # Escape HTML tag openings even inside this inert script element so a
    # trace record cannot close the element or inject page markup.
    payload = json.dumps({"name": args.trace.name, "text": trace}, ensure_ascii=False)
    payload = payload.replace("<", r"\u003c")
    page = template.replace(TRACE_SLOT, TRACE_SLOT.replace(">null<", f">{payload}<"), 1)
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", prefix="beg-trace-", suffix=".html", delete=False
    ) as output:
        output.write(page)
        path = Path(output.name)
    print(path, flush=True)
    if args.no_open:
        return
    if sys.platform == "darwin":
        opened = subprocess.run(["/usr/bin/open", str(path)], check=False).returncode == 0
    else:
        opened = webbrowser.open(path.resolve().as_uri())
    if not opened:
        parser.exit(1, f"{parser.prog}: could not launch a browser. Open the page at {path}.\n")


if __name__ == "__main__":
    main()
