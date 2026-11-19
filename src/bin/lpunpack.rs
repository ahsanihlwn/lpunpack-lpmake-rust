//! lpunpack - command-line tool for inspecting and extracting partition images from super.

use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process;

use lp_tools::lp::{
    format_metadata_json, format_metadata_text, read_metadata_auto, LpError, LpMetadata,
    LpMetadataExtent, LpMetadataPartition, LP_SECTOR_SIZE, LP_TARGET_TYPE_LINEAR,
    LP_TARGET_TYPE_ZERO,
};
use lp_tools::sparse::ImageSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InfoFormat {
    Text,
    Json,
}

struct CliArgs {
    partitions: Vec<String>,
    slot: u32,
    show_info: bool,
    format: InfoFormat,
    super_image: PathBuf,
    output_dir: Option<PathBuf>,
}

fn print_usage(prog: &str) {
    eprintln!(
        "{prog} - command-line tool for extracting partition images from super\n\
         \n\
         Usage:\n\
         \x20 {prog} [options...] SUPER_IMAGE [OUTPUT_DIR]\n\
         \n\
         Options:\n\
         \x20 -h, --help               Show this help message and exit.\n\
         \x20 -p, --partition=NAME     Extract the named partition. Can be specified\n\
         \x20                          multiple times or delimited by ',' or ':'.\n\
         \x20 -S, --slot=NUM           Slot number (default is 0).\n\
         \x20     --info, --no-info    Display partition metadata info (default: false).\n\
         \x20 -f, --format=FORMAT      Info output format: text or json (default: text)."
    );
}

fn parse_slot(val: &str) -> Result<u32, String> {
    if let Ok(n) = val.parse::<u32>() {
        return Ok(n);
    }
    match val {
        "a" | "_a" => Ok(0),
        "b" | "_b" => Ok(1),
        _ => Err(format!("Slot must be a valid unsigned number: '{val}'")),
    }
}

fn split_partition_names(val: &str, out: &mut Vec<String>) {
    for part in val.split(|c| c == ',' || c == ':') {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
}

fn parse_args() -> Result<Option<CliArgs>, (String, i32)> {
    let raw_args: Vec<String> = env::args().collect();
    let prog = raw_args
        .first()
        .map(|s| s.as_str())
        .unwrap_or("lpunpack");

    if raw_args.len() <= 1 {
        print_usage(prog);
        return Err((String::new(), 64));
    }

    let mut partitions = Vec::new();
    let mut slot: u32 = 0;
    let mut show_info = false;
    let mut format = InfoFormat::Text;
    let mut positionals: Vec<String> = Vec::new();
    let mut stop_flags = false;

    let mut i = 1;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        if !stop_flags && arg == "--" {
            stop_flags = true;
            i += 1;
            continue;
        }

        if !stop_flags && arg.starts_with("--") {
            let (flag, inline_val) = match arg.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (arg.as_str(), None),
            };
            match flag {
                "--help" => {
                    print_usage(prog);
                    return Ok(None);
                }
                "--info" => {
                    if inline_val.is_some() {
                        return Err(("--info does not take a value.".into(), 64));
                    }
                    show_info = true;
                }
                "--no-info" => {
                    if inline_val.is_some() {
                        return Err(("--no-info does not take a value.".into(), 64));
                    }
                    show_info = false;
                }
                "--partition" => {
                    let val = match inline_val {
                        Some(v) => v.to_string(),
                        None => {
                            i += 1;
                            raw_args
                                .get(i)
                                .cloned()
                                .ok_or_else(|| ("--partition requires a value.".into(), 64))?
                        }
                    };
                    split_partition_names(&val, &mut partitions);
                }
                "--slot" => {
                    let val = match inline_val {
                        Some(v) => v.to_string(),
                        None => {
                            i += 1;
                            raw_args
                                .get(i)
                                .cloned()
                                .ok_or_else(|| ("--slot requires a value.".into(), 64))?
                        }
                    };
                    slot = parse_slot(&val).map_err(|e| (e, 64))?;
                }
                "--format" => {
                    let val = match inline_val {
                        Some(v) => v.to_string(),
                        None => {
                            i += 1;
                            raw_args
                                .get(i)
                                .cloned()
                                .ok_or_else(|| ("--format requires a value.".into(), 64))?
                        }
                    };
                    format = match val.as_str() {
                        "text" => InfoFormat::Text,
                        "json" => InfoFormat::Json,
                        _ => {
                            return Err((
                                format!("Invalid format '{val}': expected 'text' or 'json'."),
                                64,
                            ))
                        }
                    };
                }
                _ => return Err((format!("Unrecognized argument: {arg}"), 64)),
            }
        } else if !stop_flags && arg.starts_with('-') && arg.len() > 1 {
            let mut chars = arg[1..].chars();
            while let Some(ch) = chars.next() {
                match ch {
                    'h' => {
                        print_usage(prog);
                        return Ok(None);
                    }
                    'p' => {
                        let rest: String = chars.collect();
                        let val = if !rest.is_empty() {
                            rest
                        } else {
                            i += 1;
                            raw_args
                                .get(i)
                                .cloned()
                                .ok_or_else(|| ("-p requires a value.".into(), 64))?
                        };
                        split_partition_names(&val, &mut partitions);
                        break;
                    }
                    'S' => {
                        let rest: String = chars.collect();
                        let val = if !rest.is_empty() {
                            rest
                        } else {
                            i += 1;
                            raw_args
                                .get(i)
                                .cloned()
                                .ok_or_else(|| ("-S requires a value.".into(), 64))?
                        };
                        slot = parse_slot(&val).map_err(|e| (e, 64))?;
                        break;
                    }
                    'f' => {
                        let rest: String = chars.collect();
                        let val = if !rest.is_empty() {
                            rest
                        } else {
                            i += 1;
                            raw_args
                                .get(i)
                                .cloned()
                                .ok_or_else(|| ("-f requires a value.".into(), 64))?
                        };
                        format = match val.as_str() {
                            "text" => InfoFormat::Text,
                            "json" => InfoFormat::Json,
                            _ => {
                                return Err((
                                    format!("Invalid format '{val}': expected 'text' or 'json'."),
                                    64,
                                ))
                            }
                        };
                        break;
                    }
                    _ => return Err((format!("Unrecognized argument: -{ch}"), 64)),
                }
            }
        } else {
            positionals.push(arg.clone());
        }
        i += 1;
    }

    if positionals.is_empty() {
        return Err(("Missing super image argument.".into(), 64));
    }
    if positionals.len() > 2 {
        return Err(("Unrecognized command-line arguments.".into(), 64));
    }

    let super_image = PathBuf::from(&positionals[0]);
    let output_dir = positionals.get(1).map(PathBuf::from);

    Ok(Some(CliArgs {
        partitions,
        slot,
        show_info,
        format,
        super_image,
        output_dir,
    }))
}

struct SparseHoleWriter {
    output: File,
    block_size: usize,
    hole_size: u64,
    written_pos: u64,
}

impl SparseHoleWriter {
    fn new(output: File, block_size: u32) -> Self {
        Self {
            output,
            block_size: block_size as usize,
            hole_size: 0,
            written_pos: 0,
        }
    }

    fn write_extent<R: Read + Seek>(
        &mut self,
        source: &mut R,
        extent: &LpMetadataExtent,
    ) -> Result<(), LpError> {
        let total_bytes = extent.num_sectors * LP_SECTOR_SIZE;
        if extent.target_type == LP_TARGET_TYPE_ZERO {
            self.hole_size += total_bytes;
            return Ok(());
        }
        if extent.target_type != LP_TARGET_TYPE_LINEAR {
            return Err(LpError::InvalidMetadata(format!(
                "Unsupported target type in extent: {}",
                extent.target_type
            )));
        }

        let super_offset = extent.target_data * LP_SECTOR_SIZE;
        source.seek(SeekFrom::Start(super_offset))?;

        let batch_size = self.block_size * ((1024 * 1024) / self.block_size).max(1);
        let mut buf = vec![0u8; batch_size];
        let mut remaining = total_bytes;
        while remaining > 0 {
            let to_read = (remaining as usize).min(batch_size);
            source.read_exact(&mut buf[..to_read])?;
            self.write_batched(&buf[..to_read])?;
            remaining -= to_read as u64;
        }
        Ok(())
    }

    fn write_batched(&mut self, data: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        let mut run_start: Option<usize> = None;

        while offset < data.len() {
            let end = (offset + self.block_size).min(data.len());
            let block = &data[offset..end];
            if block.iter().all(|&b| b == 0) {
                if let Some(start) = run_start.take() {
                    self.flush_nonzero(&data[start..offset])?;
                }
                self.hole_size += block.len() as u64;
            } else if run_start.is_none() {
                run_start = Some(offset);
            }
            offset = end;
        }

        if let Some(start) = run_start {
            self.flush_nonzero(&data[start..])?;
        }
        Ok(())
    }

    fn flush_nonzero(&mut self, data: &[u8]) -> io::Result<()> {
        if self.hole_size > 0 {
            self.output.seek(SeekFrom::Current(self.hole_size as i64))?;
            self.written_pos += self.hole_size;
            self.hole_size = 0;
        }
        self.output.write_all(data)?;
        self.written_pos += data.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        if self.hole_size > 0 {
            self.output.set_len(self.written_pos + self.hole_size)?;
        }
        self.output.flush()?;
        Ok(())
    }
}

fn open_device_source(
    super_path: &Path,
    primary_source: &mut ImageSource<File>,
    metadata: &LpMetadata,
    target_source: u32,
) -> Result<Option<ImageSource<File>>, LpError> {
    if target_source == 0 {
        return Ok(None);
    }
    let dev_name = metadata
        .block_devices
        .get(target_source as usize)
        .map(|d| d.partition_name())
        .ok_or_else(|| {
            LpError::InvalidMetadata(format!(
                "Extent references unknown block device index {target_source}"
            ))
        })?;
    let parent = super_path.parent().unwrap_or_else(|| Path::new("."));
    let sibling = parent.join(format!("super_{dev_name}.img"));
    if sibling.exists() {
        let file = File::open(&sibling)?;
        return Ok(Some(ImageSource::open(file)?));
    }
    let _ = primary_source;
    Err(LpError::InvalidMetadata(format!(
        "Split super devices are not supported (missing '{}').",
        sibling.display()
    )))
}

fn extract_partition(
    super_path: &Path,
    primary_source: &mut ImageSource<File>,
    metadata: &LpMetadata,
    partition: &LpMetadataPartition,
    output_dir: &Path,
) -> Result<(), LpError> {
    let part_name = partition.name();
    print!("Extracting partition [{part_name}] ....");
    io::stdout().flush()?;

    let out_path = output_dir.join(format!("{part_name}.img"));
    let out_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&out_path)?;

    let mut writer = SparseHoleWriter::new(out_file, metadata.geometry.logical_block_size);

    for i in 0..partition.num_extents {
        let idx = (partition.first_extent_index + i) as usize;
        let extent = &metadata.extents[idx];
        if extent.target_type == LP_TARGET_TYPE_LINEAR && extent.target_source != 0 {
            if let Some(mut alt_src) =
                open_device_source(super_path, primary_source, metadata, extent.target_source)?
            {
                writer.write_extent(&mut alt_src, extent)?;
            }
        } else {
            writer.write_extent(primary_source, extent)?;
        }
    }

    writer.finish()?;
    println!(" [ok]");
    Ok(())
}

fn run(args: CliArgs) -> Result<(), (String, i32)> {
    let file = File::open(&args.super_image).map_err(|e| {
        (
            format!("open failed: {}: {e}", args.super_image.display()),
            71,
        )
    })?;
    let mut source = ImageSource::open(file).map_err(|e| (e.to_string(), 65))?;
    if source.is_sparse() && !args.show_info {
        println!("Sparse image detected.");
    }

    let (mut metadata, is_empty) =
        read_metadata_auto(&mut source, args.slot).map_err(|e| (e.to_string(), 65))?;

    if !args.partitions.is_empty() {
        let mut remaining: HashSet<String> = args.partitions.iter().cloned().collect();
        let mut filtered = Vec::new();
        for part in &metadata.partitions {
            let name = part.name();
            if remaining.remove(&name) {
                filtered.push(*part);
            }
        }
        if let Some(missing) = remaining.iter().next() {
            return Err((format!("Could not find partition: {missing}"), 70));
        }
        metadata.partitions = filtered;
    }

    if args.show_info {
        match args.format {
            InfoFormat::Text => print!("{}", format_metadata_text(&metadata, args.slot)),
            InfoFormat::Json => println!("{}", format_metadata_json(&metadata)),
        }
    }

    let extract_dir = match args.output_dir {
        Some(dir) => Some(dir),
        None if !args.show_info => Some(PathBuf::from(".")),
        None => None,
    };

    if let Some(out_dir) = extract_dir {
        if is_empty {
            return Err((
                "Empty super image contains metadata only and cannot be unpacked.".into(),
                64,
            ));
        }
        if !out_dir.exists() {
            fs::create_dir_all(&out_dir).map_err(|e| {
                (
                    format!("Failed to create output directory '{}': {e}", out_dir.display()),
                    73,
                )
            })?;
        }
        for part in &metadata.partitions {
            extract_partition(&args.super_image, &mut source, &metadata, part, &out_dir)
                .map_err(|e| (e.to_string(), 70))?;
        }
    }

    Ok(())
}

fn main() {
    match parse_args() {
        Ok(None) => process::exit(0),
        Ok(Some(args)) => {
            if let Err((msg, code)) = run(args) {
                if !msg.is_empty() {
                    eprintln!("{msg}");
                }
                process::exit(code);
            }
        }
        Err((msg, code)) => {
            if !msg.is_empty() {
                eprintln!("{msg}");
            }
            process::exit(code);
        }
    }
}
