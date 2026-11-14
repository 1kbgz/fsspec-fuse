from __future__ import annotations

import argparse
import signal
import sys
import time

from .mount import mount


def _options(pairs: list[str]) -> dict[str, str]:
    options = {}
    for pair in pairs:
        key, sep, value = pair.partition("=")
        if not sep or not key:
            raise argparse.ArgumentTypeError(f"storage option must be KEY=VALUE, got {pair!r}")
        options[key] = value
    return options


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="fsspec-fuse", description="Mount an fsspec URL with FUSE, read-only unless --writable is given.")
    parser.add_argument("url", help="fsspec URL to mount, such as s3://bucket/prefix")
    parser.add_argument("mountpoint", help="existing directory to mount on")
    parser.add_argument("-o", "--option", action="append", default=[], metavar="KEY=VALUE", help="storage option; repeat for more")
    parser.add_argument("--threads", type=int, default=4, help="requests served at once (default: 4)")
    parser.add_argument("--writable", action="store_true", help="allow creating, changing, and removing files")
    args = parser.parse_args(argv)
    try:
        options = _options(args.option)
    except argparse.ArgumentTypeError as error:
        parser.error(str(error))

    # SIGTERM unwinds like Ctrl-C so the mount is released on the way out.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    with mount(args.url, args.mountpoint, threads=args.threads, read_only=not args.writable, **options) as mounted:
        print(f"Mounted {args.url} at {args.mountpoint}; press Ctrl-C to unmount.", file=sys.stderr)
        try:
            while mounted.mounted:
                time.sleep(0.5)
        except KeyboardInterrupt:
            pass
    return 0
