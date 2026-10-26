//! Word（.docx）导入：把 OOXML 读成受控的 HTML 片段。
//!
//! 为什么要自己写这一层：
//!
//! - docx 就是一个 zip，正文在 `word/document.xml`。用 `zip` 解包 + `quick-xml`
//!   流式读事件，足以还原**段落、标题、加粗/斜体/下划线、列表、表格、内嵌图片**——
//!   也就是团队写文档真正在乎的那几样。
//! - 不引入 `docx-rs` 这类"文档模型"库：它们会把整个文档建成树、再让我们遍历，
//!   而我们只需要一次线性的 HTML 生成。少一层抽象，少一处版本漂移。
//!
//! 输出的 HTML **仍然要过 `richtext::sanitize`**——这一层只负责"翻译结构"，
//! 不负责"信任"。图片先以 `{{img:N}}` 占位，由调用方落盘换成真实 URL
//! （解析器因此不碰数据库、不碰配置，可以纯函数测试）。

use std::collections::HashMap;
use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::AppError;
use crate::shared::richtext;

/// 单个 docx 上限。团队文档常见几十页带图也就 1–3 MB，8 MiB 留足余量，
/// 同时把"上传一个 200 MB 的 pptx 改名成 docx"挡在解析之前。
pub const MAX_DOCX_BYTES: usize = 8 * 1024 * 1024;
/// `word/document.xml` 解压后的上限（防 zip 炸弹：压缩比可以做到 1000:1）。
const MAX_DOCUMENT_XML: u64 = 32 * 1024 * 1024;
/// 单个内嵌图片解压后的上限。
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// 内嵌图片张数的上限：再多的话正文已经不是"文档"了。
const MAX_IMAGES: usize = 32;
/// 纯文本与 HTML 的输出上限。
const MAX_TEXT_CHARS: usize = 60_000;
const MAX_HTML_CHARS: usize = 200_000;

/// 从 docx 里抽出来的一张图（字节 + 扩展名）。
#[derive(Debug)]
pub struct DocxImage {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
    pub mime: &'static str,
}

/// 解析结果。
#[derive(Debug)]
pub struct ParsedDocx {
    /// 文档标题：优先 `docProps/core.xml` 的 `dc:title`，否则正文第一段，再否则空串。
    pub title: String,
    /// 净化后的 HTML，图片是 `{{img:N}}` 占位。
    pub html: String,
    /// 纯文本（段落之间换行）。搜索、摘要、审计都读它。
    pub text: String,
    /// 内嵌图片，顺序与占位符里的 N 对应。
    pub images: Vec<DocxImage>,
    /// 降级说明（例如"文本框内容未导入"），展示给上传者看，不写进正文。
    pub notes: Vec<&'static str>,
}

/// 解析一份 docx。任何结构问题都退化成"能读多少读多少"，只有
/// "不是 docx / 正文缺失 / 太大" 才报错。
pub fn parse_docx(bytes: &[u8]) -> Result<ParsedDocx, AppError> {
    if bytes.is_empty() {
        return Err(AppError::unprocessable("这个文件是空的"));
    }
    if bytes.len() > MAX_DOCX_BYTES {
        return Err(AppError::unprocessable(format!(
            "Word 文档不能超过 {} MB",
            MAX_DOCX_BYTES / 1024 / 1024
        )));
    }
    if !looks_like_zip(bytes) {
        return Err(AppError::unprocessable("这不是一个 Word 文档（.docx）"));
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| AppError::unprocessable("这个 Word 文档打不开，可能已经损坏"))?;

    let mut notes: Vec<&'static str> = Vec::new();

    let document_xml =
        read_part(&mut archive, "word/document.xml", MAX_DOCUMENT_XML)?.ok_or_else(|| {
            AppError::unprocessable("这个文件里没有正文（不是 .docx，或者只是改了扩展名）")
        })?;

    let rels = read_part(
        &mut archive,
        "word/_rels/document.xml.rels",
        4 * 1024 * 1024,
    )?
    .map(|xml| parse_rels(&xml))
    .unwrap_or_default();

    let numbering = read_part(&mut archive, "word/numbering.xml", 4 * 1024 * 1024)?
        .map(|xml| parse_numbering(&xml))
        .unwrap_or_default();

    let core_title = read_part(&mut archive, "docProps/core.xml", 1024 * 1024)?
        .and_then(|xml| parse_core_title(&xml));

    let images = collect_images(&mut archive, &rels, &mut notes)?;

    // rel id -> 图片下标
    let mut image_by_rel: HashMap<String, usize> = HashMap::new();
    for (index, image) in images.iter().enumerate() {
        if let Some(rel) = image.rel_id.as_deref() {
            image_by_rel.insert(rel.to_owned(), index);
        }
    }

    let body = parse_body(&document_xml, &numbering, &image_by_rel, &mut notes)?;

    let html = richtext::sanitize(&body.html).into_string();
    let html = clamp_html(&html);

    let title = core_title
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            body.paragraphs
                .iter()
                .map(|p| p.trim())
                .find(|p| !p.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default();

    Ok(ParsedDocx {
        title: clamp_chars(&title, 120).to_owned(),
        html,
        text: clamp_chars(&body.text, MAX_TEXT_CHARS).to_owned(),
        images: images.into_iter().map(|i| i.image).collect(),
        notes,
    })
}

/// 内部用的图片条目：多带一个 rel id，方便正文里把 `r:embed` 映射成下标。
struct PendingImage {
    rel_id: Option<String>,
    image: DocxImage,
}

fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

fn read_part(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, AppError> {
    let file = match archive.by_name(name) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    if file.size() > limit {
        return Ok(None);
    }
    let mut buffer = Vec::with_capacity(file.size().min(1024 * 1024) as usize);
    // 用 take 再兜一层：`size()` 来自中央目录，理论上可信，但多一层不必付代价。
    file.take(limit)
        .read_to_end(&mut buffer)
        .map_err(|err| AppError::internal(anyhow::anyhow!("解压 {name} 失败：{err}")))?;
    Ok(Some(buffer))
}

fn collect_images(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    rels: &HashMap<String, String>,
    notes: &mut Vec<&'static str>,
) -> Result<Vec<PendingImage>, AppError> {
    // 先收集名字，再逐个读：`by_name` 要 &mut archive，不能在遍历名字时同时持有。
    let mut names: Vec<String> = Vec::new();
    for index in 0..archive.len() {
        let file = match archive.by_index_raw(index) {
            Ok(file) => file,
            Err(_) => continue,
        };
        // zip 9 的 name() 返回 Result（文件名不保证是合法 UTF-8），退化用 name_raw。
        let name = match file.name() {
            Ok(name) => name.into_owned(),
            Err(_) => String::from_utf8_lossy(file.name_raw()).into_owned(),
        };
        if name.starts_with("word/media/") {
            names.push(name);
        }
    }

    // rel 里 target 可能是 `media/image1.png`，也可能是 `../media/...`，统一按文件名索引。
    let mut by_file: HashMap<String, String> = HashMap::new();
    for (rel_id, target) in rels {
        let file = target.rsplit('/').next().unwrap_or(target);
        by_file.insert(file.to_owned(), rel_id.clone());
    }

    let mut out: Vec<PendingImage> = Vec::new();
    for name in names {
        if out.len() >= MAX_IMAGES {
            notes.push("文档里的图片超过 32 张，多出的没有导入");
            break;
        }
        let file_part = name.rsplit('/').next().unwrap_or(&name).to_owned();
        if !has_image_ext(&file_part) {
            continue;
        }
        let Some(ext) = image_ext(&file_part) else {
            continue;
        };
        let file = match archive.by_name(&name) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if file.size() > MAX_IMAGE_BYTES {
            notes.push("有一张图片超过 8 MB，没有导入");
            continue;
        }
        let mut bytes = Vec::new();
        if file.take(MAX_IMAGE_BYTES).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if bytes.len() < 32 || sniff_mismatch(&bytes, ext) {
            // 扩展名与内容不符：宁可丢一张图，也不要把非图片塞进 /uploads。
            notes.push("有一张图片的格式无法识别，没有导入");
            continue;
        }
        out.push(PendingImage {
            rel_id: by_file.get(&file_part).cloned(),
            image: DocxImage {
                bytes,
                ext,
                mime: image_mime(ext),
            },
        });
    }
    Ok(out)
}

fn has_image_ext(name: &str) -> bool {
    image_ext(name).is_some()
}

fn image_ext(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".png") {
        Some("png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("jpg")
    } else if lower.ends_with(".gif") {
        Some("gif")
    } else if lower.ends_with(".webp") {
        Some("webp")
    } else {
        None
    }
}

fn image_mime(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

/// 只做"扩展名与 magic bytes 是否自相矛盾"的判断，详细校验由 media 服务负责。
fn sniff_mismatch(bytes: &[u8], ext: &str) -> bool {
    match ext {
        "png" => !bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "gif" => !bytes.starts_with(b"GIF8"),
        "jpg" => !bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "webp" => {
            !(bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..].starts_with(b"WEBP"))
        }
        _ => true,
    }
}

fn parse_rels(xml: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let masked = mask_entities(xml);
    let mut reader = Reader::from_str(&masked);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                if local_name(event.name().as_ref()) != "Relationship" {
                    buffer.clear();
                    continue;
                }
                let mut id = None;
                let mut target = None;
                let mut is_image = false;
                for attr in event.attributes().flatten() {
                    let key = attr.key.as_ref();
                    let value = attr_text(&attr);
                    match local_name(key) {
                        "Id" => id = Some(value),
                        "Target" => target = Some(value),
                        "Type" => is_image = value.contains("/image"),
                        _ => {}
                    }
                }
                if is_image && let (Some(id), Some(target)) = (id, target) {
                    out.insert(id, target);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    out
}

/// numbering.xml → `numId -> 是否有序`。
///
/// 只做一级判断：Word 的一个 numId 对应一个 abstractNum，第一级（ilvl=0）若是
/// `numFmt val="bullet"` 就是无序，其余（decimal、lowerLetter…）一律当有序。
/// 嵌套层级不还原——正文里的多级列表会被拍平，这是有意的取舍，写进 §41 的债务。
fn parse_numbering(xml: &[u8]) -> HashMap<String, bool> {
    let mut num_to_abstract: HashMap<String, String> = HashMap::new();
    let mut abstract_ordered: HashMap<String, bool> = HashMap::new();

    let masked = mask_entities(xml);
    let mut reader = Reader::from_str(&masked);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut current_abstract: Option<String> = None;
    let mut current_num: Option<String> = None;
    let mut first_level: Option<bool> = None;
    let mut in_first_level = false;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => {
                match local_name(event.name().as_ref()) {
                    "abstractNum" => {
                        current_abstract = attr_value(&event, "abstractNumId");
                        first_level = None;
                    }
                    "lvl" => {
                        // 只看 ilvl=0 的那一级。
                        in_first_level = attr_value(&event, "ilvl").as_deref() == Some("0");
                    }
                    // Word 写的是 <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>：
                    // numId 在外层开标签上，abstractNumId 是它唯一的子元素。早先只在
                    // num 元素自己的属性里找 abstractNumId，永远找不到，于是所有列表
                    // 都退回无序。
                    "num" => {
                        current_num = attr_value(&event, "numId");
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(event)) => match local_name(event.name().as_ref()) {
                "abstractNumId" => {
                    if let (Some(num_id), Some(abs)) =
                        (current_num.clone(), attr_value(&event, "val"))
                    {
                        num_to_abstract.insert(num_id, abs);
                    }
                }
                "numFmt" if in_first_level => {
                    let fmt = attr_value(&event, "val").unwrap_or_default();
                    first_level = Some(fmt != "bullet");
                }
                _ => {}
            },
            Ok(Event::End(event)) => match local_name(event.name().as_ref()) {
                "num" => current_num = None,
                "lvl" => in_first_level = false,
                "abstractNum" => {
                    if let Some(abstract_id) = current_abstract.take() {
                        abstract_ordered.insert(abstract_id, first_level.unwrap_or(false));
                    }
                    first_level = None;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    let mut out = HashMap::new();
    for (num_id, abstract_id) in num_to_abstract {
        out.insert(
            num_id,
            *abstract_ordered.get(&abstract_id).unwrap_or(&false),
        );
    }
    out
}

fn parse_core_title(xml: &[u8]) -> Option<String> {
    let masked = mask_entities(xml);
    let mut reader = Reader::from_str(&masked);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut in_title = false;
    let mut out = String::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => {
                if local_name(event.name().as_ref()) == "title" {
                    in_title = true;
                }
            }
            Ok(Event::Text(event)) if in_title => {
                out.push_str(&unmask(&event.into_inner()));
            }
            Ok(Event::End(event)) => {
                if local_name(event.name().as_ref()) == "title" {
                    in_title = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    Some(out)
}

/// 把 XML 实体换成私用区字符再交给解析器。
///
/// 为什么要这道工序：quick-xml 在分词阶段就会把 `&lt;` 解成 `<`，而解出来的 `<`
/// 会被它当作标签起点继续分词——实测 `<w:t>&lt;script&gt;alert(1)&lt;/script&gt;</w:t>`
/// 被切成三个文本事件（"script" / "alert(1)" / "/script"），尖括号本身丢了。
/// 这既是字符丢失（正文里写 `a < b` 会变成 `a  b`），也让人误以为"净化器兜住了"。
///
/// 做法：解析前把五个实体替换成私用区字符（U+E000..），解析完在取文本/属性时换回来，
/// 再由 escape_text 统一转义。占位字符在真实 Word 文档里出现概率极低，且即使出现
/// 也只是"变成对应字符"，不会造成注入。
fn mask_entities(xml: &[u8]) -> String {
    let text = String::from_utf8_lossy(xml);
    text.replace("&lt;", "\u{E001}")
        .replace("&gt;", "\u{E002}")
        .replace("&quot;", "\u{E003}")
        .replace("&apos;", "\u{E004}")
        .replace("&nbsp;", "\u{E005}")
        // &amp; 必须最后替换：否则 &amp;lt; 会先变成 &lt; 再被当成尖括号。
        .replace("&amp;", "\u{E000}")
}

/// 把 mask_entities 换进去的占位字符还原成真实字符。单趟扫描，不会二次解码。
fn unmask(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        out.push(match ch {
            '\u{E000}' => '&',
            '\u{E001}' => '<',
            '\u{E002}' => '>',
            '\u{E003}' => '"',
            '\u{E004}' => '\'',
            '\u{E005}' => ' ',
            other => other,
        });
    }
    out
}

fn attr_value(event: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    for attr in event.attributes().flatten() {
        if local_name(attr.key.as_ref()) == name {
            return Some(attr_text(&attr));
        }
    }
    None
}

/// 属性值取原始字节再还原占位字符。
///
/// 刻意不走 quick-xml 的实体解析：实体在 `mask_entities` 阶段已经换成占位字符，
/// 这里再解一次反而会把实体解成裸的尖括号（正文那边踩的是同一个坑）。
fn attr_text(attr: &quick_xml::events::attributes::Attribute<'_>) -> String {
    // quick-xml 0.42 的 Attribute::value 已经是 Cow<str>（早先是字节），不需要再走
    // from_utf8_lossy；直接用 &str 还原占位字符即可。
    let raw: &str = &attr.value;
    unmask(raw)
}

/// 去掉命名空间前缀：`w:p` → `p`，`a:blip` → `blip`。
///
/// quick-xml 0.42 的 `QName`/`LocalName` 实现的是 `AsRef<str>`，所以这里是 `&str` 而不是 `&[u8]`。
fn local_name(raw: &str) -> &str {
    match raw.find(':') {
        Some(index) => &raw[index + 1..],
        None => raw,
    }
}

struct Body {
    html: String,
    text: String,
    paragraphs: Vec<String>,
}

/// 正文状态机。
struct Walker<'a> {
    numbering: &'a HashMap<String, bool>,
    images: &'a HashMap<String, usize>,
    notes: &'a mut Vec<&'static str>,

    html: String,
    text: String,
    paragraphs: Vec<String>,

    // 当前段落
    para_style: Option<String>,
    para_num: Option<String>,
    para_runs: String,
    para_text: String,

    // 当前 run 的格式
    in_run: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    run_tags: Vec<&'static str>,

    // 游标
    in_ppr: bool,
    in_rpr: bool,
    in_text: bool,
    in_instr: bool,
    drawing_depth: usize,

    // 列表与表格
    open_list: Option<bool>, // Some(true)=ol, Some(false)=ul
    table_depth: usize,
    cell_depth: usize,
}

fn parse_body(
    xml: &[u8],
    numbering: &HashMap<String, bool>,
    images: &HashMap<String, usize>,
    notes: &mut Vec<&'static str>,
) -> Result<Body, AppError> {
    let mut walker = Walker {
        numbering,
        images,
        notes,
        html: String::new(),
        text: String::new(),
        paragraphs: Vec::new(),
        para_style: None,
        para_num: None,
        para_runs: String::new(),
        para_text: String::new(),
        in_run: false,
        bold: false,
        italic: false,
        underline: false,
        run_tags: Vec::new(),
        in_ppr: false,
        in_rpr: false,
        in_text: false,
        in_instr: false,
        drawing_depth: 0,
        open_list: None,
        table_depth: 0,
        cell_depth: 0,
    };

    let masked = mask_entities(xml);
    let mut reader = Reader::from_str(&masked);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => walker.start(local_name(event.name().as_ref()), &event),
            Ok(Event::Empty(event)) => walker.empty(local_name(event.name().as_ref()), &event),
            Ok(Event::Text(event)) => {
                if walker.in_text && !walker.in_instr && walker.drawing_depth == 0 {
                    // 实体已经在 mask_entities 里换成了占位字符，这里只需还原，
                    // 剩下的转义交给 push_text（它负责 escape_text）。
                    walker.push_text(&unmask(&event.into_inner()));
                }
            }
            Ok(Event::End(event)) => walker.end(local_name(event.name().as_ref())),
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(AppError::unprocessable(format!(
                    "这个 Word 文档的正文解析失败：{err}"
                )));
            }
            _ => {}
        }
        buffer.clear();
    }

    walker.finish();
    Ok(Body {
        html: walker.html,
        text: clamp_chars(&walker.text, MAX_TEXT_CHARS).to_owned(),
        paragraphs: walker.paragraphs,
    })
}

impl<'a> Walker<'a> {
    fn start(&mut self, name: &str, event: &quick_xml::events::BytesStart<'_>) {
        match name {
            "p" => {
                if self.table_depth == 0 {
                    // 段落开始前先关掉还没收尾的列表项由 flush 负责。
                }
                self.para_style = None;
                self.para_num = None;
                self.para_runs.clear();
                self.para_text.clear();
            }
            "pPr" => self.in_ppr = true,
            "rPr" => self.in_rpr = true,
            "numPr" => {
                if self.in_ppr {
                    self.para_num = Some(String::from("1"));
                }
            }
            "r" => {
                self.in_run = true;
                self.bold = false;
                self.italic = false;
                self.underline = false;
                self.run_tags.clear();
            }
            "t" => self.in_text = true,
            "instrText" => self.in_instr = true,
            "drawing" => self.drawing_depth += 1,
            "tbl" => {
                self.close_list();
                self.html.push_str("<table><tbody>");
                self.table_depth += 1;
            }
            "tr" => {
                if self.table_depth > 0 {
                    self.html.push_str("<tr>");
                }
            }
            "tc" => {
                if self.table_depth > 0 {
                    self.html.push_str("<td>");
                    self.cell_depth += 1;
                }
            }
            "pStyle" => {
                if self.in_ppr {
                    self.para_style = attr_value(event, "val");
                }
            }
            "numId" if self.in_ppr => {
                self.para_num = attr_value(event, "val");
            }
            _ => {}
        }
    }

    fn empty(&mut self, name: &str, event: &quick_xml::events::BytesStart<'_>) {
        match name {
            "b" if self.in_rpr => self.bold = flag(event),
            "i" if self.in_rpr => self.italic = flag(event),
            "u" if self.in_rpr => self.underline = flag(event),
            // Word 把这些属性写成自闭合的 `<w:pStyle w:val="…"/>`、`<w:numId w:val="1"/>`，
            // 都在 pPr 里面——只在 Start 分支处理会一个都读不到（标题全变普通段落）。
            "pStyle" if self.in_ppr => self.para_style = attr_value(event, "val"),
            "numId" if self.in_ppr => self.para_num = attr_value(event, "val"),
            "br" => {
                if self.drawing_depth == 0 {
                    self.para_runs.push_str("<br>");
                }
            }
            "tab" => {
                if self.drawing_depth == 0 {
                    self.para_runs.push(' ');
                }
            }
            "blip" => {
                if let Some(rel) = attr_value(event, "embed") {
                    if let Some(index) = self.images.get(&rel) {
                        self.para_runs.push_str(&format!("{{{{img:{index}}}}}"));
                    } else {
                        self.note("文档里有图片引用了不存在的资源，已跳过");
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "p" => self.flush_paragraph(),
            "r" => {
                self.close_run();
                self.in_run = false;
            }
            "t" => self.in_text = false,
            "instrText" => self.in_instr = false,
            "rPr" => self.in_rpr = false,
            "pPr" => self.in_ppr = false,
            "drawing" => self.drawing_depth = self.drawing_depth.saturating_sub(1),
            "tc" => {
                if self.cell_depth > 0 {
                    self.html.push_str("</td>");
                    self.cell_depth -= 1;
                }
            }
            "tr" => {
                if self.table_depth > 0 {
                    self.html.push_str("</tr>");
                }
            }
            "tbl" if self.table_depth > 0 => {
                self.html.push_str("</tbody></table>");
                self.table_depth -= 1;
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        let trimmed = text.replace('\u{00A0}', " ");
        if trimmed.is_empty() {
            return;
        }
        self.open_run();
        self.para_runs.push_str(&escape_text(&trimmed));
        self.para_text.push_str(&trimmed);
    }

    fn open_run(&mut self) {
        if !self.in_run {
            return;
        }
        if self.bold && !self.run_tags.contains(&"b") {
            self.para_runs.push_str("<b>");
            self.run_tags.push("b");
        }
        if self.italic && !self.run_tags.contains(&"i") {
            self.para_runs.push_str("<i>");
            self.run_tags.push("i");
        }
        if self.underline && !self.run_tags.contains(&"u") {
            self.para_runs.push_str("<u>");
            self.run_tags.push("u");
        }
    }

    fn close_run(&mut self) {
        while let Some(tag) = self.run_tags.pop() {
            self.para_runs.push_str("</");
            self.para_runs.push_str(tag);
            self.para_runs.push('>');
        }
    }

    fn flush_paragraph(&mut self) {
        self.close_run();
        let content = std::mem::take(&mut self.para_runs);
        let plain = std::mem::take(&mut self.para_text);
        let style = self.para_style.take();
        let num = self.para_num.take();

        // 空段落不落 HTML：Word 用空段落排版，照搬会在网页里留一大片空白。
        if content.trim().is_empty() {
            return;
        }

        if self.cell_depth > 0 {
            // 表格里不用 li/h2，避免把单元格变成块级排版灾难。
            self.html.push_str("<p>");
            self.html.push_str(&content);
            self.html.push_str("</p>");
        } else if num.is_some() {
            let ordered = num
                .as_deref()
                .and_then(|id| self.numbering.get(id).copied())
                .unwrap_or(false);
            self.open_list(ordered);
            self.html.push_str("<li>");
            self.html.push_str(&content);
            self.html.push_str("</li>");
        } else {
            self.close_list();
            let tag = heading_tag(style.as_deref());
            self.html.push('<');
            self.html.push_str(tag);
            self.html.push('>');
            self.html.push_str(&content);
            self.html.push_str("</");
            self.html.push_str(tag);
            self.html.push('>');
        }

        if !plain.trim().is_empty() {
            self.text.push_str(plain.trim());
            self.text.push('\n');
            self.paragraphs.push(plain);
        }
    }

    fn open_list(&mut self, ordered: bool) {
        if self.open_list == Some(ordered) {
            return;
        }
        self.close_list();
        self.html.push_str(if ordered { "<ol>" } else { "<ul>" });
        self.open_list = Some(ordered);
    }

    fn close_list(&mut self) {
        if let Some(ordered) = self.open_list.take() {
            self.html.push_str(if ordered { "</ol>" } else { "</ul>" });
        }
    }

    fn note(&mut self, note: &'static str) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    fn finish(&mut self) {
        self.flush_paragraph();
        self.close_list();
        while self.table_depth > 0 {
            self.html.push_str("</tbody></table>");
            self.table_depth -= 1;
        }
    }
}

/// `w:b`/`w:i`/`w:u` 不带 `w:val` 时为真；带 `0`/`false`/`off` 时为假。
fn flag(event: &quick_xml::events::BytesStart<'_>) -> bool {
    match attr_value(event, "val") {
        None => true,
        Some(value) => !matches!(value.as_str(), "0" | "false" | "off"),
    }
}

/// Word 的标题样式名：`Heading1`/`heading 2`/`标题 3`（本地化版本会写中文）。
fn heading_tag(style: Option<&str>) -> &'static str {
    let Some(style) = style else {
        return "p";
    };
    let lower = style.to_ascii_lowercase();
    let level = if lower.starts_with("heading") || style.starts_with("标题") {
        style
            .chars()
            .find(|c| c.is_ascii_digit() || ('１'..='９').contains(c))
            .and_then(|c| c.to_digit(10))
            .unwrap_or(1)
    } else if lower.starts_with("title") {
        1
    } else {
        return "p";
    };
    if level <= 2 { "h2" } else { "h3" }
}

fn escape_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn clamp_chars(raw: &str, max: usize) -> &str {
    if raw.chars().count() <= max {
        return raw;
    }
    match raw.char_indices().nth(max) {
        Some((index, _)) => &raw[..index],
        None => raw,
    }
}

/// HTML 太长时整体截断，并补一句说明——半截标签比"没有内容"更糟，
/// 所以截断点回退到最后一个 `>`。
fn clamp_html(raw: &str) -> String {
    if raw.chars().count() <= MAX_HTML_CHARS {
        return raw.to_owned();
    }
    let cut = raw
        .char_indices()
        .nth(MAX_HTML_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(raw.len());
    let head = &raw[..cut];
    let safe = match head.rfind('>') {
        Some(index) => &head[..=index],
        None => "",
    };
    format!("{safe}<p>（文档过长，后面的内容没有导入）</p>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// 造一份最小 docx：把给定的 `word/document.xml` 与可选部件压进 zip。
    fn build_docx(document: &str, numbering: Option<&str>, media: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let mut put = |name: &str, data: &[u8]| {
                writer.start_file(name, options).unwrap();
                writer.write_all(data).unwrap();
            };
            put("word/document.xml", document.as_bytes());
            put(
                "word/_rels/document.xml.rels",
                br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/></Relationships>"#,
            );
            if let Some(numbering) = numbering {
                put("word/numbering.xml", numbering.as_bytes());
            }
            for (name, bytes) in media {
                put(name, bytes);
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    fn wrap(body: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="w" xmlns:a="a" xmlns:r="r"><w:body>{body}</w:body></w:document>"#
        )
    }

    #[test]
    fn paragraphs_and_runs_become_paragraphs_with_marks() {
        let doc = wrap(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>标题</w:t></w:r></w:p>
               <w:p><w:r><w:t>普通</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>加粗</w:t></w:r><w:r><w:rPr><w:i/><w:u/></w:rPr><w:t>斜</w:t></w:r></w:p>"#,
        );
        let parsed = parse_docx(&build_docx(&doc, None, &[])).unwrap();
        assert_eq!(
            parsed.html,
            "<h2>标题</h2><p>普通<b>加粗</b><i><u>斜</u></i></p>"
        );
        assert_eq!(parsed.title, "标题");
        assert!(parsed.text.contains("普通加粗斜"));
    }

    #[test]
    fn bullet_and_numbered_lists_are_separated() {
        let numbering = r#"<?xml version="1.0"?><w:numbering xmlns:w="w">
            <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum>
            <w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
        </w:numbering>"#;
        let doc = wrap(
            r#"<w:p><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>甲</w:t></w:r></w:p>
               <w:p><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>乙</w:t></w:r></w:p>
               <w:p><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>丙</w:t></w:r></w:p>"#,
        );
        let parsed = parse_docx(&build_docx(&doc, Some(numbering), &[])).unwrap();
        assert_eq!(
            parsed.html,
            "<ul><li>甲</li><li>乙</li></ul><ol><li>丙</li></ol>"
        );
    }

    #[test]
    fn tables_keep_cells_in_rows() {
        let doc = wrap(
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>姓名</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>角色</w:t></w:r></w:p></w:tc></w:tr>
               <w:tr><w:tc><w:p><w:r><w:t>阿岚</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>剪辑</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        );
        let parsed = parse_docx(&build_docx(&doc, None, &[])).unwrap();
        assert_eq!(
            parsed.html,
            "<table><tbody><tr><td><p>姓名</p></td><td><p>角色</p></td></tr><tr><td><p>阿岚</p></td><td><p>剪辑</p></td></tr></tbody></table>"
        );
    }

    #[test]
    fn embedded_images_become_placeholders() {
        // 1×1 PNG
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let doc = wrap(
            r#"<w:p><w:r><w:t>看图：</w:t></w:r><w:r><w:drawing><a:blip r:embed="rId7"/></w:drawing></w:r></w:p>"#,
        );
        let parsed =
            parse_docx(&build_docx(&doc, None, &[("word/media/image1.png", png)])).unwrap();
        assert_eq!(parsed.html, "<p>看图：{{img:0}}</p>");
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].ext, "png");
        assert_eq!(parsed.images[0].bytes.len(), png.len());
    }

    #[test]
    fn hostile_content_is_translated_then_sanitized() {
        let doc = wrap(
            r#"<w:p><w:r><w:t>&lt;script&gt;alert(1)&lt;/script&gt;</w:t></w:r></w:p>
               <w:p><w:r><w:drawing><wps:txbx><w:p><w:r><w:t>文本框里的字</w:t></w:r></w:p></wps:txbx></w:drawing></w:r><w:r><w:t>正文</w:t></w:r></w:p>"#,
        );
        let parsed = parse_docx(&build_docx(&doc, None, &[])).unwrap();
        // 文本里的尖括号被转义，不会变成标签；文本框内容（在 drawing 里）整段忽略。
        assert!(parsed.html.contains("&lt;script&gt;"), "{}", parsed.html);
        assert!(!parsed.html.contains("<script>"), "{}", parsed.html);
        assert!(!parsed.html.contains("文本框里的字"), "{}", parsed.html);
        assert!(parsed.html.contains("正文"), "{}", parsed.html);
    }

    #[test]
    fn not_a_docx_is_rejected_with_a_readable_message() {
        let err = parse_docx(b"not a zip at all").unwrap_err();
        assert!(err.public_message().contains("Word 文档"));
        let err = parse_docx(&build_docx(&wrap(""), None, &[]));
        // 正文只有空 body：能解析，但没有任何段落。
        assert!(err.is_ok());
    }

    #[test]
    fn image_with_wrong_magic_is_skipped() {
        let doc = wrap(r#"<w:p><w:r><w:t>坏图</w:t></w:r></w:p>"#);
        let parsed = parse_docx(&build_docx(
            &doc,
            None,
            &[("word/media/image1.png", b"not a png at all")],
        ))
        .unwrap();
        assert!(parsed.images.is_empty());
        assert!(parsed.notes.iter().any(|n| n.contains("格式无法识别")));
    }
}
