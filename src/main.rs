use std::{
    fs::{File, Metadata, OpenOptions},
    io::{prelude::*, SeekFrom},
    os::unix::fs::PermissionsExt,
    os::unix::{fs::OpenOptionsExt, prelude::MetadataExt},
    path::{Path, PathBuf}, collections::HashSet,
};

use clap::{Args, Parser, Subcommand};
use flate2::{read::DeflateDecoder, write::DeflateEncoder};
use flate2::{Compression as DeflateCompression, CrcReader, CrcWriter};
use lz4_flex::frame::FrameDecoder as Lz4Decoder;
use lz4_flex::frame::FrameEncoder as Lz4Encoder;
use xz2::{read::XzDecoder, write::XzEncoder};
use zstd::stream::read::Decoder as ZstdDecoder;
use zstd::stream::write::Encoder as ZstdEncoder;

#[derive(Args)]
struct CliActionAdd {
    files: Vec<String>,

    #[arg(short = 'o', long = "output", name = "ARCHIVE.ooo")]
    archive: PathBuf,

    #[arg(short, long, default_value = "zstd")]
    /// Filter to apply to file data (none / zstd / lzma / lz4 / flate)
    filter: String,

    #[arg(short, long, default_value = "6", value_parser = clap::value_parser!(u8).range(0..=9))]
    /// Compression level (between 0-9)
    level: u8,
}

#[derive(Args)]
struct CliActionExtract {
    archive: PathBuf,

    #[arg(short = 'o', long = "output", name = "OUTPUT_DIR")]
    output: PathBuf,

    #[arg(short = 'f', long = "only-files", name = "FILE")]
    only_files: Vec<String>,
}

#[derive(Args)]
struct CliActionList {
    archive: PathBuf,
}

#[derive(Subcommand)]
enum CliAction {
    #[command(visible_alias = "a")]
    /// Add files to an archive
    Add(CliActionAdd),

    #[command(visible_alias = "x")]
    /// Extract files from an archive
    Extract(CliActionExtract),

    #[command(visible_alias = "l")]
    /// List files contained in an archive
    List(CliActionList),
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    action: CliAction,

    #[arg(short, long, global = true)]
    verbose: bool,
}

const BUFFER_SIZE: usize = 16384;

/// Wrapper around a writer that counts all bytes written
struct CountingWriter<W: Write> {
    inner: W,
    bytes_written: u64,
}

impl<W: Write> CountingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

struct BoundedReader<'a, R: Read> {
    inner: &'a mut R,
    bytes_left: usize,
}

impl<'a, R: Read> BoundedReader<'a, R> {
    pub fn new(inner: &'a mut R, bytes_to_read: usize) -> Self {
        Self {
            inner,
            bytes_left: bytes_to_read,
        }
    }
}

impl<R: Read> Read for BoundedReader<'_, R> {
    fn read(&mut self, mut buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.len() > self.bytes_left {
            buf = &mut buf[..self.bytes_left];
        }

        let bytes_read = self.inner.read(buf)?;
        self.bytes_left -= bytes_read;
        Ok(bytes_read)
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes_written += written as u64;
        Ok(written)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.bytes_written += buf.len() as u64;
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Extract all files from an archive
fn extract(
    archive_path: PathBuf,
    output_path: PathBuf,
    verbose: bool,
    only_files: HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut archive = File::open(&archive_path)?;
    let mut corrupted = false;

    walk_through_archive(&mut archive, |archive, dict, archive_size_left| {
        let original_name = String::from_utf8(dict.name)?;
        let filename = output_path.join(&original_name);

        if only_files.len() != 0 && !only_files.contains(&original_name) {
            return Ok(());
        }

        if verbose {
            let virtual_path = archive_path.join(&original_name);
            println!(
                "(-) {} -> {}",
                virtual_path.to_string_lossy(),
                filename.to_string_lossy()
            );
        }

        if let Some(basedir) = filename.parent() {
            std::fs::create_dir_all(basedir)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .mode(dict.mode)
            .open(&filename)?;

        archive.seek(SeekFrom::Start(archive_size_left))?;

        macro_rules! write_file {
            ($file_writer: expr) => {{
                archive.flush()?;

                let mut archive_reader = BoundedReader::new(archive, dict.size as _);

                match dict.filter.as_slice() {
                    b"none" => {
                        std::io::copy(&mut archive_reader, &mut $file_writer)?;
                    }
                    b"flate" => {
                        let mut flate = DeflateDecoder::new(archive_reader);
                        std::io::copy(&mut flate, &mut $file_writer)?;
                    }
                    b"zstd" => {
                        let mut zstd = ZstdDecoder::new(archive_reader)?;
                        std::io::copy(&mut zstd, &mut $file_writer)?;
                        zstd.finish();
                    }
                    b"lzma" => {
                        let mut lzma = XzDecoder::new(archive_reader);
                        std::io::copy(&mut lzma, &mut $file_writer)?;
                    }
                    b"lz4" => {
                        let mut lz4 = Lz4Decoder::new(archive_reader);
                        std::io::copy(&mut lz4, &mut $file_writer)?;
                    }
                    _ => {
                        return Err(format!(
                            "unknown filter type '{}'",
                            String::from_utf8_lossy(&dict.filter)
                        )
                        .into())
                    }
                }
            }};
        }

        if dict.crc == 0 {
            write_file!(file);
        } else {
            let mut file_writer = CrcWriter::new(file);
            write_file!(file_writer);

            let crc = file_writer.crc().sum();
            if crc != dict.crc {
                eprintln!("(!) file corrupted: {}", filename.to_string_lossy());
                corrupted = true;
            } else if verbose {
                println!("(#) crc ok");
            }
        }

        Ok(())
    })?;

    if corrupted {
        Err("one or more files extracted from this archive has been corrupted".into())
    } else {
        Ok(())
    }
}

/// List archive entries
fn list(archive_path: PathBuf, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut archive = File::open(&archive_path)?;
    walk_through_archive(&mut archive, |_archive, entry, offset| {
        if verbose {
            println!(
                "{} (type={:?} size={} mode={:o} filter={:?} crc={} target={:?}) @ offset={}",
                String::from_utf8_lossy(&entry.name),
                String::from_utf8_lossy(&entry.entry_type),
                entry.size,
                entry.mode,
                String::from_utf8_lossy(&entry.filter),
                entry.crc,
                String::from_utf8_lossy(&entry.target),
                offset,
            );
        } else {
            println!("{}", String::from_utf8_lossy(&entry.name));
        }

        Ok(())
    })?;

    Ok(())
}

/// Walk through all elements of the archive
fn walk_through_archive<F>(
    archive: &mut File,
    mut on_entry: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut(&mut File, ArchiveEntryMeta, u64) -> Result<(), Box<dyn std::error::Error>>,
{
    let archive_size = archive.metadata()?.len();
    let mut archive_size_left = archive_size;

    macro_rules! read_at {
        ($pos: expr) => {{
            let mut one = [0; 1];
            archive.seek(SeekFrom::Start($pos))?;
            archive.read_exact(&mut one)?;
            one[0]
        }};
    }

    loop {
        if archive_size_left == 0 {
            break;
        }

        if read_at!(archive_size_left - 1) != b')' {
            panic!("can't find ')'");
        }

        let mut size = 1;
        let mut parenthesis_depth = 1u32;
        let mut acc = Vec::with_capacity(BUFFER_SIZE);
        loop {
            let c = read_at!(archive_size_left - size - 1);
            match c {
                b')' => parenthesis_depth += 1,
                b'(' => {
                    parenthesis_depth -= 1;
                    if parenthesis_depth == 0 {
                        size += 1;
                        break;
                    }
                }
                _ => (),
            }

            size += 1;
            acc.push(c);
        }

        acc.reverse();

        let dict = ArchiveEntryMeta::parse(&acc);
        archive_size_left -= size + dict.size;

        on_entry(archive, dict, archive_size_left)?;
    }

    Ok(())
}

/// An archive entry's metadata
struct ArchiveEntryMeta {
    pub entry_type: Vec<u8>,
    pub size: u64,
    pub mode: u32,
    pub name: Vec<u8>,
    pub filter: Vec<u8>,
    pub crc: u32,
    pub target: Vec<u8>,
}

impl ArchiveEntryMeta {
    /// Create an archive entry's metadata by reading a dictionary (e.g. "size=12 mode=1234")
    fn parse(dict: &[u8]) -> Self {
        let mut result = Self {
            entry_type: b"file".to_vec(),
            size: 0,
            mode: 0o664,
            name: Vec::new(),
            filter: b"none".to_vec(),
            crc: 0,
            target: Vec::new(),
        };

        // Read each field
        let mut i = 0;
        let mut current_id = Vec::with_capacity(16);
        while let Some(&c) = dict.get(i) {
            if c == b' ' {
                i += 1;
                continue;
            }

            if c != b'=' {
                current_id.push(c);
                i += 1;
                continue;
            }

            i += 1;

            match current_id.as_slice() {
                b"size" => result.size = parse_u64(&mut i, dict),
                b"mode" => result.mode = parse_mode(&mut i, dict),
                b"name" => result.name = parse_byte_string(&mut i, dict),
                b"type" => result.entry_type = parse_byte_string(&mut i, dict),
                b"target" => result.target = parse_byte_string(&mut i, dict),
                b"filter" => result.filter = parse_byte_string(&mut i, dict),
                b"crc" => result.crc = parse_u32(&mut i, dict),
                _ => panic!("unknown id '{}'", String::from_utf8_lossy(&current_id)),
            }

            current_id.clear();
        }

        result
    }
}

/// Parse next string
fn parse_byte_string(index: &mut usize, dict: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    *index += 1;

    while let Some(&c) = dict.get(*index) {
        if c == b'\\' {
            result.push(*dict.get(*index).expect("bad esc sequence"));
            *index += 2;
        } else if c == b'"' {
            *index += 2;
            break;
        } else {
            result.push(c);
            *index += 1;
        }
    }

    result
}

/// Read next u64
fn parse_u64(index: &mut usize, dict: &[u8]) -> u64 {
    let mut result = 0;

    while let Some(&c) = dict.get(*index) {
        if !matches!(c, b'0'..=b'9') {
            break;
        }

        result *= 10;
        result += (c - b'0') as u64;

        *index += 1;
    }

    result
}

/// Read next u32
fn parse_u32(index: &mut usize, dict: &[u8]) -> u32 {
    let mut result = 0;

    while let Some(&c) = dict.get(*index) {
        if !matches!(c, b'0'..=b'9') {
            break;
        }

        result *= 10;
        result += (c - b'0') as u32;

        *index += 1;
    }

    result
}

/// Read next file mode (octal u32)
fn parse_mode(index: &mut usize, dict: &[u8]) -> u32 {
    let mut result = 0;

    while let Some(&c) = dict.get(*index) {
        if !matches!(c, b'0'..=b'7') {
            break;
        }

        result *= 8;
        result += (c - b'0') as u32;

        *index += 1;
    }

    result
}

/// Add a symlink to the end of an archive
fn add_symlink(
    mut archive: File,
    metadata: Metadata,
    filename: &Path,
    output_name: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let target_path = std::fs::read_link(filename)?;
    let mode = metadata.mode();

    write!(
        &mut archive,
        "(name={:?} type=\"symlink\" target={:?} mode={:o})",
        output_name.to_string_lossy(),
        target_path,
        mode,
    )?;

    Ok(())
}

/// Add a file to the end of an archive
fn compress(
    archive_path: &Path,
    filename: &Path,
    output_name: &Path,
    filter: &str,
    level: u8,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if verbose {
        let virtual_path = archive_path.join(output_name);
        println!(
            "(+) {} -> {}",
            filename.to_string_lossy(),
            virtual_path.to_string_lossy()
        );
    }

    let archive = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&archive_path)?;

    let symlink_metadata = std::fs::symlink_metadata(filename)?;
    if symlink_metadata.is_symlink() {
        return add_symlink(archive, symlink_metadata, filename, output_name);
    }

    let mut archive = CountingWriter::new(archive);
    let file = File::open(&filename)?;
    let mode = file.metadata()?.permissions().mode();
    let mut file_reader = CrcReader::new(file);

    match filter {
        "none" => {
            std::io::copy(&mut file_reader, &mut archive)?;
        }
        "zstd" => {
            let mut zstd = ZstdEncoder::new(archive, level as _)?;
            std::io::copy(&mut file_reader, &mut zstd)?;
            archive = zstd.finish()?;
        }
        "flate" => {
            let mut flate = DeflateEncoder::new(archive, DeflateCompression::new(level as _));
            std::io::copy(&mut file_reader, &mut flate)?;
            archive = flate.finish()?;
        }
        "lzma" => {
            let mut lzma = XzEncoder::new(archive, level as _);
            std::io::copy(&mut file_reader, &mut lzma)?;
            archive = lzma.finish()?;
        }
        "lz4" => {
            let mut lz4 = Lz4Encoder::new(archive);
            std::io::copy(&mut file_reader, &mut lz4)?;
            archive = lz4.finish()?;
        }
        _ => return Err(format!("unknown filter '{}'", filter).into()),
    }

    let crc = file_reader.crc();
    let bytes_written = archive.bytes_written();

    write!(
        &mut archive,
        "(size={} name={:?} mode={:o} filter=\"{}\" crc={})",
        bytes_written,
        output_name.as_os_str().to_string_lossy(),
        mode,
        filter,
        crc.sum(),
    )?;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.action {
        CliAction::Add(action) => {
            for inout_names in &action.files {
                let (in_name, out_name) = match inout_names.split_once(':') {
                    None => (inout_names.as_str(), inout_names.as_str()),
                    Some((a, b)) => (a, b),
                };

                compress(
                    &action.archive,
                    &Path::new(in_name),
                    &Path::new(out_name),
                    &action.filter,
                    action.level,
                    cli.verbose,
                )?;
            }
        }
        CliAction::Extract(action) => extract(action.archive, action.output, cli.verbose, HashSet::from_iter(action.only_files))?,
        CliAction::List(action) => list(action.archive, cli.verbose)?,
    }

    Ok(())
}
