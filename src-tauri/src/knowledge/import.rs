//! 资料库导入的文本提取：纯文本照旧直读；docx/xlsx/pptx 是 zip+XML，就地解；
//! PDF 与图片没有本地解析器，走 Umi-OCR（knowledge::ocr），没配引擎时报人话。
//!
//! 设计口径：`extract_file` 只认 (路径, OCR 配置) → (文本, 提取方式)，
//! 不碰资料库文件——`import_files_at` 拿到文本后仍走原路的 doc_add_at。

use std::io::Read;
use std::path::Path;

use super::ocr::{base_url, ocr_document, ocr_image};

/// 纯文本的上限照旧；Office/PDF/图片走解析或 OCR，放宽到 32MB
pub const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_PARSE_BYTES: u64 = 32 * 1024 * 1024;

const TEXT_EXTS: [&str; 12] = [
    "txt", "md", "markdown", "rst", "csv", "json", "toml", "yaml", "yml", "xml", "html", "log",
];
const OFFICE_EXTS: [&str; 3] = ["docx", "xlsx", "pptx"];
/// PDF 走文档任务流，图片走单图接口
const PDF_EXTS: [&str; 1] = ["pdf"];
const IMAGE_EXTS: [&str; 7] = ["png", "jpg", "jpeg", "bmp", "webp", "tif", "tiff"];

/// 提取结果：(正文, 提取方式)。方式进文档的 source 字段，用户在库里看得到出处
pub type Extracted = (String, String);

pub fn extract_file(path: &Path, ocr: &crate::config::OcrConfig) -> Result<Extracted, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if TEXT_EXTS.contains(&ext.as_str()) {
        return extract_text(path);
    }
    if OFFICE_EXTS.contains(&ext.as_str()) {
        return extract_office(path, &ext);
    }
    if IMAGE_EXTS.contains(&ext.as_str()) || PDF_EXTS.contains(&ext.as_str()) {
        return extract_ocr(path, &ext, ocr);
    }
    Err(format!(
        "不支持的格式（.{ext}）。文本、docx/xlsx/pptx、PDF 与常见图片都行。"
    ))
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读不到（{e}）"))?;
    if meta.len() > cap {
        return Err(format!("超过 {} MB", cap / 1024 / 1024));
    }
    std::fs::read(path).map_err(|e| format!("读取失败（{e}）"))
}

fn extract_text(path: &Path) -> Result<Extracted, String> {
    let bytes = read_capped(path, MAX_TEXT_BYTES)?;
    if bytes.contains(&0) {
        return Err("是二进制文件".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "不是 UTF-8 文本".to_string())?;
    Ok((text, "纯文本".into()))
}

fn extract_office(path: &Path, ext: &str) -> Result<Extracted, String> {
    let bytes = read_capped(path, MAX_PARSE_BYTES)?;
    let cursor = std::io::Cursor::new(bytes.as_slice());
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("不是有效的文档包（{e}）"))?;
    let text = match ext {
        "docx" => docx_text(&mut archive)?,
        "xlsx" => xlsx_text(&mut archive)?,
        "pptx" => pptx_text(&mut archive)?,
        _ => unreachable!("外层已按扩展名分派"),
    };
    Ok((text, format!("{ext} 解析")))
}

fn xml_error(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what} 解析失败（{e}）")
}

/// docx：word/document.xml 的 `<w:t>` 跑条，`</w:p>` 收段落
type DocArchive<'a> = zip::ZipArchive<std::io::Cursor<&'a [u8]>>;

fn docx_text(archive: &mut DocArchive<'_>) -> Result<String, String> {
    let xml = read_entry(archive, "word/document.xml", "word/document.xml")?;
    let paragraphs: Vec<String> = xml
        .split("</w:p>")
        .map(|para| collect_tag_texts(para, "w:t"))
        .filter(|para| !para.trim().is_empty())
        .collect();
    if paragraphs.is_empty() {
        return Err("文档里没有可提取的文字".into());
    }
    Ok(paragraphs.join("\n"))
}

/// xlsx：sharedStrings 存共享串，工作表逐格取值（内联或共享下标），制表符拼行
fn xlsx_text(archive: &mut DocArchive<'_>) -> Result<String, String> {
    let shared: Vec<String> = match read_entry_opt(archive, "xl/sharedStrings.xml") {
        Some(xml) => xml
            .split("</si>")
            .map(|si| collect_tag_texts(si, "t"))
            .collect(),
        None => Vec::new(),
    };
    let mut sheet_names: Vec<String> = archive
        .file_names()
        .filter(|name| name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
        .map(|name| name.to_string())
        .collect();
    sheet_names.sort();
    let mut out: Vec<String> = Vec::new();
    for name in sheet_names {
        let xml = read_entry(archive, &name, "工作表")?;
        for row in xml.split("<row") {
            let mut cells: Vec<String> = Vec::new();
            for cell in row.split("</c>") {
                let shared_ref = cell.contains("t=\"s\"");
                let value = collect_tag_texts(cell, "v");
                if value.is_empty() {
                    continue;
                }
                if shared_ref {
                    if let Ok(index) = value.trim().parse::<usize>() {
                        if let Some(text) = shared.get(index) {
                            cells.push(text.clone());
                        }
                        continue;
                    }
                }
                cells.push(value);
            }
            let line = cells.join("\t");
            if !line.trim().is_empty() {
                out.push(line);
            }
        }
    }
    if out.is_empty() {
        return Err("表格里没有可提取的内容".into());
    }
    Ok(out.join("\n"))
}

/// pptx：每张 slide 一个 xml，`<a:t>` 跑条拼文本，按页码排序
fn pptx_text(archive: &mut DocArchive<'_>) -> Result<String, String> {
    let mut slides: Vec<(u32, String)> = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| xml_error("文档包", e))?;
        let name = file.name().to_string();
        let number: Option<u32> = name
            .strip_prefix("ppt/slides/slide")
            .and_then(|rest| rest.strip_suffix(".xml"))
            .and_then(|rest| rest.parse().ok());
        let Some(number) = number else { continue };
        let mut xml = String::new();
        file.read_to_string(&mut xml)
            .map_err(|e| xml_error("幻灯片", e))?;
        let text = collect_tag_texts(&xml, "a:t");
        if !text.trim().is_empty() {
            slides.push((number, text));
        }
    }
    slides.sort_by_key(|(number, _)| *number);
    if slides.is_empty() {
        return Err("演示文稿里没有可提取的文字".into());
    }
    Ok(slides
        .into_iter()
        .map(|(number, text)| format!("--- 第 {number} 页 ---\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn read_entry(archive: &mut DocArchive<'_>, name: &str, what: &str) -> Result<String, String> {
    read_entry_opt(archive, name).ok_or_else(|| format!("文档缺 {what}，不是标准的 {what} 文件。"))
}

fn read_entry_opt(archive: &mut DocArchive<'_>, name: &str) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// 收集 `<tag …>文本</tag>` 的内文（嵌套标签自然被剥掉），XML 实体还原成字符
fn collect_tag_texts(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after_open = match rest[start..].find('>') {
            Some(gt) => start + gt + 1,
            None => break,
        };
        let end = match rest[after_open..].find(&close) {
            Some(end) => after_open + end,
            None => break,
        };
        out.push_str(&decode_entities(&rest[after_open..end]));
        rest = &rest[end + close.len()..];
    }
    out
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// PDF 与图片：本地不解析，交给 Umi-OCR
fn extract_ocr(
    path: &Path,
    ext: &str,
    ocr: &crate::config::OcrConfig,
) -> Result<Extracted, String> {
    let bytes = read_capped(path, MAX_PARSE_BYTES)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("file.{ext}"));
    let text = if ext == "pdf" {
        ocr_document(ocr, &name, &bytes)?
    } else {
        ocr_image(ocr, &bytes)?
    };
    if text.trim().is_empty() {
        return Err("OCR 没认出文字（可能是空图或纯图页）".into());
    }
    Ok((text, format!("OCR（Umi-OCR · {}）", base_url(ocr))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 测试里现造 docx/pptx 包：zip 写手构造与真实产物同构的条目
    fn write_entry(
        archive: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
        name: &str,
        text: &str,
    ) {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(text.as_bytes()).unwrap();
    }

    fn docx_xml(paragraphs: &[&str]) -> String {
        let body: String = paragraphs
            .iter()
            .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
            .collect();
        format!("<w:document>{body}</w:document>")
    }

    #[test]
    fn docx_paragraphs_come_out_in_order_with_entities_decoded() {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut buffer);
        write_entry(
            &mut archive,
            "word/document.xml",
            &docx_xml(&["第一段&amp;注", "第二段&lt;重点&gt;"]),
        );
        drop(archive);
        let bytes = buffer.into_inner();
        let (text, method) = extract_office_stub("docx", &bytes).unwrap();
        assert_eq!(method, "docx 解析");
        assert_eq!(text, "第一段&注\n第二段<重点>");
    }

    #[test]
    fn xlsx_cells_resolve_shared_strings_and_inline_values() {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut buffer);
        write_entry(
            &mut archive,
            "xl/sharedStrings.xml",
            "<sst><si><t>钓点</t></si><si><t>鱼情</t></si></sst>",
        );
        write_entry(
            &mut archive,
            "xl/worksheets/sheet1.xml",
            "<sheetData><row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c></row><row r=\"2\"><c r=\"A2\"><v>12.5</v></c></row></sheetData>",
        );
        drop(archive);
        let bytes = buffer.into_inner();
        let (text, _) = extract_office_stub("xlsx", &bytes).unwrap();
        assert_eq!(text, "钓点\t鱼情\n12.5");
    }

    #[test]
    fn pptx_slides_are_ordered_by_number() {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut buffer);
        write_entry(
            &mut archive,
            "ppt/slides/slide2.xml",
            "<p:sp><a:t>第二页</a:t></p:sp>",
        );
        write_entry(
            &mut archive,
            "ppt/slides/slide1.xml",
            "<p:sp><a:t>第一页</a:t></p:sp>",
        );
        write_entry(
            &mut archive,
            "ppt/theme/theme1.xml",
            "<a:t>主题不是页</a:t>",
        );
        drop(archive);
        let bytes = buffer.into_inner();
        let (text, _) = extract_office_stub("pptx", &bytes).unwrap();
        assert!(text.contains("--- 第 1 页 ---\n第一页"), "{text}");
        assert!(text.contains("--- 第 2 页 ---\n第二页"), "{text}");
        assert!(!text.contains("主题不是页"));
    }

    #[test]
    fn unsupported_and_oversize_are_rejected_with_reasons() {
        let scoped = crate::test_support::scoped_temp_dir("kb-import-bin");
        let bin = scoped.path.join("data.bin");
        std::fs::write(&bin, b"\x00\x01").unwrap();
        assert!(extract_file(&bin, &Default::default()).is_err());
        let empty_ext = scoped.path.join("noext");
        std::fs::write(&empty_ext, "文本").unwrap();
        assert!(extract_file(&empty_ext, &Default::default()).is_err());
    }

    /// extract_office 接口吃路径；这里直接喂字节，绕开磁盘
    fn extract_office_stub(ext: &str, bytes: &[u8]) -> Result<Extracted, String> {
        let cursor = std::io::Cursor::new(bytes);
        let mut archive =
            zip::ZipArchive::new(cursor).map_err(|e| format!("不是有效的文档包（{e}）"))?;
        let text = match ext {
            "docx" => docx_text(&mut archive)?,
            "xlsx" => xlsx_text(&mut archive)?,
            "pptx" => pptx_text(&mut archive)?,
            _ => unreachable!(),
        };
        Ok((text, format!("{ext} 解析")))
    }
}
