use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::env;
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const DEFAULT_BINS: usize = 200;
const DEFAULT_SAMPLE_MS: u64 = 100;
const READ_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const ALIGNMENT: u64 = 4096;
const END_RUNWAY_BYTES: u64 = GIB;
const BLKGETSIZE64: c_ulong = 0x8008_1272;
const O_DIRECT: c_int = 0o40000;

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

#[derive(Debug)]
struct Config {
    device: PathBuf,
    bins: usize,
    sample_duration: Duration,
    output: Option<PathBuf>,
}

#[derive(Debug)]
struct DeviceMetadata {
    input_path: PathBuf,
    canonical_path: PathBuf,
    by_id_path: Option<PathBuf>,
    label: String,
}

#[derive(Debug)]
struct RunMetadata {
    generated_unix_seconds: u64,
    generated_utc: String,
}

#[derive(Debug)]
struct Sample {
    index: usize,
    offset: u64,
    bytes_read: u64,
    elapsed_secs: f64,
    mib_per_sec: f64,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = Config::parse(env::args().skip(1))?;
    let device_metadata = DeviceMetadata::for_path(&config.device);
    let output = config
        .output
        .clone()
        .unwrap_or_else(|| default_output_path(&device_metadata));
    let markdown_output = markdown_output_path(&output);
    let run_metadata = RunMetadata::new();
    let file = open_direct(&config.device)?;
    let device_size = block_device_size(&file).map_err(|err| {
        format!(
            "failed to determine size for {}: {err}",
            config.device.display()
        )
    })?;

    if device_size == 0 {
        return Err(format!("{} has zero readable bytes", config.device.display()));
    }

    if device_size < ALIGNMENT {
        return Err(format!(
            "{} has fewer than {ALIGNMENT} readable bytes; direct I/O requires aligned reads",
            config.device.display()
        ));
    }

    let offsets = sample_offsets(device_size, config.bins);
    let mut samples = Vec::with_capacity(offsets.len());

    eprintln!(
        "benchmarking {}: size {}, {} samples, up to {} per sample",
        config.device.display(),
        human_bytes(device_size),
        offsets.len(),
        human_duration(config.sample_duration)
    );
    eprintln!(
        "read-only direct I/O benchmark; no writes will be issued and the Linux page cache is bypassed"
    );

    for (index, offset) in offsets.into_iter().enumerate() {
        let sample = read_sample(&file, index, offset, device_size, config.sample_duration)?;
        println!(
            "{:>5} {:>7.2}% offset {:>12} read {:>10} in {:>7.3}s {:>9.2} MiB/s",
            sample.index + 1,
            position_percent(sample.offset, device_size),
            human_bytes(sample.offset),
            human_bytes(sample.bytes_read),
            sample.elapsed_secs,
            sample.mib_per_sec
        );
        samples.push(sample);
    }

    write_svg(
        &device_metadata,
        &run_metadata,
        &output,
        device_size,
        config.sample_duration,
        &samples,
    )?;
    write_markdown_report(
        &device_metadata,
        &run_metadata,
        &output,
        &markdown_output,
        device_size,
        config.sample_duration,
        &samples,
    )?;
    print_summary(&samples, &output, &markdown_output);

    Ok(())
}

impl Config {
    fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut device = None;
        let mut bins = DEFAULT_BINS;
        let mut sample_ms = DEFAULT_SAMPLE_MS;
        let mut output = None;

        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--bins" => {
                    bins = parse_next_usize(&mut args, "--bins")?;
                    if bins == 0 {
                        return Err("--bins must be greater than zero".to_string());
                    }
                }
                "--sample-ms" => {
                    sample_ms = parse_next_u64(&mut args, "--sample-ms")?;
                    if sample_ms == 0 {
                        return Err("--sample-ms must be greater than zero".to_string());
                    }
                }
                "-o" | "--output" => {
                    output = Some(PathBuf::from(next_arg(&mut args, arg.as_str())?));
                }
                _ if arg.starts_with('-') => {
                    return Err(format!("unknown option: {arg}"));
                }
                _ => {
                    if device.replace(PathBuf::from(&arg)).is_some() {
                        return Err(format!("unexpected extra positional argument: {arg}"));
                    }
                }
            }
        }

        let device = device.ok_or_else(|| "missing block device path".to_string())?;

        Ok(Self {
            device,
            bins,
            sample_duration: Duration::from_millis(sample_ms),
            output,
        })
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{name} requires a value"))
}

fn parse_next_usize(args: &mut impl Iterator<Item = String>, name: &str) -> Result<usize, String> {
    next_arg(args, name)?
        .parse()
        .map_err(|err| format!("invalid value for {name}: {err}"))
}

fn parse_next_u64(args: &mut impl Iterator<Item = String>, name: &str) -> Result<u64, String> {
    next_arg(args, name)?
        .parse()
        .map_err(|err| format!("invalid value for {name}: {err}"))
}

fn print_usage() {
    println!(
        "Usage: block-benchie <block-device> [--bins N] [--sample-ms MS] [--output FILE]\n\
\n\
Read-only benchmark that samples transfer rate across a block device and writes an SVG graph.\n\
\n\
Options:\n\
  --bins N          Number of evenly spaced samples (default: {DEFAULT_BINS})\n\
  --sample-ms MS    Time budget for each sample in milliseconds (default: {DEFAULT_SAMPLE_MS})\n\
  -o, --output FILE SVG output path (default: block-benchie-DEVICE.svg)\n\
  -h, --help        Show this help"
    );
}

impl DeviceMetadata {
    fn for_path(path: &Path) -> Self {
        let canonical_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let by_id_path = best_by_id_path(&canonical_path);
        let label = by_id_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(device_label_from_by_id)
            .or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| "block-device".to_string());

        Self {
            input_path: path.to_path_buf(),
            canonical_path,
            by_id_path,
            label,
        }
    }
}

impl RunMetadata {
    fn new() -> Self {
        let generated_unix_seconds = generated_unix_seconds();
        Self {
            generated_unix_seconds,
            generated_utc: format_utc_unix_seconds(generated_unix_seconds),
        }
    }
}

fn best_by_id_path(canonical_device: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir("/dev/disk/by-id").ok()?;
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let target = fs::canonicalize(&path).ok()?;
            if target == canonical_device {
                Some(path)
            } else {
                None
            }
        })
        .min_by_key(|path| by_id_rank(path))
}

fn by_id_rank(path: &Path) -> (u8, usize, String) {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let generic = name.starts_with("wwn-")
        || name.starts_with("nvme-eui.")
        || name.starts_with("dm-uuid-")
        || name.starts_with("dm-name-");
    (generic as u8, name.len(), name.to_string())
}

fn device_label_from_by_id(name: &str) -> String {
    name.strip_prefix("nvme-")
        .or_else(|| name.strip_prefix("ata-"))
        .or_else(|| name.strip_prefix("scsi-"))
        .or_else(|| name.strip_prefix("usb-"))
        .unwrap_or(name)
        .to_string()
}

fn default_output_path(metadata: &DeviceMetadata) -> PathBuf {
    PathBuf::from(format!("block-benchie-{}.svg", filename_safe(&metadata.label)))
}

fn markdown_output_path(svg_output: &Path) -> PathBuf {
    let mut output = svg_output.to_path_buf();
    output.set_extension("md");
    output
}

fn filename_safe(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }

    if output.is_empty() {
        "block-device".to_string()
    } else {
        output
    }
}

fn open_direct(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECT)
        .open(path)
        .map_err(|err| format!("failed to open {} read-only with O_DIRECT: {err}", path.display()))
}

fn block_device_size(file: &File) -> io::Result<u64> {
    if let Ok(size) = file.metadata().map(|metadata| metadata.len()) {
        if size > 0 {
            return Ok(size);
        }
    }

    let mut size = 0_u64;
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            BLKGETSIZE64,
            &mut size as *mut u64 as *mut c_void,
        )
    };

    if rc == 0 {
        Ok(size)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn sample_offsets(device_size: u64, bins: usize) -> Vec<u64> {
    let end_runway = if device_size > END_RUNWAY_BYTES + ALIGNMENT {
        END_RUNWAY_BYTES
    } else {
        ALIGNMENT
    };
    let max_offset = align_down(device_size.saturating_sub(end_runway), ALIGNMENT);
    if bins == 1 || max_offset == 0 {
        return vec![0];
    }

    (0..bins)
        .map(|index| {
            let raw = (max_offset as u128 * index as u128) / (bins - 1) as u128;
            align_down(raw as u64, ALIGNMENT)
        })
        .collect()
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value / alignment * alignment
}

fn read_sample(
    file: &File,
    index: usize,
    offset: u64,
    device_size: u64,
    sample_duration: Duration,
) -> Result<Sample, String> {
    let readable_bytes = align_down(device_size.saturating_sub(offset), ALIGNMENT);
    let buffer_len = readable_bytes.min(READ_CHUNK_BYTES as u64) as usize;
    let mut buffer = AlignedBuffer::new(buffer_len, ALIGNMENT as usize)?;
    let start = Instant::now();
    let mut bytes_read = 0_u64;

    while start.elapsed() < sample_duration && bytes_read < readable_bytes {
        let remaining = (readable_bytes - bytes_read) as usize;
        let len = remaining.min(buffer.len());
        debug_assert_eq!(len % ALIGNMENT as usize, 0);
        let read = file
            .read_at(buffer.as_mut_slice(len), offset + bytes_read)
            .map_err(|err| format!("read failed at offset {}: {err}", offset + bytes_read))?;

        if read == 0 {
            break;
        }
        if read % ALIGNMENT as usize != 0 && bytes_read + read as u64 != readable_bytes {
            return Err(format!(
                "unaligned short read at offset {}: read {read} bytes",
                offset + bytes_read
            ));
        }

        bytes_read += read as u64;
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    let mib_per_sec = if elapsed_secs > 0.0 {
        bytes_read as f64 / MIB as f64 / elapsed_secs
    } else {
        0.0
    };

    Ok(Sample {
        index,
        offset,
        bytes_read,
        elapsed_secs,
        mib_per_sec,
    })
}

struct AlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(len: usize, alignment: usize) -> Result<Self, String> {
        let layout = Layout::from_size_align(len, alignment)
            .map_err(|err| format!("invalid direct I/O buffer layout: {err}"))?;
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or_else(|| {
            format!(
                "failed to allocate {} direct I/O buffer aligned to {alignment} bytes",
                human_bytes(len as u64)
            )
        })?;

        Ok(Self { ptr, len, layout })
    }

    fn len(&self) -> usize {
        self.len
    }

    fn as_mut_slice(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.len);
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

fn write_svg(
    metadata: &DeviceMetadata,
    run_metadata: &RunMetadata,
    output: &Path,
    device_size: u64,
    sample_duration: Duration,
    samples: &[Sample],
) -> Result<(), String> {
    let width = 1200.0;
    let height = 520.0;
    let margin_left = 78.0;
    let margin_right = 28.0;
    let margin_top = 74.0;
    let margin_bottom = 72.0;
    let plot_width = width - margin_left - margin_right;
    let plot_height = height - margin_top - margin_bottom;
    let max_rate = max_rate(samples).max(1.0);
    let avg_rate = average_rate(samples);
    let bar_width = (plot_width / samples.len().max(1) as f64).max(1.0);

    let mut svg = String::new();
    svg.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    svg.push('\n');
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0} {height:.0}" role="img" aria-labelledby="title desc">"#
    ));
    svg.push('\n');
    write_svg_metadata(
        &mut svg,
        metadata,
        run_metadata,
        output,
        device_size,
        sample_duration,
        samples,
    );
    svg.push_str(&format!(
        "<title id=\"title\">Read benchmark for {}</title>\n",
        escape_xml(&metadata.label)
    ));
    svg.push_str(&format!(
        "<desc id=\"desc\">Read-only transfer-rate samples across {}, generated at {}, maximum {:.2} MiB/s, average {:.2} MiB/s.</desc>\n",
        escape_xml(&human_bytes(device_size)),
        escape_xml(&run_metadata.generated_utc),
        max_rate,
        avg_rate
    ));
    svg.push_str("<rect width=\"1200\" height=\"520\" fill=\"#f8fafc\"/>\n");
    svg.push_str(&format!(
        "<text x=\"{margin_left}\" y=\"30\" font-family=\"sans-serif\" font-size=\"20\" fill=\"#111827\">Read rate across {}</text>\n",
        escape_xml(&metadata.label)
    ));
    svg.push_str(&format!(
        "<text x=\"{margin_left}\" y=\"50\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#475569\">{} samples, up to {} per sample, device size {}</text>\n",
        samples.len(),
        escape_xml(&human_duration(sample_duration)),
        escape_xml(&human_bytes(device_size))
    ));
    svg.push_str(&format!(
        "<text x=\"{margin_left}\" y=\"66\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#475569\">Generated {}</text>\n",
        escape_xml(&run_metadata.generated_utc)
    ));

    for tick in 0..=5 {
        let ratio = tick as f64 / 5.0;
        let y = margin_top + plot_height - ratio * plot_height;
        let rate = ratio * max_rate;
        svg.push_str(&format!(
            "<line x1=\"{margin_left:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\" stroke=\"#e2e8f0\"/>\n",
            width - margin_right
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#475569\">{rate:.0}</text>\n",
            margin_left - 8.0,
            y + 4.0
        ));
    }

    for tick in 0..=4 {
        let ratio = tick as f64 / 4.0;
        let x = margin_left + ratio * plot_width;
        svg.push_str(&format!(
            "<line x1=\"{x:.1}\" y1=\"{margin_top:.1}\" x2=\"{x:.1}\" y2=\"{:.1}\" stroke=\"#e2e8f0\"/>\n",
            margin_top + plot_height
        ));
        svg.push_str(&format!(
            "<text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#475569\">{:.0}%</text>\n",
            margin_top + plot_height + 22.0,
            ratio * 100.0
        ));
    }

    svg.push_str(&format!(
        "<line x1=\"{margin_left:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#334155\"/>\n",
        margin_top + plot_height,
        width - margin_right,
        margin_top + plot_height
    ));
    svg.push_str(&format!(
        "<line x1=\"{margin_left:.1}\" y1=\"{margin_top:.1}\" x2=\"{margin_left:.1}\" y2=\"{:.1}\" stroke=\"#334155\"/>\n",
        margin_top + plot_height
    ));

    for (i, sample) in samples.iter().enumerate() {
        let rate_ratio = (sample.mib_per_sec / max_rate).clamp(0.0, 1.0);
        let bar_height = rate_ratio * plot_height;
        let x = margin_left + i as f64 * bar_width;
        let y = margin_top + plot_height - bar_height;
        svg.push_str(&format!(
            "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{:.2}\" height=\"{bar_height:.2}\" fill=\"#2563eb\" opacity=\"0.85\"/>\n",
            (bar_width - 0.25).max(0.75)
        ));
    }

    let avg_y = margin_top + plot_height - (avg_rate / max_rate).clamp(0.0, 1.0) * plot_height;
    svg.push_str(&format!(
        "<line x1=\"{margin_left:.1}\" y1=\"{avg_y:.1}\" x2=\"{:.1}\" y2=\"{avg_y:.1}\" stroke=\"#dc2626\" stroke-width=\"2\" stroke-dasharray=\"6 5\"/>\n",
        width - margin_right
    ));
    svg.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#dc2626\">avg {:.2} MiB/s</text>\n",
        width - margin_right,
        avg_y - 7.0,
        avg_rate
    ));

    svg.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"13\" fill=\"#334155\">Position across device</text>\n",
        margin_left + plot_width / 2.0,
        height - 22.0
    ));
    svg.push_str(&format!(
        "<text x=\"22\" y=\"{:.1}\" transform=\"rotate(-90 22 {:.1})\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"13\" fill=\"#334155\">MiB/s</text>\n",
        margin_top + plot_height / 2.0,
        margin_top + plot_height / 2.0
    ));
    svg.push_str("</svg>\n");

    fs::write(output, svg).map_err(|err| format!("failed to write {}: {err}", output.display()))
}

fn write_svg_metadata(
    svg: &mut String,
    metadata: &DeviceMetadata,
    run_metadata: &RunMetadata,
    output: &Path,
    device_size: u64,
    sample_duration: Duration,
    samples: &[Sample],
) {
    svg.push_str("<metadata>\n");
    svg.push_str(&format!(
        "  <block-benchie device-label=\"{}\" input-path=\"{}\" canonical-path=\"{}\"",
        escape_xml_attr(&metadata.label),
        escape_xml_attr(&metadata.input_path.display().to_string()),
        escape_xml_attr(&metadata.canonical_path.display().to_string())
    ));
    if let Some(by_id_path) = &metadata.by_id_path {
        svg.push_str(&format!(
            " by-id-path=\"{}\"",
            escape_xml_attr(&by_id_path.display().to_string())
        ));
    }
    svg.push_str(&format!(
        " output-path=\"{}\" io=\"direct\" open-flag=\"O_DIRECT\" device-size-bytes=\"{}\" sample-duration-ms=\"{}\" samples=\"{}\" end-runway-bytes=\"{}\" read-chunk-bytes=\"{}\" alignment-bytes=\"{}\" generated-unix-seconds=\"{}\" generated-utc=\"{}\" />\n",
        escape_xml_attr(&output.display().to_string()),
        device_size,
        sample_duration.as_millis(),
        samples.len(),
        END_RUNWAY_BYTES,
        READ_CHUNK_BYTES,
        ALIGNMENT,
        run_metadata.generated_unix_seconds,
        escape_xml_attr(&run_metadata.generated_utc)
    ));
    svg.push_str("</metadata>\n");
}

fn write_markdown_report(
    metadata: &DeviceMetadata,
    run_metadata: &RunMetadata,
    svg_output: &Path,
    markdown_output: &Path,
    device_size: u64,
    sample_duration: Duration,
    samples: &[Sample],
) -> Result<(), String> {
    let mut markdown = String::new();
    markdown.push_str(&format!(
        "# Block Benchie: {}\n\n",
        markdown_cell(&metadata.label)
    ));
    markdown.push_str("## Metadata\n\n");
    markdown.push_str("| Field | Value |\n");
    markdown.push_str("| --- | --- |\n");
    push_markdown_row(
        &mut markdown,
        "Generated Unix seconds",
        &run_metadata.generated_unix_seconds.to_string(),
    );
    push_markdown_row(&mut markdown, "Generated UTC", &run_metadata.generated_utc);
    push_markdown_row(&mut markdown, "Input path", &metadata.input_path.display().to_string());
    push_markdown_row(
        &mut markdown,
        "Canonical path",
        &metadata.canonical_path.display().to_string(),
    );
    push_markdown_row(
        &mut markdown,
        "By-id path",
        metadata
            .by_id_path
            .as_deref()
            .map(Path::display)
            .map(|display| display.to_string())
            .as_deref()
            .unwrap_or(""),
    );
    push_markdown_row(&mut markdown, "SVG output", &svg_output.display().to_string());
    push_markdown_row(
        &mut markdown,
        "Markdown output",
        &markdown_output.display().to_string(),
    );
    push_markdown_row(&mut markdown, "I/O mode", "direct");
    push_markdown_row(&mut markdown, "Open flag", "O_DIRECT");
    push_markdown_row(
        &mut markdown,
        "Device size",
        &format!("{} ({device_size} bytes)", human_bytes(device_size)),
    );
    push_markdown_row(
        &mut markdown,
        "Sample duration",
        &human_duration(sample_duration),
    );
    push_markdown_row(&mut markdown, "Samples", &samples.len().to_string());
    push_markdown_row(
        &mut markdown,
        "End runway",
        &format!(
            "{} ({} bytes)",
            human_bytes(END_RUNWAY_BYTES),
            END_RUNWAY_BYTES
        ),
    );
    push_markdown_row(
        &mut markdown,
        "Read chunk size",
        &format!(
            "{} ({} bytes)",
            human_bytes(READ_CHUNK_BYTES as u64),
            READ_CHUNK_BYTES
        ),
    );
    push_markdown_row(
        &mut markdown,
        "Alignment",
        &format!("{} bytes", ALIGNMENT),
    );

    markdown.push_str("\n## Summary\n\n");
    markdown.push_str("| Metric | Value |\n");
    markdown.push_str("| --- | --- |\n");
    push_markdown_row(&mut markdown, "Minimum", &format!("{:.2} MiB/s", min_rate(samples)));
    push_markdown_row(
        &mut markdown,
        "Average",
        &format!("{:.2} MiB/s", average_rate(samples)),
    );
    push_markdown_row(&mut markdown, "Maximum", &format!("{:.2} MiB/s", max_rate(samples)));

    markdown.push_str("\n## Samples\n\n");
    markdown.push_str(
        "| Sample | Position | Offset | Bytes read | Elapsed seconds | MiB/s |\n",
    );
    markdown.push_str("| ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for sample in samples {
        markdown.push_str(&format!(
            "| {} | {:.2}% | {} | {} | {:.3} | {:.2} |\n",
            sample.index + 1,
            position_percent(sample.offset, device_size),
            markdown_cell(&human_bytes(sample.offset)),
            markdown_cell(&human_bytes(sample.bytes_read)),
            sample.elapsed_secs,
            sample.mib_per_sec
        ));
    }

    fs::write(markdown_output, markdown)
        .map_err(|err| format!("failed to write {}: {err}", markdown_output.display()))
}

fn push_markdown_row(markdown: &mut String, field: &str, value: &str) {
    markdown.push_str(&format!(
        "| {} | {} |\n",
        markdown_cell(field),
        markdown_cell(value)
    ));
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\n', " ")
        .replace('\r', " ")
}

fn print_summary(samples: &[Sample], svg_output: &Path, markdown_output: &Path) {
    let min = min_rate(samples);
    let max = max_rate(samples);
    let avg = average_rate(samples);

    eprintln!(
        "summary: min {:.2} MiB/s, avg {:.2} MiB/s, max {:.2} MiB/s",
        min, avg, max
    );
    eprintln!("wrote {}", svg_output.display());
    eprintln!("wrote {}", markdown_output.display());
}

fn average_rate(samples: &[Sample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }

    samples
        .iter()
        .map(|sample| sample.mib_per_sec)
        .sum::<f64>()
        / samples.len() as f64
}

fn min_rate(samples: &[Sample]) -> f64 {
    samples
        .iter()
        .map(|sample| sample.mib_per_sec)
        .fold(f64::INFINITY, f64::min)
}

fn max_rate(samples: &[Sample]) -> f64 {
    samples
        .iter()
        .map(|sample| sample.mib_per_sec)
        .fold(0.0_f64, f64::max)
}

fn position_percent(offset: u64, device_size: u64) -> f64 {
    if device_size == 0 {
        0.0
    } else {
        offset as f64 / device_size as f64 * 100.0
    }
}

fn human_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = units[0];

    for next_unit in units.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next_unit;
    }

    if unit == "B" {
        format!("{bytes} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}

fn human_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        format!("{millis} ms")
    } else {
        format!("{:.3} s", duration.as_secs_f64())
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn escape_xml_attr(value: &str) -> String {
    escape_xml(value)
}

fn generated_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn format_utc_unix_seconds(seconds: u64) -> String {
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, u32, u32) {
    let days = days_since_unix_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };

    if month <= 2 {
        year += 1;
    }

    (year, month as u32, day as u32)
}
