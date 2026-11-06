use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use encoding_rs::{UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    Docx,
    Xlsx,
    Pdf,
    Binary,
}

impl ContentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentKind::Text => "text",
            ContentKind::Docx => "docx",
            ContentKind::Xlsx => "xlsx",
            ContentKind::Pdf => "pdf",
            ContentKind::Binary => "binary",
        }
    }

    pub fn is_textual(&self) -> bool {
        !matches!(self, ContentKind::Binary)
    }
}

pub fn kind_for_path(path: &Path, data: &[u8]) -> ContentKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "docx" | "docm" | "dotx" => return ContentKind::Docx,
        "xlsx" | "xlsm" | "xltx" | "xlsb" | "ods" => return ContentKind::Xlsx,
        "pdf" => return ContentKind::Pdf,
        _ => {}
    }
    if data.is_empty() {
        return ContentKind::Text;
    }
    if content_inspector::inspect(data).is_text() {
        ContentKind::Text
    } else {
        ContentKind::Binary
    }
}

pub fn decode_text(data: &[u8]) -> String {
    if data.starts_with(&[0xFF, 0xFE]) {
        return UTF_16LE.decode(&data[2..]).0.into_owned();
    }
    if data.starts_with(&[0xFE, 0xFF]) {
        return UTF_16BE.decode(&data[2..]).0.into_owned();
    }
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return UTF_8.decode(&data[3..]).0.into_owned();
    }
    match std::str::from_utf8(data) {
        Ok(text) => text.to_string(),
        Err(_) => WINDOWS_1252.decode(data).0.into_owned(),
    }
}

pub fn extract_text(kind: ContentKind, data: &[u8]) -> Option<String> {
    match kind {
        ContentKind::Text => Some(decode_text(data)),
        ContentKind::Docx => docx_text(data),
        ContentKind::Xlsx => xlsx_text(data),
        ContentKind::Pdf => pdf_text(data),
        ContentKind::Binary => None,
    }
}

const MAX_UNPACKED_XML: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED_RATIO: u64 = 200;
const MAX_ARCHIVE_TOTAL: u64 = 256 * 1024 * 1024;

const MAX_ENTRIES: usize = 10_000;

fn archive_is_safe(data: &[u8]) -> bool {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(data)) else {
        return false;
    };
    if archive.len() > MAX_ENTRIES {
        return false;
    }
    let mut total = 0u64;
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            return false;
        };
        let compressed = entry.compressed_size().max(1);
        let remaining = MAX_ARCHIVE_TOTAL - total;
        let limit = MAX_UNPACKED_XML.min(remaining);
        let Ok(actual) = std::io::copy(&mut entry.take(limit + 1), &mut std::io::sink()) else {
            return false;
        };
        if actual > limit {
            return false;
        }
        if actual / compressed > MAX_UNPACKED_RATIO && actual > 1024 * 1024 {
            return false;
        }
        total += actual;
    }
    true
}

fn docx_text(data: &[u8]) -> Option<String> {
    if !archive_is_safe(data) {
        return None;
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(data)).ok()?;
    let file = archive.by_name("word/document.xml").ok()?;
    if file.size() > MAX_UNPACKED_XML {
        return None;
    }
    let mut buf = Vec::new();
    file.take(MAX_UNPACKED_XML + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_UNPACKED_XML {
        return None;
    }
    let xml = String::from_utf8_lossy(&buf).into_owned();
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buf: Vec<u8> = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                match e.name().0 {
                    "w:t" => in_text = true,
                    "w:p" => {
                        if !out.ends_with('\n') && !out.is_empty() {
                            out.push('\n');
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Empty(e)) => match e.name().0 {
                "w:tab" => out.push('\t'),
                "w:br" => out.push('\n'),
                "w:cr" => out.push('\n'),
                _ => {}
            },
            Ok(quick_xml::events::Event::Text(t)) => {
                if in_text {
                    let raw = t.as_ref();
                    let decoded = quick_xml::escape::unescape(raw).unwrap_or_else(|_| raw.into());
                    out.push_str(&decoded);
                }
            }
            Ok(quick_xml::events::Event::End(e)) => {
                match e.name().0 {
                    "w:t" => in_text = false,
                    "w:p" => out.push('\n'),
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    while out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

fn xlsx_text(data: &[u8]) -> Option<String> {
    use calamine::{open_workbook_auto_from_rs, Reader};
    if data.starts_with(b"PK") && !archive_is_safe(data) {
        return None;
    }
    let mut workbook = open_workbook_auto_from_rs(Cursor::new(data)).ok()?;
    let sheets = workbook.worksheets();
    let mut out = String::new();
    for (name, range) in sheets {
        out.push_str(&format!("## {}\n", name));
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    calamine::Data::String(value) => value.clone(),
                    calamine::Data::Float(value) => format!("{}", value),
                    calamine::Data::Int(value) => value.to_string(),
                    calamine::Data::Bool(value) => value.to_string(),
                    calamine::Data::DateTime(value) => format!("{:?}", value),
                    calamine::Data::DateTimeIso(value) => value.to_string(),
                    calamine::Data::DurationIso(value) => value.to_string(),
                    calamine::Data::Error(_) => "#error".to_string(),
                    _ => String::new(),
                })
                .collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
    }
    Some(out)
}

pub const PDF_HELPER_ARG: &str = "--fht-extract-pdf";
const PDF_TIMEOUT: Duration = Duration::from_secs(20);
const PDF_MAX_OUTPUT: u64 = 16 * 1024 * 1024;

static PDF_HELPER: AtomicBool = AtomicBool::new(false);
static PDF_FAILED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

pub fn enable_pdf_helper() {
    PDF_HELPER.store(true, Ordering::SeqCst);
}

pub fn run_pdf_helper() -> i32 {
    let mut data = Vec::new();
    if std::io::stdin().take(512 * 1024 * 1024).read_to_end(&mut data).is_err() {
        return 2;
    }
    match pdf_extract::extract_text_from_mem(&data) {
        Ok(text) => {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            if out.write_all(text.as_bytes()).is_err() {
                return 2;
            }
            0
        }
        Err(_) => 1,
    }
}

fn pdf_text_in_helper(data: &[u8]) -> Option<String> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().ok()?;
    let mut command = Command::new(exe);
    command
        .arg(PDF_HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let input = data.to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(PDF_MAX_OUTPUT).read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + PDF_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let _ = writer.join();
    let output = reader.join().ok()?;
    match status {
        Some(status) if status.success() => Some(String::from_utf8_lossy(&output).into_owned()),
        _ => None,
    }
}

fn pdf_text(data: &[u8]) -> Option<String> {
    let hash = blake3::hash(data).to_hex().to_string();
    let failed = PDF_FAILED.get_or_init(|| Mutex::new(HashSet::new()));
    if failed.lock().unwrap_or_else(|e| e.into_inner()).contains(&hash) {
        return None;
    }
    let text = if PDF_HELPER.load(Ordering::SeqCst) {
        pdf_text_in_helper(data)
    } else {
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(data).ok())
            .ok()
            .flatten()
    };
    if text.is_none() {
        failed.lock().unwrap_or_else(|e| e.into_inner()).insert(hash);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_utf8() {
        assert_eq!(decode_text("hello wörld".as_bytes()), "hello wörld");
    }

    #[test]
    fn decodes_utf8_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"text");
        assert_eq!(decode_text(&data), "text");
    }

    #[test]
    fn decodes_utf16_le() {
        let mut data = vec![0xFF, 0xFE];
        for unit in "hi".encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_text(&data), "hi");
    }

    #[test]
    fn detects_binary_content() {
        let binary = vec![0u8, 159, 146, 150, 0u8, 1, 2, 3];
        assert_eq!(kind_for_path(Path::new("file.bin"), &binary), ContentKind::Binary);
    }

    #[test]
    fn detects_text_content() {
        assert_eq!(
            kind_for_path(Path::new("file.txt"), b"plain text"),
            ContentKind::Text
        );
    }

    #[test]
    fn extracts_text_from_docx() {
        use std::io::Write;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            writer
                .start_file("word/document.xml", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer
                .write_all(
                    br#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:p><w:r><w:t>World</w:t></w:r></w:p></w:body></w:document>"#,
                )
                .unwrap();
            writer.finish().unwrap();
        }
        let bytes = buffer.into_inner();
        let text = docx_text(&bytes).expect("docx text");
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }

    #[test]
    fn archive_check_counts_real_decompressed_bytes() {
        use std::io::Write;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .large_file(true);
            writer.start_file("xl/worksheets/sheet1.xml", options).unwrap();
            let chunk = vec![b'a'; 1024 * 1024];
            for _ in 0..(MAX_UNPACKED_XML / chunk.len() as u64 + 2) {
                writer.write_all(&chunk).unwrap();
            }
            writer.finish().unwrap();
        }
        let bytes = buffer.into_inner();
        assert!(bytes.len() < 2 * 1024 * 1024, "the bomb itself is small");
        assert!(!archive_is_safe(&bytes));
        assert!(xlsx_text(&bytes).is_none());
    }

    #[test]
    fn extracts_text_from_xlsx() {
        use std::io::Write;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            writer
                .start_file(
                    "[Content_Types].xml",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
                )
                .unwrap();
            writer
                .start_file(
                    "_rels/.rels",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
                )
                .unwrap();
            writer
                .start_file(
                    "xl/workbook.xml",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
                )
                .unwrap();
            writer
                .start_file(
                    "xl/_rels/workbook.xml.rels",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
                )
                .unwrap();
            writer
                .start_file(
                    "xl/worksheets/sheet1.xml",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Cell value</t></is></c></row></sheetData></worksheet>"#,
                )
                .unwrap();
            writer.finish().unwrap();
        }
        let bytes = buffer.into_inner();
        let text = xlsx_text(&bytes).expect("xlsx text");
        assert!(text.contains("Cell value"), "unexpected text: {text}");
    }
}

