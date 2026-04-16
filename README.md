# `block-benchie`

Read-only direct-I/O block device transfer-rate sampler. Divides a device into
evenly spaced sample points, reads for a fixed time budget at each point, and
produces an SVG graph and Markdown report of the measured throughput.

Primarily useful for detecting NVMe drives with read degradation as data ages
and TLC/QLC cells become slower to read.

![Example output](https://i.imgur.com/MIm5BLf.png)

## Quick start

```bash
# Run directly via Nix (no install required)
nix run github:kylemanna/block-benchie -- /dev/nvme0n1
```

```bash
# Or build from source with Cargo
cargo build --release
sudo ./target/release/block-benchie /dev/nvme0n1
```

By default the benchmark divides the device into 200 evenly spaced sample
points, reads for up to 100 ms at each point, prints per-sample throughput, and
writes an SVG read-rate graph named after the best matching `/dev/disk/by-id`
entry. It also writes a Markdown report with the same basename.

Root (or read permission on the device node) is required to open a raw block device.

## How it works

1. Opens the device read-only with `O_DIRECT`, bypassing the Linux page cache.
2. Divides the device into `--bins` evenly spaced positions.
3. Reads for up to `--sample-ms` milliseconds at each position.
4. Prints per-sample throughput to stdout as each sample completes.
5. Writes an SVG bar chart and a Markdown report when all samples finish.

No writes are issued at any point.

## Options

```
Usage: block-benchie <block-device> [--bins N] [--sample-ms MS] [--output FILE]

Options:
  --bins N          Number of evenly spaced sample points  (default: 200)
  --sample-ms MS    Time budget for each sample point  (default: 100)
  -o, --output FILE SVG output path  (default: block-benchie-<device>.svg)
  -h, --help        Show this help
```

Example with explicit options:

```bash
block-benchie /dev/nvme0n1 \
  --bins 200 \
  --sample-ms 100 \
  --output nvme0n1.svg
```

## Output

**Console** — one line per sample printed to stdout as reads complete:

```
    1   0.00% offset           0 B read    200.00 MiB in   0.123s   1626.02 MiB/s
    2   0.50% offset    931.32 MiB read    196.00 MiB in   0.100s   1960.00 MiB/s
  ...
```

A summary line with min/avg/max throughput is printed to stderr at the end.

**SVG graph** — a bar chart of read rate vs. device position, with an average
line overlaid. Named after the matching `/dev/disk/by-id` entry when available
(e.g. `block-benchie-Samsung_SSD_990_PRO_2TB_S7KHNJ0W123456.svg`), otherwise
after the device node.

**Markdown report** — a `.md` file with the same basename as the SVG. Includes
a metadata table (device paths, I/O settings, sample configuration), a summary
table (min/avg/max), and a full per-sample table.

Both output files embed device metadata: generation time, input path, resolved
device path, `/dev/disk/by-id` symlink, direct-I/O settings, and sample
configuration.

## Notes

- Results can be affected by other I/O running concurrently on the same device.
- Reads near the end of small devices stop at the readable aligned device size.
- Reads are 4 MiB chunks internally; all offsets and lengths are aligned to
  4096 bytes as required by `O_DIRECT`.
- Supported platforms: `x86_64-linux`, `aarch64-linux`.
