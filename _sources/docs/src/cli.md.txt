# Command-line reference

```text
fsspec-fuse URL MOUNTPOINT [-o KEY=VALUE]... [--threads N]
```

Mounts `URL` read-only at the existing directory `MOUNTPOINT` and keeps running until
interrupted.

| Argument                   | Default  | Meaning                                                                    |
| -------------------------- | -------- | -------------------------------------------------------------------------- |
| `URL`                      | required | fsspec URL to mount, such as `s3://bucket/prefix` or `file:///data`.       |
| `MOUNTPOINT`               | required | Existing directory to mount on.                                            |
| `-o KEY=VALUE`, `--option` | none     | Storage option passed to the backend as a string. Repeat for more options. |
| `--threads N`              | `4`      | Number of requests served at once.                                         |
| `-h`, `--help`             | off      | Show help and exit.                                                        |

An option without `=` or with an empty key is rejected before mounting.

Ctrl-C (`SIGINT`) and `SIGTERM` unmount the filesystem and exit with status 0. The command
also exits when the filesystem is unmounted from outside, for example with `fusermount3 -u`.
Argument errors exit with status 2.
