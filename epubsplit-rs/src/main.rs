use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use log::{debug, info, warn};
use percent_encoding::percent_decode_str;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read as IoRead, Write as IoWrite};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

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
#[derive(Debug, Clone)]
struct SplitLine {
    toc: Vec<String>,
    guide: Option<(String, String)>, // (type, title)
    anchor: Option<String>,
    id: String,
    href: String,
    media_type: String,
    sample: String,
}

/// Manifest item info
#[derive(Debug, Clone)]
struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
}

/// TOC entry
#[derive(Debug, Clone)]
struct TocEntry {
    text: String,
    anchor: Option<String>,
}

/// Main EPUB splitting engine
struct SplitEpub {
    archive: ZipArchive<BufReader<File>>,
    path: PathBuf,
    content_opf_path: String,
    content_relpath: String,
    manifest_items: HashMap<String, ManifestItem>,
    guide_items: HashMap<String, (String, String)>, // href -> (type, title)
    toc_map: HashMap<String, Vec<TocEntry>>,        // href -> [(text, anchor), ...]
    orig_title: String,
    orig_authors: Vec<String>,
}

impl SplitEpub {
    fn new(path: PathBuf) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open EPUB file: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).context("Failed to read EPUB as ZIP archive")?;

        // Find the .opf file from container.xml
        let container_xml = Self::read_file_from_archive(&mut archive, "META-INF/container.xml")?;
        let content_opf_path = Self::parse_container_xml(&container_xml)?;
        let content_relpath = Self::get_path_part(&content_opf_path);

        debug!("OPF path: {}", content_opf_path);
        debug!("Content relative path: {}", content_relpath);

        // Parse the OPF file
        let opf_content = Self::read_file_from_archive(&mut archive, &content_opf_path)?;
        let (manifest_items, toc_path) =
            Self::parse_manifest(&opf_content, &content_relpath)?;
        let guide_items = Self::parse_guide(&opf_content, &content_relpath)?;
        let (orig_title, orig_authors) = Self::parse_metadata(&opf_content)?;

        debug!("Found {} manifest items", manifest_items.len());
        debug!("Original title: {}", orig_title);
        debug!("Original authors: {:?}", orig_authors);

        // Parse TOC if available
        let toc_map = if let Some(toc_path) = toc_path {
            let toc_relpath = Self::get_path_part(&toc_path);
            let toc_content = Self::read_file_from_archive(&mut archive, &toc_path)?;
            Self::parse_toc(&toc_content, &toc_relpath)?
        } else {
            warn!("No TOC file found");
            HashMap::new()
        };

        debug!("Found {} TOC entries", toc_map.len());

        Ok(Self {
            archive,
            path,
            content_opf_path,
            content_relpath,
            manifest_items,
            guide_items,
            toc_map,
            orig_title,
            orig_authors,
        })
    }

    fn read_file_from_archive(
        archive: &mut ZipArchive<BufReader<File>>,
        path: &str,
    ) -> Result<String> {
        let mut file = archive
            .by_name(path)
            .with_context(|| format!("File not found in EPUB: {}", path))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("Failed to read file from EPUB: {}", path))?;
        Ok(contents)
    }

    fn get_path_part(path: &str) -> String {
        if let Some(pos) = path.rfind('/') {
            path[..=pos].to_string()
        } else {
            String::new()
        }
    }

    fn normalize_path(path: &str) -> String {
        // Simple path normalization - remove ../ and ./ segments
        let decoded = percent_decode_str(path).decode_utf8_lossy().to_string();
        let mut parts: Vec<&str> = Vec::new();

        for part in decoded.split('/') {
            match part {
                ".." => {
                    parts.pop();
                }
                "." | "" => {}
                _ => parts.push(part),
            }
        }

        parts.join("/")
    }

    fn parse_container_xml(xml: &str) -> Result<String> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        loop {
            match reader.read_event() {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                    if e.local_name().as_ref() == b"rootfile" =>
                {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"full-path" {
                            return Ok(String::from_utf8_lossy(&attr.value).to_string());
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => bail!("Error parsing container.xml: {}", e),
                _ => {}
            }
        }

        bail!("No rootfile found in container.xml")
    }

    fn parse_manifest(
        opf: &str,
        content_relpath: &str,
    ) -> Result<(HashMap<String, ManifestItem>, Option<String>)> {
        let mut items = HashMap::new();
        let mut toc_path = None;
        let mut reader = Reader::from_str(opf);
        reader.config_mut().trim_text(true);

        loop {
            match reader.read_event() {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                    if e.local_name().as_ref() == b"item" =>
                {
                    let mut id = String::new();
                    let mut href = String::new();
                    let mut media_type = String::new();

                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => id = String::from_utf8_lossy(&attr.value).to_string(),
                            b"href" => {
                                let raw_href = String::from_utf8_lossy(&attr.value).to_string();
                                href = Self::normalize_path(&format!(
                                    "{}{}",
                                    content_relpath, raw_href
                                ));
                            }
                            b"media-type" => {
                                media_type = String::from_utf8_lossy(&attr.value).to_string()
                            }
                            _ => {}
                        }
                    }

                    if !id.is_empty() {
                        // Check if this is the TOC file
                        if media_type == "application/x-dtbncx+xml" {
                            toc_path = Some(href.clone());
                        }

                        items.insert(
                            id.clone(),
                            ManifestItem {
                                id,
                                href,
                                media_type,
                            },
                        );
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => bail!("Error parsing OPF manifest: {}", e),
                _ => {}
            }
        }

        Ok((items, toc_path))
    }

    fn parse_guide(opf: &str, content_relpath: &str) -> Result<HashMap<String, (String, String)>> {
        let mut items = HashMap::new();
        let mut reader = Reader::from_str(opf);
        reader.config_mut().trim_text(true);

        loop {
            match reader.read_event() {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                    if e.local_name().as_ref() == b"reference" =>
                {
                    let mut href = String::new();
                    let mut ref_type = String::new();
                    let mut title = String::new();

                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"href" => {
                                let raw_href = String::from_utf8_lossy(&attr.value).to_string();
                                // Remove anchor part for guide lookup
                                let base_href = raw_href.split('#').next().unwrap_or(&raw_href);
                                href = Self::normalize_path(&format!(
                                    "{}{}",
                                    content_relpath, base_href
                                ));
                            }
                            b"type" => {
                                ref_type = String::from_utf8_lossy(&attr.value).to_string()
                            }
                            b"title" => title = String::from_utf8_lossy(&attr.value).to_string(),
                            _ => {}
                        }
                    }

                    if !href.is_empty() {
                        items.insert(href, (ref_type, title));
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => bail!("Error parsing OPF guide: {}", e),
                _ => {}
            }
        }

        Ok(items)
    }

    fn parse_metadata(opf: &str) -> Result<(String, Vec<String>)> {
        let mut title = String::from("(Title Missing)");
        let mut authors = Vec::new();
        let mut reader = Reader::from_str(opf);
        reader.config_mut().trim_text(true);

        let mut in_title = false;
        let mut in_creator = false;
        let mut creator_is_author = true;

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    let local_name = e.local_name();
                    if local_name.as_ref() == b"title"
                        || local_name.as_ref() == b"dc:title"
                    {
                        in_title = true;
                    } else if local_name.as_ref() == b"creator"
                        || local_name.as_ref() == b"dc:creator"
                    {
                        in_creator = true;
                        creator_is_author = true;
                        // Check for role attribute
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"opf:role" || attr.key.as_ref() == b"role" {
                                let role = String::from_utf8_lossy(&attr.value);
                                if role != "aut" {
                                    creator_is_author = false;
                                }
                            }
                        }
                    }
                }
                Ok(Event::Text(ref e)) => {
                    if in_title {
                        title = e.unescape().unwrap_or_default().to_string();
                        in_title = false;
                    } else if in_creator && creator_is_author {
                        let author = e.unescape().unwrap_or_default().to_string();
                        if !author.is_empty() && !authors.contains(&author) {
                            authors.push(author);
                        }
                        in_creator = false;
                    }
                }
                Ok(Event::End(_)) => {
                    in_title = false;
                    in_creator = false;
                }
                Ok(Event::Eof) => break,
                Err(e) => bail!("Error parsing OPF metadata: {}", e),
                _ => {}
            }
        }

        if authors.is_empty() {
            authors.push("(Authors Missing)".to_string());
        }

        Ok((title, authors))
    }

    fn parse_toc(toc_xml: &str, toc_relpath: &str) -> Result<HashMap<String, Vec<TocEntry>>> {
        let mut toc_map: HashMap<String, Vec<TocEntry>> = HashMap::new();
        let mut reader = Reader::from_str(toc_xml);
        reader.config_mut().trim_text(true);

        let mut in_nav_point = false;
        let mut in_text = false;
        let mut current_text = String::new();
        let mut current_src = String::new();
        let mut depth = 0;

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    if e.local_name().as_ref() == b"navPoint" {
                        in_nav_point = true;
                        depth += 1;
                    } else if e.local_name().as_ref() == b"text" && in_nav_point {
                        in_text = true;
                    } else if e.local_name().as_ref() == b"content" && in_nav_point && depth == 1 {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"src" {
                                let raw_src = String::from_utf8_lossy(&attr.value).to_string();
                                current_src =
                                    Self::normalize_path(&format!("{}{}", toc_relpath, raw_src));
                            }
                        }
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    if e.local_name().as_ref() == b"content" && in_nav_point && depth == 1 {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"src" {
                                let raw_src = String::from_utf8_lossy(&attr.value).to_string();
                                current_src =
                                    Self::normalize_path(&format!("{}{}", toc_relpath, raw_src));
                            }
                        }
                    }
                }
                Ok(Event::Text(ref e)) => {
                    if in_text {
                        current_text = e.unescape().unwrap_or_default().trim().to_string();
                    }
                }
                Ok(Event::End(ref e)) => {
                    if e.local_name().as_ref() == b"navPoint" {
                        if depth == 1 && !current_src.is_empty() {
                            let (href, anchor) = if current_src.contains('#') {
                                let parts: Vec<&str> = current_src.splitn(2, '#').collect();
                                (parts[0].to_string(), Some(parts[1].to_string()))
                            } else {
                                (current_src.clone(), None)
                            };

                            let entry = TocEntry {
                                text: current_text.clone(),
                                anchor: anchor.clone(),
                            };

                            let entries = toc_map.entry(href).or_default();

                            // Put file links (no anchor) before anchor links
                            if anchor.is_none() {
                                let insert_pos = entries.iter().take_while(|e| e.anchor.is_none()).count();
                                entries.insert(insert_pos, entry);
                            } else {
                                entries.push(entry);
                            }
                        }

                        depth -= 1;
                        if depth == 0 {
                            in_nav_point = false;
                            current_text.clear();
                            current_src.clear();
                        }
                    } else if e.local_name().as_ref() == b"text" {
                        in_text = false;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => bail!("Error parsing TOC: {}", e),
                _ => {}
            }
        }

        Ok(toc_map)
    }

    fn get_split_lines(&mut self) -> Result<Vec<SplitLine>> {
        let mut split_lines = Vec::new();

        // Parse spine from OPF
        let opf_content =
            Self::read_file_from_archive(&mut self.archive, &self.content_opf_path)?;
        let spine_refs = Self::parse_spine(&opf_content)?;

        debug!("Found {} spine items", spine_refs.len());

        for idref in spine_refs {
            let item = self
                .manifest_items
                .get(&idref)
                .ok_or_else(|| anyhow!("Spine reference not found in manifest: {}", idref))?
                .clone();

            // Read sample content
            let content = Self::read_file_from_archive(&mut self.archive, &item.href)
                .unwrap_or_default();
            let sample = if content.len() > 1500 {
                format!("{}...", &content[..1500])
            } else {
                content.clone()
            };

            let mut current_line = SplitLine {
                toc: Vec::new(),
                guide: self.guide_items.get(&item.href).cloned(),
                anchor: None,
                id: item.id.clone(),
                href: item.href.clone(),
                media_type: item.media_type.clone(),
                sample,
            };

            // Check if this href has TOC entries
            if let Some(toc_entries) = self.toc_map.get(&item.href) {
                for entry in toc_entries {
                    if let Some(anchor) = &entry.anchor {
                        // This TOC entry has an anchor - add current line and start a new one
                        split_lines.push(current_line);

                        // Get sample content from anchor point
                        let anchor_sample =
                            Self::split_html_at_anchor(&content, anchor).unwrap_or_default();
                        let anchor_sample = if anchor_sample.len() > 1500 {
                            format!("{}...", &anchor_sample[..1500])
                        } else {
                            anchor_sample
                        };

                        current_line = SplitLine {
                            toc: vec![entry.text.clone()],
                            guide: None,
                            anchor: Some(anchor.clone()),
                            id: item.id.clone(),
                            href: item.href.clone(),
                            media_type: item.media_type.clone(),
                            sample: anchor_sample,
                        };
                    } else {
                        // No anchor - add text to current line's TOC
                        current_line.toc.push(entry.text.clone());
                    }
                }
            }

            split_lines.push(current_line);
        }

        Ok(split_lines)
    }

    fn parse_spine(opf: &str) -> Result<Vec<String>> {
        let mut spine_refs = Vec::new();
        let mut reader = Reader::from_str(opf);
        reader.config_mut().trim_text(true);

        loop {
            match reader.read_event() {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                    if e.local_name().as_ref() == b"itemref" =>
                {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"idref" {
                            spine_refs.push(String::from_utf8_lossy(&attr.value).to_string());
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => bail!("Error parsing OPF spine: {}", e),
                _ => {}
            }
        }

        Ok(spine_refs)
    }

    fn split_html_at_anchor(html: &str, anchor: &str) -> Option<String> {
        // Simple implementation: find the anchor and return content from there
        let patterns = [
            format!(r#"id="{}""#, anchor),
            format!(r#"id='{}'"#, anchor),
            format!(r#"name="{}""#, anchor),
            format!(r#"name='{}'"#, anchor),
        ];

        for pattern in &patterns {
            if let Some(pos) = html.find(pattern) {
                return Some(html[pos..].to_string());
            }
        }

        None
    }

    fn write_split_epub(
        &mut self,
        output_path: PathBuf,
        section_indices: &[usize],
        authors: &[String],
        title: Option<&str>,
        description: Option<&str>,
        tags: &[String],
        languages: &[String],
        cover_path: Option<&PathBuf>,
    ) -> Result<()> {
        // Get split lines if not already loaded
        let split_lines = self.get_split_lines()?;

        // Validate indices
        for &idx in section_indices {
            if idx >= split_lines.len() {
                bail!(
                    "Section index {} is out of range (max: {})",
                    idx,
                    split_lines.len() - 1
                );
            }
        }

        let indices_set: HashSet<usize> = section_indices.iter().copied().collect();

        // Collect files to include and linked resources
        let mut content_files: Vec<(String, String, String)> = Vec::new(); // (href, id, media_type)
        let mut linked_files: HashSet<String> = HashSet::new();
        let mut toc_entries: Vec<(String, String)> = Vec::new(); // (title, href)
        let mut included_hrefs: HashSet<String> = HashSet::new();

        for (idx, line) in split_lines.iter().enumerate() {
            if indices_set.contains(&idx) {
                // Add content file if not already added
                if !included_hrefs.contains(&line.href) {
                    included_hrefs.insert(line.href.clone());
                    content_files.push((
                        line.href.clone(),
                        line.id.clone(),
                        line.media_type.clone(),
                    ));

                    // Scan for linked resources
                    if let Ok(content) =
                        Self::read_file_from_archive(&mut self.archive, &line.href)
                    {
                        self.scan_for_linked_files(&content, &line.href, &mut linked_files)?;
                    }
                }

                // Add TOC entries
                for toc_text in &line.toc {
                    let href = if let Some(anchor) = &line.anchor {
                        format!("{}#{}", line.href, anchor)
                    } else {
                        line.href.clone()
                    };
                    toc_entries.push((toc_text.clone(), href));
                }
            }
        }

        // Create output file
        let output_file = File::create(&output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
        let mut zip = ZipWriter::new(output_file);

        // Write mimetype first (must be uncompressed and first)
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zip.start_file("mimetype", options)
            .context("Failed to write mimetype")?;
        zip.write_all(b"application/epub+zip")
            .context("Failed to write mimetype content")?;

        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        // Write META-INF/container.xml
        let container_xml = self.generate_container_xml();
        zip.start_file("META-INF/container.xml", options)
            .context("Failed to create container.xml")?;
        zip.write_all(container_xml.as_bytes())
            .context("Failed to write container.xml")?;

        // Generate unique ID
        let unique_id = format!(
            "epubsplit-uid-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );

        // Determine title
        let default_title = format!("{} Split", self.orig_title);
        let final_title = title.unwrap_or(&default_title);

        // Determine description
        let final_description = description.map(|d| d.to_string()).unwrap_or_else(|| {
            format!(
                "Split from {} by {}.",
                self.orig_title,
                self.orig_authors.join(", ")
            )
        });

        // Build manifest items
        let mut manifest_items: Vec<(String, String, String)> = Vec::new(); // (id, href, media-type)

        // Add NCX to manifest
        manifest_items.push((
            "ncx".to_string(),
            "toc.ncx".to_string(),
            "application/x-dtbncx+xml".to_string(),
        ));

        // Add cover if provided
        if cover_path.is_some() {
            manifest_items.push((
                "coverimageid".to_string(),
                "cover.jpg".to_string(),
                "image/jpeg".to_string(),
            ));
            manifest_items.push((
                "cover".to_string(),
                "cover.xhtml".to_string(),
                "application/xhtml+xml".to_string(),
            ));
        }

        // Write content files and add to manifest
        let mut content_count = 0;
        let mut spine_items: Vec<String> = Vec::new();

        if cover_path.is_some() {
            spine_items.push("cover".to_string());
        }

        for (href, _orig_id, media_type) in &content_files {
            let content = Self::read_file_from_archive(&mut self.archive, href)
                .with_context(|| format!("Failed to read content file: {}", href))?;

            zip.start_file(href.as_str(), options)
                .with_context(|| format!("Failed to add file to EPUB: {}", href))?;
            zip.write_all(content.as_bytes())
                .with_context(|| format!("Failed to write content file: {}", href))?;

            let id = format!("content{}", content_count);
            content_count += 1;
            manifest_items.push((id.clone(), href.clone(), media_type.clone()));
            spine_items.push(id);
        }

        // Write linked files (CSS, images, fonts)
        for href in &linked_files {
            if let Ok(data) = self.read_binary_file_from_archive(href) {
                zip.start_file(href.as_str(), options)
                    .with_context(|| format!("Failed to add linked file: {}", href))?;
                zip.write_all(&data)
                    .with_context(|| format!("Failed to write linked file: {}", href))?;

                let id = format!("resource{}", content_count);
                content_count += 1;
                let media_type = self.guess_media_type(href);
                manifest_items.push((id, href.clone(), media_type));
            } else {
                warn!("Skipping linked file that couldn't be read: {}", href);
            }
        }

        // Generate and write content.opf
        let content_opf = self.generate_content_opf(
            &unique_id,
            final_title,
            authors,
            &final_description,
            tags,
            languages,
            &manifest_items,
            &spine_items,
            cover_path.is_some(),
        );
        zip.start_file("content.opf", options)
            .context("Failed to create content.opf")?;
        zip.write_all(content_opf.as_bytes())
            .context("Failed to write content.opf")?;

        // Generate and write toc.ncx
        let toc_ncx = self.generate_toc_ncx(&unique_id, final_title, &toc_entries);
        zip.start_file("toc.ncx", options)
            .context("Failed to create toc.ncx")?;
        zip.write_all(toc_ncx.as_bytes())
            .context("Failed to write toc.ncx")?;

        // Write cover if provided
        if let Some(cover) = cover_path {
            let mut cover_file =
                File::open(cover).with_context(|| format!("Failed to open cover: {}", cover.display()))?;
            let mut cover_data = Vec::new();
            cover_file
                .read_to_end(&mut cover_data)
                .context("Failed to read cover file")?;

            zip.start_file("cover.jpg", options)
                .context("Failed to add cover.jpg")?;
            zip.write_all(&cover_data)
                .context("Failed to write cover.jpg")?;

            let cover_xhtml = self.generate_cover_xhtml();
            zip.start_file("cover.xhtml", options)
                .context("Failed to add cover.xhtml")?;
            zip.write_all(cover_xhtml.as_bytes())
                .context("Failed to write cover.xhtml")?;
        }

        zip.finish().context("Failed to finalize EPUB file")?;

        info!("Successfully wrote EPUB to {}", output_path.display());
        Ok(())
    }

    fn scan_for_linked_files(
        &mut self,
        content: &str,
        base_href: &str,
        linked_files: &mut HashSet<String>,
    ) -> Result<()> {
        let base_path = Self::get_path_part(base_href);

        // Scan for images: src="..." and xlink:href="..."
        let img_re = Regex::new(r#"(?:src|xlink:href)=["']([^"']+)["']"#)
            .context("Failed to compile image regex")?;
        for cap in img_re.captures_iter(content) {
            if let Some(src) = cap.get(1) {
                let src_str = src.as_str();
                if !src_str.starts_with("http://") && !src_str.starts_with("https://") {
                    let full_path = Self::normalize_path(&format!("{}{}", base_path, src_str));
                    linked_files.insert(full_path);
                }
            }
        }

        // Scan for CSS links: href="..." with type="text/css"
        let css_link_re = Regex::new(r#"<link[^>]+href=["']([^"']+\.css)["'][^>]*>"#)
            .context("Failed to compile CSS link regex")?;
        for cap in css_link_re.captures_iter(content) {
            if let Some(href) = cap.get(1) {
                let full_path = Self::normalize_path(&format!("{}{}", base_path, href.as_str()));
                linked_files.insert(full_path.clone());

                // Also scan CSS file for @import and url()
                if let Ok(css_content) = Self::read_file_from_archive(&mut self.archive, &full_path)
                {
                    self.scan_css_for_resources(&css_content, &full_path, linked_files)?;
                }
            }
        }

        Ok(())
    }

    fn scan_css_for_resources(
        &self,
        css_content: &str,
        base_href: &str,
        linked_files: &mut HashSet<String>,
    ) -> Result<()> {
        let base_path = Self::get_path_part(base_href);

        // Remove CSS comments
        let comment_re =
            Regex::new(r"/\*.*?\*/").context("Failed to compile CSS comment regex")?;
        let css_clean = comment_re.replace_all(css_content, "");

        // Scan for @import
        let import_re = Regex::new(r#"@import\s+(?:url\()?["']?([^"'\)]+)["']?\)?"#)
            .context("Failed to compile @import regex")?;
        for cap in import_re.captures_iter(&css_clean) {
            if let Some(url) = cap.get(1) {
                let full_path = Self::normalize_path(&format!("{}{}", base_path, url.as_str()));
                linked_files.insert(full_path);
            }
        }

        // Scan for url()
        let url_re =
            Regex::new(r#"url\(["']?([^"'\)]+)["']?\)"#).context("Failed to compile url() regex")?;
        for cap in url_re.captures_iter(&css_clean) {
            if let Some(url) = cap.get(1) {
                let url_str = url.as_str();
                if !url_str.starts_with("data:") {
                    let full_path = Self::normalize_path(&format!("{}{}", base_path, url_str));
                    linked_files.insert(full_path);
                }
            }
        }

        Ok(())
    }

    fn read_binary_file_from_archive(&mut self, path: &str) -> Result<Vec<u8>> {
        let mut file = self
            .archive
            .by_name(path)
            .with_context(|| format!("File not found in EPUB: {}", path))?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .with_context(|| format!("Failed to read file from EPUB: {}", path))?;
        Ok(contents)
    }

    fn guess_media_type(&self, href: &str) -> String {
        let lower = href.to_lowercase();
        if lower.ends_with(".css") {
            "text/css".to_string()
        } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
            "image/jpeg".to_string()
        } else if lower.ends_with(".png") {
            "image/png".to_string()
        } else if lower.ends_with(".gif") {
            "image/gif".to_string()
        } else if lower.ends_with(".svg") {
            "image/svg+xml".to_string()
        } else if lower.ends_with(".ttf") {
            "application/x-font-ttf".to_string()
        } else if lower.ends_with(".otf") {
            "application/vnd.ms-opentype".to_string()
        } else if lower.ends_with(".woff") {
            "application/font-woff".to_string()
        } else if lower.ends_with(".woff2") {
            "font/woff2".to_string()
        } else {
            "application/octet-stream".to_string()
        }
    }

    fn generate_container_xml(&self) -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
   <rootfiles>
      <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
   </rootfiles>
</container>
"#
        .to_string()
    }

    fn generate_content_opf(
        &self,
        unique_id: &str,
        title: &str,
        authors: &[String],
        description: &str,
        tags: &[String],
        languages: &[String],
        manifest_items: &[(String, String, String)],
        spine_items: &[String],
        has_cover: bool,
    ) -> String {
        let mut opf = String::new();

        opf.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="2.0" xmlns="http://www.idpf.org/2007/opf" unique-identifier="epubsplit-id">
   <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
"#);

        // Add identifier
        opf.push_str(&format!(
            "      <dc:identifier id=\"epubsplit-id\">{}</dc:identifier>\n",
            Self::escape_xml(unique_id)
        ));

        // Add title
        opf.push_str(&format!(
            "      <dc:title>{}</dc:title>\n",
            Self::escape_xml(title)
        ));

        // Add authors
        for author in authors {
            opf.push_str(&format!(
                "      <dc:creator opf:role=\"aut\">{}</dc:creator>\n",
                Self::escape_xml(author)
            ));
        }

        // Add contributor
        opf.push_str(
            "      <dc:contributor opf:role=\"bkp\">epubsplit-rs</dc:contributor>\n",
        );

        // Add languages
        for lang in languages {
            opf.push_str(&format!(
                "      <dc:language>{}</dc:language>\n",
                Self::escape_xml(lang)
            ));
        }

        // Add description
        opf.push_str(&format!(
            "      <dc:description>{}</dc:description>\n",
            Self::escape_xml(description)
        ));

        // Add tags/subjects
        for tag in tags {
            opf.push_str(&format!(
                "      <dc:subject>{}</dc:subject>\n",
                Self::escape_xml(tag)
            ));
        }

        // Add cover metadata if present
        if has_cover {
            opf.push_str("      <meta name=\"cover\" content=\"coverimageid\"/>\n");
        }

        opf.push_str("   </metadata>\n");

        // Add manifest
        opf.push_str("   <manifest>\n");
        for (id, href, media_type) in manifest_items {
            opf.push_str(&format!(
                "      <item id=\"{}\" href=\"{}\" media-type=\"{}\"/>\n",
                Self::escape_xml(id),
                Self::escape_xml(href),
                Self::escape_xml(media_type)
            ));
        }
        opf.push_str("   </manifest>\n");

        // Add spine
        opf.push_str("   <spine toc=\"ncx\">\n");
        for idref in spine_items {
            opf.push_str(&format!(
                "      <itemref idref=\"{}\" linear=\"yes\"/>\n",
                Self::escape_xml(idref)
            ));
        }
        opf.push_str("   </spine>\n");

        // Add guide if cover present
        if has_cover {
            opf.push_str("   <guide>\n");
            opf.push_str(
                "      <reference type=\"cover\" title=\"Cover\" href=\"cover.xhtml\"/>\n",
            );
            opf.push_str("   </guide>\n");
        }

        opf.push_str("</package>\n");

        opf
    }

    fn generate_toc_ncx(&self, unique_id: &str, title: &str, toc_entries: &[(String, String)]) -> String {
        let mut ncx = String::new();

        ncx.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx version="2005-1" xmlns="http://www.daisy.org/z3986/2005/ncx/">
   <head>
"#);

        ncx.push_str(&format!(
            "      <meta name=\"dtb:uid\" content=\"{}\"/>\n",
            Self::escape_xml(unique_id)
        ));
        ncx.push_str("      <meta name=\"dtb:depth\" content=\"1\"/>\n");
        ncx.push_str("      <meta name=\"dtb:totalPageCount\" content=\"0\"/>\n");
        ncx.push_str("      <meta name=\"dtb:maxPageNumber\" content=\"0\"/>\n");
        ncx.push_str("   </head>\n");

        ncx.push_str("   <docTitle>\n");
        ncx.push_str(&format!(
            "      <text>{}</text>\n",
            Self::escape_xml(title)
        ));
        ncx.push_str("   </docTitle>\n");

        ncx.push_str("   <navMap>\n");

        for (idx, (text, src)) in toc_entries.iter().enumerate() {
            let play_order = idx + 1;
            ncx.push_str(&format!(
                "      <navPoint id=\"navpoint-{}\" playOrder=\"{}\">\n",
                play_order, play_order
            ));
            ncx.push_str("         <navLabel>\n");
            ncx.push_str(&format!(
                "            <text>{}</text>\n",
                Self::escape_xml(text)
            ));
            ncx.push_str("         </navLabel>\n");
            ncx.push_str(&format!(
                "         <content src=\"{}\"/>\n",
                Self::escape_xml(src)
            ));
            ncx.push_str("      </navPoint>\n");
        }

        ncx.push_str("   </navMap>\n");
        ncx.push_str("</ncx>\n");

        ncx
    }

    fn generate_cover_xhtml(&self) -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en">
<head>
   <title>Cover</title>
   <style type="text/css">
      @page { padding: 0pt; margin: 0pt; }
      body { text-align: center; padding: 0pt; margin: 0pt; }
      div { margin: 0pt; padding: 0pt; }
   </style>
</head>
<body>
   <div>
      <img src="cover.jpg" alt="cover"/>
   </div>
</body>
</html>
"#
        .to_string()
    }

    fn escape_xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn get_orig_title(&self) -> &str {
        &self.orig_title
    }

    fn get_orig_authors(&self) -> &[String] {
        &self.orig_authors
    }
}

fn list_split_points(lines: &[SplitLine]) -> Result<()> {
    for (index, line) in lines.iter().enumerate() {
        println!("\nLine Number: {}", index);

        if !line.toc.is_empty() {
            println!("\ttoc: {:?}", line.toc);
        }
        if let Some((ref_type, title)) = &line.guide {
            println!("\tguide: {} ({})", ref_type, title);
        }
        if let Some(anchor) = &line.anchor {
            println!("\tanchor: {}", anchor);
        }
        println!("\tid: {}", line.id);
        println!("\thref: {}", line.href);
    }

    Ok(())
}

fn split_by_section(
    epub: &mut SplitEpub,
    lines: &[SplitLine],
    section_indices: &[usize],
    cli: &Cli,
) -> Result<()> {
    let output_filename = ensure_epub_extension(&cli.output);

    let mut splits_list: Vec<(Vec<usize>, String)> = Vec::new();
    let mut current_sections: Vec<usize> = Vec::new();
    let mut current_title: Option<String> = None;

    for &line_no in section_indices {
        if line_no >= lines.len() {
            bail!("Line number {} is out of range (max: {})", line_no, lines.len() - 1);
        }

        let line = &lines[line_no];
        let toc_list = &line.toc;

        if !current_sections.is_empty() && toc_list.is_empty() {
            // No TOC entry - include with previous section
            current_sections.push(line_no);
        } else {
            // Has TOC entry or first section - start new split
            if !current_sections.is_empty() {
                let title = current_title.clone().unwrap_or_else(|| {
                    cli.title
                        .clone()
                        .unwrap_or_else(|| format!("{} Split", epub.get_orig_title()))
                });
                splits_list.push((current_sections.clone(), title));
            }

            let title = if !toc_list.is_empty() {
                toc_list[0].clone()
            } else {
                cli.title
                    .clone()
                    .unwrap_or_else(|| format!("{} Split", epub.get_orig_title()))
            };
            println!("title: {}", title);
            current_title = Some(title);
            current_sections = vec![line_no];
        }
    }

    // Add the last section
    if !current_sections.is_empty() {
        let title = current_title.unwrap_or_else(|| {
            cli.title
                .clone()
                .unwrap_or_else(|| format!("{} Split", epub.get_orig_title()))
        });
        splits_list.push((current_sections, title));
    }

    // Write each split
    for (file_count, (section_list, title)) in splits_list.iter().enumerate() {
        let output_file = format!("{:04}-{}", file_count + 1, output_filename);
        let output_path = if let Some(ref dir) = cli.output_dir {
            dir.join(&output_file)
        } else {
            PathBuf::from(&output_file)
        };

        println!("output file: {}", output_path.display());

        let authors = if cli.author.is_empty() {
            epub.get_orig_authors().to_vec()
        } else {
            cli.author.clone()
        };

        epub.write_split_epub(
            output_path,
            section_list,
            &authors,
            Some(&title),
            cli.description.as_deref(),
            &cli.tag,
            &cli.language,
            cli.cover.as_ref(),
        )?;
    }

    Ok(())
}

fn extract_sections(epub: &mut SplitEpub, section_indices: &[usize], cli: &Cli) -> Result<()> {
    let output_filename = ensure_epub_extension(&cli.output);
    let output_path = if let Some(ref dir) = cli.output_dir {
        dir.join(&output_filename)
    } else {
        PathBuf::from(&output_filename)
    };

    println!("output file: {}", output_path.display());

    let authors = if cli.author.is_empty() {
        epub.get_orig_authors().to_vec()
    } else {
        cli.author.clone()
    };

    let title = cli
        .title
        .clone()
        .unwrap_or_else(|| format!("{} Split", epub.get_orig_title()));

    epub.write_split_epub(
        output_path,
        section_indices,
        &authors,
        Some(&title),
        cli.description.as_deref(),
        &cli.tag,
        &cli.language,
        cli.cover.as_ref(),
    )
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
    let mut epub = SplitEpub::new(cli.input.clone())
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
        split_by_section(&mut epub, &lines, &indices, &cli)?;
    } else if cli.lines.is_empty() {
        // Mode: List available split points
        list_split_points(&lines)?;
    } else {
        // Mode: Extract specific sections into one file
        extract_sections(&mut epub, &cli.lines, &cli)?;
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
