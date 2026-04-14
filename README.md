# `block-benchie`

Read-only block device transfer-rate sampler.

```bash
nix run . -- /dev/nvme0n1 --output nvme0n1.svg
```

By default the benchmark divides the device into 1000 evenly spaced sample
points, reads 100 MiB at each point, prints per-sample throughput, and writes an
SVG read-rate graph.

Useful options:

```bash
block-benchie /dev/nvme0n1 \
  --bins 1000 \
  --sample-mib 100 \
  --output nvme0n1.svg
```

The program only opens the device read-only. It does not issue writes or modify
the block device. Results can still be affected by the Linux page cache and by
other I/O running at the same time.
