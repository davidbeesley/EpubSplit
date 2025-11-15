use anyhow::{Context, Result};
use clap::Parser;
use log::{debug, info};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "epubsplit",
    about = "Split EPUB files into multiple books",
    long_about = "Giving an epub without line numbers will return a list of line numbers: the \
                  possible split points in the input file. Calling with line numbers will \
                  generate an epub with each of the \"lines\" given included."
)]
struct Cli {
    /// Input EPUB file to split
    input: PathBuf,

    /// Line numbers of sections to include in output
    #[arg(value_name = "LINE")]
    lines: Vec<usize>,

    /// Output file name
    #[arg(short, long, default_value = "split.epub")]
    output: String,

    /// Output directory
    #[arg(long)]
    output_dir: Option<PathBuf>,

    /// Create a new epub from each listed section instead of one containing all
    #[arg(long)]
    split_by_section: bool,

    /// Metadata title for output epub
    #[arg(short, long)]
    title: Option<String>,

    /// Metadata description for output epub
    #[arg(short, long)]
    description: Option<String>,

    /// Metadata author(s) for output epub (can be specified multiple times)
    #[arg(short, long)]
    author: Vec<String>,

    /// Subject tag(s) for output epub (can be specified multiple times)
    #[arg(short = 'g', long)]
    tag: Vec<String>,

    /// Language(s) for output epub (can be specified multiple times)
    #[arg(short, long, default_value = "en")]
    language: Vec<String>,

    /// Path to cover image (JPG)
    #[arg(short, long)]
    cover: Option<PathBuf>,

    /// Enable debug output
    #[arg(long)]
    debug: bool,
}

/// Represents a split point in the EPUB
#[derive(Debug)]
#[allow(dead_code)]
struct SplitLine {
    toc: Vec<String>,
    guide: Option<String>,
    anchor: Option<String>,
    id: Option<String>,
    href: String,
}

/// Main EPUB splitting engine
#[allow(dead_code)]
struct SplitEpub {
    path: PathBuf,
}

impl SplitEpub {
    fn new(_path: PathBuf) -> Result<Self> {
        todo!("Load and parse EPUB file")
    }

    fn get_split_lines(&self) -> Result<Vec<SplitLine>> {
        todo!("Extract split points from EPUB structure")
    }

    #[allow(dead_code)]
    fn write_split_epub(
        &self,
        _output_path: PathBuf,
        _section_indices: &[usize],
        _authors: &[String],
        _title: Option<&str>,
        _description: Option<&str>,
        _tags: &[String],
        _languages: &[String],
        _cover_path: Option<&PathBuf>,
    ) -> Result<()> {
        todo!("Write split EPUB to output path")
    }
}

fn list_split_points(_lines: &[SplitLine]) -> Result<()> {
    todo!("Display available split points to user")
}

fn split_by_section(
    _epub: &SplitEpub,
    _lines: &[SplitLine],
    _section_indices: &[usize],
    _cli: &Cli,
) -> Result<()> {
    todo!("Split EPUB into multiple files, one per section")
}

fn extract_sections(_epub: &SplitEpub, _section_indices: &[usize], _cli: &Cli) -> Result<()> {
    todo!("Extract specified sections into single output EPUB")
}

fn ensure_epub_extension(filename: &str) -> String {
    if filename.to_lowercase().ends_with(".epub") {
        filename.to_string()
    } else {
        format!("{}.epub", filename)
    }
}

fn run(cli: Cli) -> Result<()> {
    debug!("CLI arguments: {:?}", cli);

    let output_filename = ensure_epub_extension(&cli.output);
    info!("Output filename: {}", output_filename);

    // Load the EPUB file
    let epub = SplitEpub::new(cli.input.clone())
        .with_context(|| format!("Failed to load EPUB: {}", cli.input.display()))?;

    // Get available split points
    let lines = epub
        .get_split_lines()
        .context("Failed to extract split points from EPUB")?;

    if cli.split_by_section {
        // Mode: Split into separate files per section
        let indices = if cli.lines.is_empty() {
            (0..lines.len()).collect::<Vec<_>>()
        } else {
            cli.lines.clone()
        };
        split_by_section(&epub, &lines, &indices, &cli)?;
    } else if cli.lines.is_empty() {
        // Mode: List available split points
        list_split_points(&lines)?;
    } else {
        // Mode: Extract specific sections into one file
        extract_sections(&epub, &cli.lines, &cli)?;
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logger based on debug flag
    let log_level = if cli.debug { "debug" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    run(cli)
}
