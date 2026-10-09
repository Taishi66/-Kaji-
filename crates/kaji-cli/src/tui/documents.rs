use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use std::io::{Cursor, Read, Write};
use std::path::Path;

const SOURCE_BYTES: usize = 8 * 1024 * 1024;
const XML_BYTES: usize = 512 * 1024;
const ZIP_ENTRIES: usize = 1024;
const IMAGE_BYTES: usize = 5 * 1024 * 1024;
const IMAGE_PIXELS: u64 = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct ImagePreview {
    pub pixels: image::RgbaImage,
}

pub enum Document {
    Text { text: String, truncated: bool },
    Image(ImagePreview),
}

pub fn is_text_document(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| {
            ["docx", "odt", "pptx", "xlsx", "pdf"]
                .iter()
                .any(|kind| ext.eq_ignore_ascii_case(kind))
        })
}

pub fn load(
    path: &Path,
    file: std::fs::File,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<Document>> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "docx" | "odt" | "pptx" | "xlsx" => {
            let bytes = read_bounded(file, SOURCE_BYTES)?;
            let text = office_text(&bytes, &ext)?;
            Ok(Some(Document::Text {
                text,
                truncated: false,
            }))
        }
        "pdf" => {
            let bytes = read_bounded(file, SOURCE_BYTES)?;
            let (text, truncated) =
                convert_pdf(&bytes, std::process::Command::new("pdftotext"), cancelled)?;
            Ok(Some(Document::Text { text, truncated }))
        }
        "png" | "jpg" | "jpeg" | "webp" => {
            let bytes = read_bounded(file, IMAGE_BYTES)?;
            Ok(Some(Document::Image(image_preview(&bytes)?)))
        }
        _ => Ok(None),
    }
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!(
            "preview input exceeds {}",
            super::viewer::human_size(limit as u64)
        );
    }
    Ok(bytes)
}

fn image_preview(bytes: &[u8]) -> Result<ImagePreview> {
    let reader = image::io::Reader::new(Cursor::new(bytes)).with_guessed_format()?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
    ) {
        bail!("image preview supports PNG, JPEG and WebP only");
    }
    let (width, height) = reader.into_dimensions()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > IMAGE_PIXELS {
        bail!("image preview exceeds the 4 megapixel limit");
    }
    let mut reader = image::io::Reader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    Ok(ImagePreview {
        pixels: reader.decode()?.thumbnail(160, 160).to_rgba8(),
    })
}

fn office_text(bytes: &[u8], ext: &str) -> Result<String> {
    check_zip_directory(bytes)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    if zip.len() > ZIP_ENTRIES {
        bail!("document has too many ZIP entries");
    }
    let mut budget = XML_BYTES;
    match ext {
        "docx" => xml_text(
            &read_entry(&mut zip, "word/document.xml", &mut budget)?,
            false,
        ),
        "odt" => xml_text(&read_entry(&mut zip, "content.xml", &mut budget)?, true),
        "pptx" => {
            let mut slides = numbered_entries(&zip, "ppt/slides/slide");
            let limited = slides.len() > 32;
            slides.truncate(32);
            let mut out = String::new();
            for (number, name) in slides {
                out.push_str(&format!("Slide {number}\n"));
                out.push_str(&xml_text(
                    &read_entry(&mut zip, &name, &mut budget)?,
                    false,
                )?);
                out.push('\n');
            }
            if out.is_empty() {
                bail!("no slides found");
            }
            if limited {
                out.push_str("… preview limited to the first 32 slides\n");
            }
            Ok(out)
        }
        "xlsx" => {
            let strings = if zip.file_names().any(|name| name == "xl/sharedStrings.xml") {
                shared_strings(&read_entry(&mut zip, "xl/sharedStrings.xml", &mut budget)?)?
            } else {
                Vec::new()
            };
            let mut sheets = numbered_entries(&zip, "xl/worksheets/sheet");
            let limited = sheets.len() > 16;
            sheets.truncate(16);
            let mut out = String::new();
            for (number, name) in sheets {
                out.push_str(&format!("Sheet {number}\n"));
                out.push_str(&sheet_text(
                    &read_entry(&mut zip, &name, &mut budget)?,
                    &strings,
                )?);
                out.push('\n');
            }
            if out.is_empty() {
                bail!("no worksheets found");
            }
            if limited {
                out.push_str("… preview limited to the first 16 worksheets\n");
            }
            Ok(out)
        }
        _ => unreachable!(),
    }
}

// Reject ZIP64 and excessive central directories before ZipArchive allocates
// their entry tables. Nothing is extracted to a path on disk.
fn check_zip_directory(bytes: &[u8]) -> Result<()> {
    let start = bytes.len().saturating_sub(65535 + 22);
    let eocd = (start..bytes.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            bytes[i..].starts_with(b"PK\x05\x06")
                && i + 22 + usize::from(u16::from_le_bytes([bytes[i + 20], bytes[i + 21]]))
                    == bytes.len()
        })
        .context("invalid Office ZIP directory")?;
    if eocd >= 20 && bytes[eocd - 20..].starts_with(b"PK\x06\x07") {
        bail!("ZIP64 documents exceed the preview format budget");
    }
    let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]);
    let size = u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into()?);
    let offset = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into()?);
    if usize::from(count) > ZIP_ENTRIES
        || size as usize > XML_BYTES
        || u64::from(offset) + u64::from(size) > eocd as u64
        || bytes[eocd + 4..eocd + 8] != [0, 0, 0, 0]
        || bytes[eocd + 8..eocd + 10] != bytes[eocd + 10..eocd + 12]
    {
        bail!("unsupported or oversized Office ZIP directory");
    }
    Ok(())
}

fn numbered_entries(zip: &zip::ZipArchive<Cursor<&[u8]>>, prefix: &str) -> Vec<(u32, String)> {
    let mut entries = zip
        .file_names()
        .filter_map(|name| {
            let number = name
                .strip_prefix(prefix)?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((number, name.to_owned()))
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn read_entry(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    budget: &mut usize,
) -> Result<Vec<u8>> {
    let entry = zip.by_name(name)?;
    if entry.size() > *budget as u64 {
        bail!("document XML exceeds the 512 KB preview budget");
    }
    let bytes = read_bounded(entry, *budget)?;
    *budget -= bytes.len();
    Ok(bytes)
}

fn xml_text(xml: &[u8], paragraph_text: bool) -> Result<String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut out = String::new();
    let mut text_depth = 0usize;
    loop {
        match reader.read_event()? {
            Event::Start(tag)
                if tag.local_name().as_ref() == b"t"
                    || (paragraph_text && matches!(tag.local_name().as_ref(), b"p" | b"h")) =>
            {
                text_depth += 1
            }
            Event::End(tag) if tag.local_name().as_ref() == b"t" => {
                text_depth = text_depth.saturating_sub(1)
            }
            Event::Text(text) if text_depth > 0 => out.push_str(&text.decode()?),
            Event::GeneralRef(reference) if text_depth > 0 => {
                append_reference(&mut out, &reference.decode()?)?
            }
            Event::CData(text) if text_depth > 0 => out.push_str(&text.decode()?),
            Event::Empty(tag) if matches!(tag.local_name().as_ref(), b"br" | b"line-break") => {
                out.push('\n')
            }
            Event::Empty(tag) if tag.local_name().as_ref() == b"tab" => out.push('\t'),
            Event::End(tag)
                if matches!(
                    tag.local_name().as_ref(),
                    b"p" | b"h" | b"tr" | b"table-row"
                ) =>
            {
                if paragraph_text && matches!(tag.local_name().as_ref(), b"p" | b"h") {
                    text_depth = text_depth.saturating_sub(1);
                }
                out.push('\n')
            }
            Event::End(tag) if matches!(tag.local_name().as_ref(), b"tc" | b"table-cell") => {
                out.push('\t')
            }
            Event::DocType(_) => bail!("document DTDs and external entities are not supported"),
            Event::Eof => break,
            _ => {}
        }
        if out.len() > XML_BYTES {
            bail!("document text exceeds the preview budget");
        }
    }
    Ok(out)
}

fn append_reference(out: &mut String, reference: &str) -> Result<()> {
    out.push_str(&quick_xml::escape::unescape(&format!("&{reference};"))?);
    Ok(())
}

fn shared_strings(xml: &[u8]) -> Result<Vec<String>> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut value = false;
    loop {
        match reader.read_event()? {
            Event::Start(tag) if tag.local_name().as_ref() == b"t" => value = true,
            Event::End(tag) if tag.local_name().as_ref() == b"t" => value = false,
            Event::Text(text) if value => current.push_str(&text.decode()?),
            Event::GeneralRef(reference) if value => {
                append_reference(&mut current, &reference.decode()?)?
            }
            Event::End(tag) if tag.local_name().as_ref() == b"si" => {
                if strings.len() >= 4096 {
                    bail!("too many shared spreadsheet strings");
                }
                strings.push(std::mem::take(&mut current));
            }
            Event::DocType(_) => bail!("document DTDs are not supported"),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(strings)
}

fn sheet_text(xml: &[u8], strings: &[String]) -> Result<String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut out = String::new();
    let mut cell = String::new();
    let mut address = String::new();
    let mut shared = false;
    let mut value = false;
    loop {
        match reader.read_event()? {
            Event::Start(tag) if tag.local_name().as_ref() == b"c" => {
                cell.clear();
                address = tag
                    .try_get_attribute("r")?
                    .map(|a| {
                        a.decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map(|s| s.into_owned())
                    })
                    .transpose()?
                    .unwrap_or_default();
                shared = tag
                    .try_get_attribute("t")?
                    .is_some_and(|a| a.value.as_ref() == b"s");
            }
            Event::Start(tag) if matches!(tag.local_name().as_ref(), b"v" | b"t") => value = true,
            Event::End(tag) if matches!(tag.local_name().as_ref(), b"v" | b"t") => value = false,
            Event::Text(text) if value => cell.push_str(&text.decode()?),
            Event::GeneralRef(reference) if value => {
                append_reference(&mut cell, &reference.decode()?)?
            }
            Event::End(tag) if tag.local_name().as_ref() == b"c" => {
                if !address.is_empty() {
                    out.push_str(&address);
                    out.push_str(": ");
                }
                if shared {
                    let index: usize = cell.parse().context("invalid shared string index")?;
                    out.push_str(strings.get(index).context("missing spreadsheet string")?);
                } else {
                    out.push_str(&cell);
                }
                out.push('\t');
            }
            Event::End(tag) if tag.local_name().as_ref() == b"row" => out.push('\n'),
            Event::DocType(_) => bail!("document DTDs are not supported"),
            Event::Eof => break,
            _ => {}
        }
        if out.len() > XML_BYTES {
            bail!("spreadsheet output exceeds the preview budget");
        }
    }
    Ok(out)
}

fn convert_pdf(
    bytes: &[u8],
    mut command: std::process::Command,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(String, bool)> {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("document.pdf");
    let output = dir.path().join("preview.txt");
    std::fs::File::create(&input)?.write_all(bytes)?;
    use kaji::subprocess::SubprocessExt;
    command.set_no_window();
    command
        .args(["-f", "1", "-l", "20", "-layout", "-enc", "UTF-8"])
        .arg(&input)
        .arg(&output)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        // These syscalls are async-signal-safe; no allocation or lock may run
        // in this post-fork hook. They bound CPU and temporary output size.
        unsafe {
            command.pre_exec(|| {
                for (resource, max) in [
                    (libc::RLIMIT_CPU, 3),
                    (libc::RLIMIT_FSIZE, XML_BYTES as libc::rlim_t),
                    #[cfg(target_os = "linux")]
                    (libc::RLIMIT_AS, 512 * 1024 * 1024),
                ] {
                    let limit = libc::rlimit {
                        rlim_cur: max,
                        rlim_max: max,
                    };
                    if libc::setrlimit(resource, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .context("PDF text preview needs pdftotext on PATH (Poppler)")?;
    let mut guard = Converter {
        child,
        running: true,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("PDF preview cancelled");
        }
        if let Some(status) = guard.child.try_wait()? {
            guard.running = false;
            if !status.success() {
                bail!("PDF conversion failed or exceeded its resource budget");
            }
            break;
        }
        if std::time::Instant::now() >= deadline {
            bail!("PDF conversion timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut result = Vec::new();
    super::fileio::open_regular(&output)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut result)?;
    let truncated = result.len() > 256 * 1024;
    result.truncate(256 * 1024);
    if result.iter().all(u8::is_ascii_whitespace) {
        bail!("PDF has no extractable text; scanned pages need OCR");
    }
    Ok((
        format!(
            "PDF text preview · up to 20 pages · no OCR\n\n{}",
            String::from_utf8_lossy(&result)
        ),
        truncated,
    ))
}

struct Converter {
    child: std::process::Child,
    running: bool,
}

impl Drop for Converter {
    fn drop(&mut self) {
        if self.running {
            #[cfg(unix)]
            kaji::subprocess::kill_process_group(self.child.id());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pdf_adapter_removes_inherited_environment_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("converter.sh");
        std::fs::write(&script, "for arg do output=$arg; done\nenv > \"$output\"\nprintf '\\nDocument preview\\n' >> \"$output\"\n").unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg(&script)
            .env("KAJI_TEST_SECRET", "must-not-reach-parser");
        let (text, truncated) = convert_pdf(
            b"fixture",
            command,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(text.contains("Document preview"));
        assert!(!text.contains("KAJI_TEST_SECRET") && !truncated);
        assert!(convert_pdf(
            b"fixture",
            std::process::Command::new("/usr/bin/false"),
            &std::sync::atomic::AtomicBool::new(false)
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_pdf_converter_is_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow.sh");
        let marker = dir.path().join("pid");
        std::fs::write(&script, "printf '%s' \"$$\" > \"$1\"\nsleep 20\n").unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.arg(&script).arg(&marker);
        let error = convert_pdf(
            b"fixture",
            command,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        let pid = std::fs::read_to_string(marker).unwrap();
        assert!(!std::process::Command::new("kill")
            .arg("-0")
            .arg(pid)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn xml_formatting_and_sparse_spreadsheet_addresses_are_preserved_correctly() {
        assert_eq!(xml_text(b"<w:document xmlns:w='w'>\n <w:p>\n  <w:r><w:t xml:space='preserve'> a </w:t></w:r>\n </w:p>\n</w:document>", false).unwrap(), " a \n");
        let strings = shared_strings(b"<sst>\n <si>\n <t>first</t>\n </si>\n</sst>").unwrap();
        assert_eq!(strings, ["first"]);
        let xml = b"<worksheet><row><c r='A1' t='s'><v>0</v></c><c r='D1'><v>42</v></c></row></worksheet>";
        assert_eq!(sheet_text(xml, &strings).unwrap(), "A1: first\tD1: 42\t\n");
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_a_pdf_preview_reaps_its_converter_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cancel.sh");
        let marker = dir.path().join("pid");
        std::fs::write(&script, "printf '%s' \"$$\" > \"$1\"\nsleep 20\n").unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancel.clone();
        let started = marker.clone();
        let thread = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !started.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            signal.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let mut command = std::process::Command::new("/bin/sh");
        command.arg(&script).arg(&marker);
        let error = convert_pdf(b"fixture", command, &cancel).unwrap_err();
        thread.join().unwrap();
        assert!(error.to_string().contains("cancelled"), "{error}");
        let pid = std::fs::read_to_string(marker).unwrap();
        assert!(!std::process::Command::new("kill")
            .arg("-0")
            .arg(pid)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn docx_and_odt_keep_unicode_paragraphs_and_literal_xml_entities() {
        let docx = zip_bytes(&[("word/document.xml", "<w:document xmlns:w='w'><w:p><w:r><w:t>Bonjour &amp; 世界</w:t></w:r></w:p><w:p><w:r><w:t>Suite</w:t></w:r></w:p></w:document>")]);
        assert_eq!(
            office_text(&docx, "docx").unwrap(),
            "Bonjour & 世界\nSuite\n"
        );
        let odt = zip_bytes(&[(
            "content.xml",
            "<office xmlns:text='text'><text:p>Texte</text:p></office>",
        )]);
        assert_eq!(office_text(&odt, "odt").unwrap(), "Texte\n");
    }

    #[test]
    fn hostile_xml_and_compressed_bombs_are_refused() {
        let hostile = "<!DOCTYPE x [<!ENTITY leak SYSTEM 'file:///etc/passwd'>]><x>&leak;</x>";
        assert!(xml_text(hostile.as_bytes(), false).is_err());
        let bytes = zip_bytes(&[("word/document.xml", &"x".repeat(XML_BYTES + 1))]);
        assert!(office_text(&bytes, "docx").is_err());
        let mut oversized = zip_bytes(&[("word/document.xml", "<x/>")]);
        let eocd = oversized.len() - 22;
        oversized[eocd + 10..eocd + 12].copy_from_slice(&2000u16.to_le_bytes());
        assert!(check_zip_directory(&oversized).is_err());
    }

    #[test]
    fn slides_are_numeric_and_spreadsheets_resolve_shared_strings_without_evaluating_formulas() {
        let slides = zip_bytes(&[
            ("ppt/slides/slide10.xml", "<p><t>Ten</t></p>"),
            ("ppt/slides/slide2.xml", "<p><t>Two</t></p>"),
        ]);
        assert_eq!(
            office_text(&slides, "pptx").unwrap(),
            "Slide 2\nTwo\n\nSlide 10\nTen\n\n"
        );
        let sheet = zip_bytes(&[("xl/sharedStrings.xml", "<sst><si><t>世界</t></si></sst>"), ("xl/worksheets/sheet1.xml", "<worksheet><row><c t='s'><v>0</v></c><c><f>HYPERLINK(&quot;https://example.com&quot;)</f><v>42</v></c></row></worksheet>")]);
        assert_eq!(
            office_text(&sheet, "xlsx").unwrap(),
            "Sheet 1\n世界\t42\t\n\n"
        );
    }

    #[test]
    fn images_become_small_thumbnails_and_excessive_dimensions_are_rejected() {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(400, 200)
            .write_to(&mut bytes, image::ImageOutputFormat::Png)
            .unwrap();
        let preview = image_preview(bytes.get_ref()).unwrap();
        assert_eq!(preview.pixels.dimensions(), (160, 80));
        assert!(preview.pixels.as_raw().len() <= 160 * 160 * 4);
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4097, 1024)
            .write_to(&mut bytes, image::ImageOutputFormat::Png)
            .unwrap();
        assert!(image_preview(bytes.get_ref()).is_err());
    }
}
