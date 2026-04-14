# `block-benchie`

Read-only direct-I/O block device transfer-rate sampler.

```bash
nix run . -- /dev/nvme0n1
```

By default the benchmark divides the device into 200 evenly spaced sample
points, reads 200 MiB at each point, prints per-sample throughput, and writes an
SVG read-rate graph named after the best matching `/dev/disk/by-id` entry. It
also writes a Markdown report with the same basename.

Useful options:

```bash
block-benchie /dev/nvme0n1 \
  --bins 200 \
  --sample-mib 200 \
  --output nvme0n1.svg
```

The program opens the device read-only with `O_DIRECT`. It does not issue writes
or modify the block device, and reads bypass the Linux page cache. Results can
still be affected by device state and by other I/O running at the same time. The
SVG and Markdown report embed device metadata, including the generation time,
input path, resolved device path, matching `/dev/disk/by-id` path when
available, direct-I/O settings, and sample configuration.
