use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ab_glyph::{Font, FontArc, FontVec, GlyphId, PxScale, ScaleFont};
use anyhow::{Context, Result, bail};
use chrono::Local;
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;
use tokio::fs;

use crate::utils::sha1_hex;

const TOOL_REQUEST_START: &str = "<<<CAIN_CODEX_TOOL_START>>>";
const TOOL_REQUEST_END: &str = "<<<CAIN_CODEX_TOOL_END>>>";
const RENDER_DIR_NAME: &str = "cain-reply-markdown-images";
const MAX_RENDER_CHARS: usize = 20_000;
const KEEP_RENDER_FILES: usize = 120;
const MAX_RENDER_AGE_SECS: u64 = 4 * 60 * 60;
const CANVAS_WIDTH: u32 = 1240;
const CARD_WIDTH: u32 = 1080;
const CARD_MARGIN_X: u32 = (CANVAS_WIDTH - CARD_WIDTH) / 2;
const OUTER_MARGIN_Y: u32 = 48;
const CARD_PADDING_X: u32 = 44;
const CARD_PADDING_Y: u32 = 40;
const HEADER_HEIGHT: u32 = 58;
const FOOTER_HEIGHT: u32 = 40;
const MIN_CANVAS_HEIGHT: u32 = 420;
const MAX_CANVAS_HEIGHT: u32 = 14_000;
const BLOCK_GAP: u32 = 18;

static REPLY_FONT_FAMILY: OnceLock<Option<ReplyFontFamily>> = OnceLock::new();

#[derive(Clone)]
struct ReplyFontFamily {
    regular: FontArc,
    bold: FontArc,
    mono: FontArc,
}

#[derive(Debug, Clone)]
enum Block {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    Quote(String),
    List {
        ordered: bool,
        items: Vec<String>,
    },
    Table {
        headers: Vec<String>,
        aligns: Vec<TableAlign>,
        rows: Vec<Vec<String>>,
    },
    Code {
        language: String,
        code: String,
    },
    Image {
        alt: String,
        url: String,
    },
    Hr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontRole {
    Regular,
    Bold,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParagraphTone {
    Normal,
    Muted,
}

#[derive(Debug, Clone, Copy)]
struct TextStyle {
    font_role: FontRole,
    font_px: f32,
    line_height: u32,
    color: Rgba<u8>,
}

#[derive(Debug, Clone)]
struct ListItemLayout {
    marker: String,
    lines: Vec<String>,
}

#[derive(Debug, Clone)]
struct TableRowLayout {
    cells: Vec<Vec<String>>,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeLanguage {
    Generic,
    Rust,
    JavaScript,
    Shell,
    Json,
    Python,
    Yaml,
    Toml,
    Sql,
}

#[derive(Debug, Clone)]
struct CodeSpan {
    text: String,
    color: Rgba<u8>,
}

#[derive(Debug, Clone)]
enum PreparedBlock {
    Heading {
        level: u8,
        lines: Vec<String>,
        style: TextStyle,
        height: u32,
    },
    Paragraph {
        lines: Vec<String>,
        tone: ParagraphTone,
        style: TextStyle,
        height: u32,
    },
    Quote {
        lines: Vec<String>,
        style: TextStyle,
        height: u32,
    },
    List {
        ordered: bool,
        items: Vec<ListItemLayout>,
        style: TextStyle,
        marker_slot_width: u32,
        gap: u32,
        height: u32,
    },
    Table {
        header: TableRowLayout,
        rows: Vec<TableRowLayout>,
        aligns: Vec<TableAlign>,
        column_widths: Vec<u32>,
        header_style: TextStyle,
        cell_style: TextStyle,
        height: u32,
    },
    Code {
        language: String,
        highlight: CodeLanguage,
        lines: Vec<String>,
        style: TextStyle,
        height: u32,
    },
    Image {
        title_lines: Vec<String>,
        url_lines: Vec<String>,
        height: u32,
    },
    Hr {
        height: u32,
    },
}

impl PreparedBlock {
    fn height(&self) -> u32 {
        match self {
            PreparedBlock::Heading { height, .. }
            | PreparedBlock::Paragraph { height, .. }
            | PreparedBlock::Quote { height, .. }
            | PreparedBlock::List { height, .. }
            | PreparedBlock::Table { height, .. }
            | PreparedBlock::Code { height, .. }
            | PreparedBlock::Image { height, .. }
            | PreparedBlock::Hr { height } => *height,
        }
    }
}

#[derive(Debug, Clone)]
struct FenceSpec {
    marker: char,
    len: usize,
    info: String,
    closing: bool,
}

pub async fn render_reply_markdown_image(reply_text: &str) -> Result<Option<String>> {
    let normalized = sanitize_reply_text(reply_text);
    if normalized.is_empty() {
        return Ok(None);
    }

    let render_dir = std::env::temp_dir().join(RENDER_DIR_NAME);
    fs::create_dir_all(&render_dir)
        .await
        .with_context(|| format!("创建渲染目录失败: {}", render_dir.display()))?;
    cleanup_old_images(&render_dir).await;

    let output_path = build_render_output_path(&render_dir, &normalized);
    let render_source = normalized.clone();
    let render_target = output_path.clone();

    tokio::task::spawn_blocking(move || render_document_to_path(&render_target, &render_source))
        .await
        .context("等待 Markdown 渲染线程失败")??;

    Ok(Some(output_path.to_string_lossy().to_string()))
}

fn render_document_to_path(output_path: &Path, text: &str) -> Result<()> {
    let fonts =
        reply_font_family().ok_or_else(|| anyhow::anyhow!("未找到可用的 Markdown 渲染字体"))?;
    let blocks = parse_markdown_blocks(text);
    if blocks.is_empty() {
        bail!("没有可渲染的 Markdown 内容");
    }

    let max_content_height = MAX_CANVAS_HEIGHT
        .saturating_sub(OUTER_MARGIN_Y * 2)
        .saturating_sub(CARD_PADDING_Y * 2)
        .saturating_sub(HEADER_HEIGHT)
        .saturating_sub(FOOTER_HEIGHT);
    let prepared = prepare_document(&blocks, fonts, max_content_height);
    if prepared.is_empty() {
        bail!("Markdown 内容布局后为空");
    }

    let content_height = prepared.iter().map(PreparedBlock::height).sum::<u32>();
    let card_height =
        CARD_PADDING_Y + HEADER_HEIGHT + content_height + FOOTER_HEIGHT + CARD_PADDING_Y;
    let canvas_height = (card_height + OUTER_MARGIN_Y * 2)
        .max(MIN_CANVAS_HEIGHT)
        .min(MAX_CANVAS_HEIGHT);

    let mut image = RgbaImage::new(CANVAS_WIDTH, canvas_height);
    fill_vertical_gradient(&mut image, rgba(17, 23, 32), rgba(9, 12, 18));

    let card_x = CARD_MARGIN_X;
    let card_y = ((canvas_height.saturating_sub(card_height)) / 2).max(OUTER_MARGIN_Y / 2);
    draw_card(&mut image, card_x, card_y, CARD_WIDTH, card_height);
    draw_header(&mut image, fonts, card_x, card_y);

    let mut cursor_y = card_y + CARD_PADDING_Y + HEADER_HEIGHT;
    let content_x = card_x + CARD_PADDING_X;
    let content_width = CARD_WIDTH.saturating_sub(CARD_PADDING_X * 2);
    for block in &prepared {
        render_prepared_block(&mut image, fonts, block, content_x, cursor_y, content_width);
        cursor_y = cursor_y.saturating_add(block.height());
    }

    draw_footer(&mut image, fonts, card_x, card_y, card_height);
    image
        .save(output_path)
        .with_context(|| format!("保存渲染图片失败: {}", output_path.display()))?;
    Ok(())
}

fn prepare_document(
    blocks: &[Block],
    fonts: &ReplyFontFamily,
    max_content_height: u32,
) -> Vec<PreparedBlock> {
    let mut prepared = Vec::new();
    let mut used_height = 0u32;
    let content_width = CARD_WIDTH.saturating_sub(CARD_PADDING_X * 2);
    let mut truncated = false;

    for block in blocks {
        let candidate = prepare_block(block.clone(), fonts, content_width);
        let block_height = candidate.height();
        if used_height.saturating_add(block_height) > max_content_height {
            truncated = true;
            break;
        }
        used_height = used_height.saturating_add(block_height);
        prepared.push(candidate);
    }

    if truncated {
        let note = prepare_paragraph_block(
            "…(内容过长，后续已省略)",
            fonts,
            content_width,
            ParagraphTone::Muted,
        );
        while used_height.saturating_add(note.height()) > max_content_height && !prepared.is_empty()
        {
            let removed = prepared.pop().expect("pop checked");
            used_height = used_height.saturating_sub(removed.height());
        }
        if used_height.saturating_add(note.height()) <= max_content_height {
            prepared.push(note);
        }
    }

    prepared
}

fn prepare_block(block: Block, fonts: &ReplyFontFamily, max_width: u32) -> PreparedBlock {
    match block {
        Block::Heading { level, text } => {
            let style = heading_style(level);
            let lines = wrap_text_for_style(
                &simplify_inline_markdown(&text),
                fonts,
                style,
                max_width,
                true,
            );
            let body_height = lines.len().max(1) as u32 * style.line_height;
            let extra = if level == 1 { 18 } else { 8 };
            PreparedBlock::Heading {
                level,
                lines,
                style,
                height: body_height + extra + BLOCK_GAP,
            }
        }
        Block::Paragraph(text) => {
            prepare_paragraph_block(&text, fonts, max_width, ParagraphTone::Normal)
        }
        Block::Quote(text) => {
            let style = quote_style();
            let lines = wrap_text_for_style(
                &simplify_inline_markdown(&text),
                fonts,
                style,
                max_width.saturating_sub(34),
                true,
            );
            let body_height = lines.len().max(1) as u32 * style.line_height;
            PreparedBlock::Quote {
                lines,
                style,
                height: body_height + 24 + BLOCK_GAP,
            }
        }
        Block::List { ordered, items } => {
            let style = body_style();
            let marker_style = list_marker_style(ordered);
            let sample_marker = if ordered {
                format!("{}.", items.len().max(1))
            } else {
                "•".to_string()
            };
            let marker_slot_width = measure_text_line_width(fonts, marker_style, &sample_marker)
                .max(if ordered { 28 } else { 20 });
            let gap = if ordered { 10 } else { 8 };
            let item_max_width = max_width.saturating_sub(marker_slot_width + gap).max(120);
            let mut body_height = 0u32;
            let mut prepared_items = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let marker = if ordered {
                    format!("{}.", index + 1)
                } else {
                    "•".to_string()
                };
                let lines = wrap_text_for_style(
                    &simplify_inline_markdown(item),
                    fonts,
                    style,
                    item_max_width,
                    true,
                );
                body_height = body_height
                    .saturating_add(lines.len().max(1) as u32 * style.line_height)
                    .saturating_add(6);
                prepared_items.push(ListItemLayout { marker, lines });
            }
            PreparedBlock::List {
                ordered,
                items: prepared_items,
                style,
                marker_slot_width,
                gap,
                height: body_height.saturating_sub(6) + BLOCK_GAP,
            }
        }
        Block::Table {
            headers,
            aligns,
            rows,
        } => prepare_table_block(headers, aligns, rows, fonts, max_width),
        Block::Code { language, code } => {
            let style = code_style();
            let highlight = detect_code_language(&language, &code);
            let lines = wrap_code_lines(&code, fonts, style, max_width.saturating_sub(28));
            let mut body_height = lines.len().max(1) as u32 * style.line_height + 22;
            if !language.trim().is_empty() {
                body_height = body_height.saturating_add(18);
            }
            PreparedBlock::Code {
                language,
                highlight,
                lines,
                style,
                height: body_height + BLOCK_GAP,
            }
        }
        Block::Image { alt, url } => {
            let title = if alt.trim().is_empty() {
                "图片".to_string()
            } else {
                format!("图片: {}", alt.trim())
            };
            let title_style = body_style();
            let url_style = meta_style();
            let title_lines = wrap_text_for_style(
                &title,
                fonts,
                title_style,
                max_width.saturating_sub(32),
                true,
            );
            let url_lines =
                wrap_text_for_style(&url, fonts, url_style, max_width.saturating_sub(32), true);
            let body_height = title_lines.len().max(1) as u32 * title_style.line_height
                + url_lines.len().max(1) as u32 * url_style.line_height
                + 28;
            PreparedBlock::Image {
                title_lines,
                url_lines,
                height: body_height + BLOCK_GAP,
            }
        }
        Block::Hr => PreparedBlock::Hr { height: 28 },
    }
}

fn prepare_paragraph_block(
    text: &str,
    fonts: &ReplyFontFamily,
    max_width: u32,
    tone: ParagraphTone,
) -> PreparedBlock {
    let style = if tone == ParagraphTone::Muted {
        muted_body_style()
    } else {
        body_style()
    };
    let lines = wrap_text_for_style(
        &simplify_inline_markdown(text),
        fonts,
        style,
        max_width,
        true,
    );
    let body_height = lines.len().max(1) as u32 * style.line_height;
    PreparedBlock::Paragraph {
        lines,
        tone,
        style,
        height: body_height + BLOCK_GAP,
    }
}

fn prepare_table_block(
    headers: Vec<String>,
    aligns: Vec<TableAlign>,
    rows: Vec<Vec<String>>,
    fonts: &ReplyFontFamily,
    max_width: u32,
) -> PreparedBlock {
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);
    let mut normalized_headers = headers
        .into_iter()
        .map(|cell| simplify_inline_markdown(&cell))
        .collect::<Vec<_>>();
    normalized_headers.resize(columns, String::new());

    let mut normalized_rows = rows
        .into_iter()
        .map(|row| {
            let mut normalized = row
                .into_iter()
                .map(|cell| simplify_inline_markdown(&cell))
                .collect::<Vec<_>>();
            normalized.resize(columns, String::new());
            normalized
        })
        .collect::<Vec<_>>();

    if normalized_rows.is_empty() {
        normalized_rows.push(vec![String::new(); columns]);
    }

    let mut normalized_aligns = aligns;
    normalized_aligns.resize(columns, TableAlign::Left);

    let header_style = table_header_style();
    let cell_style = table_body_style();
    let cell_padding_x = 12u32;
    let cell_padding_y = 9u32;

    let preferred_min_width = match columns {
        1 => max_width.max(96),
        2 => 180,
        3 => 128,
        4 => 102,
        _ => 84,
    };
    let min_col_width = preferred_min_width
        .min(max_width.saturating_div(columns as u32).max(68))
        .max(68);

    let mut column_widths = Vec::with_capacity(columns);
    for col in 0..columns {
        let mut preferred = measure_text_line_width(fonts, header_style, &normalized_headers[col]);
        for row in &normalized_rows {
            preferred = preferred.max(measure_text_line_width(fonts, cell_style, &row[col]));
        }
        column_widths.push(
            preferred.max(min_col_width.saturating_sub(cell_padding_x * 2)) + cell_padding_x * 2,
        );
    }
    normalize_table_column_widths(&mut column_widths, max_width, min_col_width);

    let header = build_table_row_layout(
        &normalized_headers,
        &column_widths,
        fonts,
        header_style,
        cell_padding_x,
        cell_padding_y,
    );
    let rows = normalized_rows
        .iter()
        .map(|row| {
            build_table_row_layout(
                row,
                &column_widths,
                fonts,
                cell_style,
                cell_padding_x,
                cell_padding_y,
            )
        })
        .collect::<Vec<_>>();
    let table_height = header.height + rows.iter().map(|row| row.height).sum::<u32>();

    PreparedBlock::Table {
        header,
        rows,
        aligns: normalized_aligns,
        column_widths,
        header_style,
        cell_style,
        height: table_height + BLOCK_GAP,
    }
}

fn build_table_row_layout(
    cells: &[String],
    column_widths: &[u32],
    fonts: &ReplyFontFamily,
    style: TextStyle,
    cell_padding_x: u32,
    cell_padding_y: u32,
) -> TableRowLayout {
    let mut wrapped_cells = Vec::with_capacity(column_widths.len());
    let mut row_line_count = 1u32;

    for (index, width) in column_widths.iter().enumerate() {
        let cell_text = cells.get(index).map(String::as_str).unwrap_or_default();
        let inner_width = width.saturating_sub(cell_padding_x * 2).max(40);
        let lines = wrap_text_for_style(cell_text, fonts, style, inner_width, true);
        row_line_count = row_line_count.max(lines.len().max(1) as u32);
        wrapped_cells.push(lines);
    }

    TableRowLayout {
        cells: wrapped_cells,
        height: row_line_count * style.line_height + cell_padding_y * 2,
    }
}

fn normalize_table_column_widths(column_widths: &mut [u32], max_width: u32, min_col_width: u32) {
    if column_widths.is_empty() {
        return;
    }

    let mut total = column_widths.iter().copied().sum::<u32>();
    while total > max_width {
        let mut reduced = false;
        for width in column_widths.iter_mut() {
            if *width > min_col_width {
                *width -= 1;
                total -= 1;
                reduced = true;
                if total <= max_width {
                    break;
                }
            }
        }
        if !reduced {
            break;
        }
    }

    let mut index = 0usize;
    while total < max_width {
        let slot = index % column_widths.len();
        column_widths[slot] += 1;
        total += 1;
        index += 1;
    }
}

fn render_prepared_block(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    block: &PreparedBlock,
    x: u32,
    y: u32,
    width: u32,
) {
    match block {
        PreparedBlock::Heading {
            level,
            lines,
            style,
            ..
        } => {
            draw_text_lines(image, fonts, style, x, y, lines);
            if *level == 1 {
                let underline_y = y
                    .saturating_add(lines.len().max(1) as u32 * style.line_height)
                    .saturating_add(8);
                draw_horizontal_line(
                    image,
                    x,
                    x + width.saturating_sub(1),
                    underline_y,
                    rgba(55, 63, 81),
                );
            }
        }
        PreparedBlock::Paragraph {
            lines, style, tone, ..
        } => {
            if *tone == ParagraphTone::Muted {
                draw_filled_rect(
                    image,
                    x,
                    y + 4,
                    width,
                    lines.len().max(1) as u32 * style.line_height,
                    rgba(27, 31, 40),
                );
            }
            draw_text_lines(image, fonts, style, x, y, lines);
        }
        PreparedBlock::Quote { lines, style, .. } => {
            let quote_height = lines.len().max(1) as u32 * style.line_height + 22;
            draw_filled_rect(image, x, y, width, quote_height, rgba(24, 34, 48));
            draw_filled_rect(image, x, y, 5, quote_height, rgba(79, 193, 255));
            draw_text_lines(image, fonts, style, x + 18, y + 10, lines);
        }
        PreparedBlock::List {
            ordered,
            items,
            style,
            marker_slot_width,
            gap,
            ..
        } => {
            let mut cursor_y = y;
            let marker_style = list_marker_style(*ordered);
            let text_x = x + marker_slot_width + gap;
            for item in items {
                let marker_x = x + marker_slot_width.saturating_sub(measure_text_line_width(
                    fonts,
                    marker_style,
                    &item.marker,
                ));
                draw_text_line(
                    image,
                    fonts,
                    &marker_style,
                    marker_x,
                    cursor_y,
                    &item.marker,
                );
                draw_text_lines(image, fonts, style, text_x, cursor_y, &item.lines);
                cursor_y = cursor_y
                    .saturating_add(item.lines.len().max(1) as u32 * style.line_height)
                    .saturating_add(6);
            }
        }
        PreparedBlock::Table {
            header,
            rows,
            aligns,
            column_widths,
            header_style,
            cell_style,
            ..
        } => {
            let table_width = column_widths.iter().copied().sum::<u32>().min(width).max(1);
            let table_height = header.height + rows.iter().map(|row| row.height).sum::<u32>();
            draw_filled_rect(image, x, y, table_width, table_height, rgba(27, 31, 38));
            draw_rect_border(image, x, y, table_width, table_height, rgba(60, 66, 80));
            draw_filled_rect(image, x, y, table_width, header.height, rgba(42, 45, 46));

            let mut column_x = x;
            for width in column_widths
                .iter()
                .take(column_widths.len().saturating_sub(1))
            {
                column_x = column_x.saturating_add(*width);
                draw_vertical_line(
                    image,
                    column_x,
                    y,
                    y + table_height.saturating_sub(1),
                    rgba(60, 66, 80),
                );
            }

            draw_table_row(
                image,
                fonts,
                header,
                header_style,
                aligns,
                column_widths,
                x,
                y,
            );

            let mut cursor_y = y + header.height;
            draw_horizontal_line(
                image,
                x,
                x + table_width.saturating_sub(1),
                cursor_y,
                rgba(60, 66, 80),
            );
            for (index, row) in rows.iter().enumerate() {
                let row_bg = if index % 2 == 0 {
                    rgba(30, 34, 41)
                } else {
                    rgba(24, 28, 35)
                };
                draw_filled_rect(
                    image,
                    x + 1,
                    cursor_y + 1,
                    table_width.saturating_sub(2),
                    row.height.saturating_sub(1),
                    row_bg,
                );
                draw_table_row(
                    image,
                    fonts,
                    row,
                    cell_style,
                    aligns,
                    column_widths,
                    x,
                    cursor_y,
                );
                cursor_y = cursor_y.saturating_add(row.height);
                if cursor_y < y + table_height {
                    draw_horizontal_line(
                        image,
                        x,
                        x + table_width.saturating_sub(1),
                        cursor_y,
                        rgba(60, 66, 80),
                    );
                }
            }
        }
        PreparedBlock::Code {
            language,
            highlight,
            lines,
            style,
            ..
        } => {
            let code_height = lines.len().max(1) as u32 * style.line_height
                + 22
                + if language.trim().is_empty() { 0 } else { 18 };
            draw_filled_rect(image, x, y, width, code_height, rgba(18, 21, 30));
            draw_rect_border(image, x, y, width, code_height, rgba(48, 54, 68));
            let mut cursor_y = y + 12;
            if !language.trim().is_empty() {
                let label_style = code_label_style();
                draw_text_line(
                    image,
                    fonts,
                    &label_style,
                    x + 14,
                    cursor_y,
                    language.trim(),
                );
                cursor_y = cursor_y.saturating_add(18);
            }
            draw_highlighted_code_lines(image, fonts, style, x + 14, cursor_y, *highlight, lines);
        }
        PreparedBlock::Image {
            title_lines,
            url_lines,
            ..
        } => {
            let title_style = body_style();
            let url_style = meta_style();
            let panel_height = title_lines.len().max(1) as u32 * title_style.line_height
                + url_lines.len().max(1) as u32 * url_style.line_height
                + 28;
            draw_filled_rect(image, x, y, width, panel_height, rgba(24, 28, 36));
            draw_rect_border(image, x, y, width, panel_height, rgba(62, 70, 84));
            draw_text_line(image, fonts, &code_label_style(), x + 14, y + 10, "Image");
            draw_text_lines(image, fonts, &title_style, x + 14, y + 28, title_lines);
            let url_y = y + 28 + title_lines.len().max(1) as u32 * title_style.line_height + 2;
            draw_text_lines(image, fonts, &url_style, x + 14, url_y, url_lines);
        }
        PreparedBlock::Hr { .. } => {
            let line_y = y + 12;
            draw_horizontal_line(
                image,
                x,
                x + width.saturating_sub(1),
                line_y,
                rgba(55, 60, 73),
            );
        }
    }
}

fn draw_table_row(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    row: &TableRowLayout,
    style: &TextStyle,
    aligns: &[TableAlign],
    column_widths: &[u32],
    x: u32,
    y: u32,
) {
    let cell_padding_x = 12u32;
    let cell_padding_y = 9u32;
    let mut cursor_x = x;
    for (index, width) in column_widths.iter().enumerate() {
        let lines = row.cells.get(index).map(Vec::as_slice).unwrap_or(&[]);
        let align = aligns.get(index).copied().unwrap_or(TableAlign::Left);
        draw_text_lines_aligned(
            image,
            fonts,
            style,
            cursor_x + cell_padding_x,
            y + cell_padding_y,
            width.saturating_sub(cell_padding_x * 2),
            lines,
            align,
        );
        cursor_x = cursor_x.saturating_add(*width);
    }
}

fn draw_header(image: &mut RgbaImage, fonts: &ReplyFontFamily, card_x: u32, card_y: u32) {
    let title_style = TextStyle {
        font_role: FontRole::Bold,
        font_px: 22.0,
        line_height: 28,
        color: rgba(236, 240, 247),
    };
    let meta = TextStyle {
        font_role: FontRole::Regular,
        font_px: 15.0,
        line_height: 20,
        color: rgba(152, 162, 179),
    };
    let title_x = card_x + CARD_PADDING_X;
    let title_y = card_y + CARD_PADDING_Y - 4;
    draw_text_line(
        image,
        fonts,
        &title_style,
        title_x,
        title_y,
        "CainBot Reply Snapshot",
    );
    draw_text_line(
        image,
        fonts,
        &meta,
        title_x,
        title_y + 28,
        &Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    );
    draw_filled_rect(
        image,
        title_x,
        card_y + CARD_PADDING_Y + HEADER_HEIGHT - 10,
        92,
        4,
        rgba(79, 193, 255),
    );
}

fn draw_footer(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    card_x: u32,
    card_y: u32,
    card_height: u32,
) {
    let footer_y = card_y + card_height - CARD_PADDING_Y - FOOTER_HEIGHT + 6;
    let left = card_x + CARD_PADDING_X;
    let right = card_x + CARD_WIDTH - CARD_PADDING_X;
    draw_horizontal_line(image, left, right, footer_y, rgba(47, 53, 66));
    let style = meta_style();
    draw_text_line(
        image,
        fonts,
        &style,
        left,
        footer_y + 10,
        "CainBot Rust Renderer  |  https://github.com/DeterMination-Wind/CainBot-Rust",
    );
}

fn sanitize_reply_text(source_text: &str) -> String {
    let without_tools = strip_tool_blocks(source_text);
    let normalized_newlines = without_tools.replace("\r\n", "\n");
    let unwrapped = unwrap_nested_markdown_fences(&normalized_newlines);
    let mut cleaned = unwrapped
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if cleaned.chars().count() > MAX_RENDER_CHARS {
        cleaned = truncate_chars(&cleaned, MAX_RENDER_CHARS);
        cleaned.push_str("\n\n…(内容过长，后续已省略)");
    }
    cleaned
}

fn strip_tool_blocks(source_text: &str) -> String {
    let mut remaining = source_text;
    let mut result = String::with_capacity(source_text.len());
    loop {
        let Some(start) = remaining.find(TOOL_REQUEST_START) else {
            result.push_str(remaining);
            break;
        };
        result.push_str(&remaining[..start]);
        let after_start = &remaining[start + TOOL_REQUEST_START.len()..];
        let Some(end) = after_start.find(TOOL_REQUEST_END) else {
            break;
        };
        remaining = &after_start[end + TOOL_REQUEST_END.len()..];
    }
    result
}

fn unwrap_nested_markdown_fences(text: &str) -> String {
    let lines = text.split('\n').collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let Some(open_fence) = parse_fence(lines[index]) else {
            output.push(lines[index]);
            index += 1;
            continue;
        };
        if !matches!(
            open_fence.info.to_ascii_lowercase().as_str(),
            "md" | "markdown"
        ) {
            output.push(lines[index]);
            index += 1;
            continue;
        }

        let mut cursor = index + 1;
        let mut nested_depth = 0usize;
        let mut body_lines = Vec::new();
        let mut closed = false;

        while cursor < lines.len() {
            if let Some(fence) = parse_fence(lines[cursor])
                && fence.marker == open_fence.marker
                && fence.len >= open_fence.len
            {
                if nested_depth == 0 && fence.closing {
                    closed = true;
                    cursor += 1;
                    break;
                }
                if fence.closing {
                    nested_depth = nested_depth.saturating_sub(1);
                } else {
                    nested_depth = nested_depth.saturating_add(1);
                }
            }
            body_lines.push(lines[cursor]);
            cursor += 1;
        }

        if !closed {
            output.push(lines[index]);
            output.extend(body_lines);
            index = cursor;
            continue;
        }

        let start = body_lines
            .iter()
            .position(|line| !line.trim().is_empty())
            .unwrap_or(body_lines.len());
        let end = body_lines
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map(|item| item + 1)
            .unwrap_or(start);
        output.extend(body_lines[start..end].iter().copied());
        index = cursor;
    }
    output.join("\n")
}

fn parse_markdown_blocks(text: &str) -> Vec<Block> {
    let lines = text.split('\n').collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() {
            index += 1;
            continue;
        }

        if let Some(fence) = parse_fence(line) {
            let mut cursor = index + 1;
            let mut code_lines = Vec::new();
            while cursor < lines.len() {
                if let Some(closing) = parse_fence(lines[cursor])
                    && closing.marker == fence.marker
                    && closing.len >= fence.len
                    && closing.closing
                {
                    cursor += 1;
                    break;
                }
                code_lines.push(lines[cursor]);
                cursor += 1;
            }
            blocks.push(Block::Code {
                language: fence.info.trim().to_string(),
                code: code_lines.join("\n"),
            });
            index = cursor;
            continue;
        }

        if is_horizontal_rule(line) {
            blocks.push(Block::Hr);
            index += 1;
            continue;
        }

        if let Some((level, content)) = parse_heading(line) {
            blocks.push(Block::Heading {
                level,
                text: content.to_string(),
            });
            index += 1;
            continue;
        }

        if let Some((table, cursor)) = parse_table_block(&lines, index) {
            blocks.push(table);
            index = cursor;
            continue;
        }

        if let Some((alt, url)) = parse_standalone_image(line.trim()) {
            blocks.push(Block::Image { alt, url });
            index += 1;
            continue;
        }

        if line.trim_start().starts_with('>') {
            let mut cursor = index;
            let mut quote_lines = Vec::new();
            while cursor < lines.len() {
                let trimmed = lines[cursor].trim_start();
                if !trimmed.starts_with('>') {
                    break;
                }
                quote_lines.push(trimmed.trim_start_matches('>').trim_start().to_string());
                cursor += 1;
            }
            blocks.push(Block::Quote(quote_lines.join("\n")));
            index = cursor;
            continue;
        }

        if let Some((ordered, first_item)) = parse_list_item(line) {
            let mut cursor = index + 1;
            let mut items = vec![first_item];
            while cursor < lines.len() {
                let next_line = lines[cursor];
                if next_line.trim().is_empty() {
                    break;
                }
                if let Some((next_ordered, next_item)) = parse_list_item(next_line) {
                    if next_ordered != ordered {
                        break;
                    }
                    items.push(next_item);
                    cursor += 1;
                    continue;
                }
                if let Some(last) = items.last_mut() {
                    last.push(' ');
                    last.push_str(next_line.trim());
                    cursor += 1;
                    continue;
                }
                break;
            }
            blocks.push(Block::List { ordered, items });
            index = cursor;
            continue;
        }

        let mut cursor = index + 1;
        let mut paragraph_lines = vec![line.trim().to_string()];
        while cursor < lines.len() {
            let next_line = lines[cursor];
            if next_line.trim().is_empty()
                || starts_special_block(next_line)
                || parse_table_block(&lines, cursor).is_some()
            {
                break;
            }
            paragraph_lines.push(next_line.trim().to_string());
            cursor += 1;
        }
        blocks.push(Block::Paragraph(paragraph_lines.join(" ")));
        index = cursor;
    }

    blocks
}

fn starts_special_block(line: &str) -> bool {
    parse_fence(line).is_some()
        || is_horizontal_rule(line)
        || parse_heading(line).is_some()
        || parse_list_item(line).is_some()
        || line.trim_start().starts_with('>')
        || parse_standalone_image(line.trim()).is_some()
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed[hashes..].trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((hashes as u8, rest))
}

fn parse_fence(line: &str) -> Option<FenceSpec> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|ch| *ch == marker).count();
    if len < 3 {
        return None;
    }
    let info = trimmed[len..].trim().to_string();
    Some(FenceSpec {
        marker,
        len,
        info: info.clone(),
        closing: info.is_empty(),
    })
}

fn parse_list_item(line: &str) -> Option<(bool, String)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return Some((false, rest.trim().to_string()));
    }

    let mut digits_len = 0usize;
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            digits_len += ch.len_utf8();
        } else {
            break;
        }
    }
    if digits_len == 0 || digits_len >= trimmed.len() {
        return None;
    }
    let marker_char = trimmed[digits_len..].chars().next()?;
    if marker_char != '.' && marker_char != ')' {
        return None;
    }
    let rest = trimmed[digits_len + marker_char.len_utf8()..].trim_start();
    (!rest.is_empty()).then_some((true, rest.to_string()))
}

fn parse_table_block(lines: &[&str], index: usize) -> Option<(Block, usize)> {
    if index + 1 >= lines.len()
        || !looks_like_table_row(lines[index])
        || !looks_like_table_separator(lines[index + 1])
    {
        return None;
    }

    let aligns = parse_table_separator(lines[index + 1])?;
    let headers = parse_table_cells(lines[index]);
    if headers.is_empty() {
        return None;
    }

    let mut cursor = index + 2;
    let mut rows = Vec::new();
    while cursor < lines.len() {
        let line = lines[cursor];
        if line.trim().is_empty() || !looks_like_table_row(line) {
            break;
        }
        let cells = parse_table_cells(line);
        if cells.is_empty() {
            break;
        }
        rows.push(cells);
        cursor += 1;
    }

    Some((
        Block::Table {
            headers,
            aligns,
            rows,
        },
        cursor,
    ))
}

fn parse_table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if matches!(chars.peek(), Some('|')) {
                current.push('|');
                chars.next();
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '|' {
            cells.push(current.trim().to_string());
            current.clear();
            continue;
        }
        current.push(ch);
    }
    cells.push(current.trim().to_string());

    if trimmed.starts_with('|') && cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if trimmed.ends_with('|') && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
}

fn parse_table_separator(line: &str) -> Option<Vec<TableAlign>> {
    let cells = parse_table_cells(line);
    if cells.is_empty() {
        return None;
    }

    let mut aligns = Vec::with_capacity(cells.len());
    for cell in cells {
        let normalized = cell.trim();
        if normalized.is_empty() {
            return None;
        }
        if !normalized.chars().all(|ch| ch == '-' || ch == ':') {
            return None;
        }
        if normalized.chars().filter(|ch| *ch == '-').count() < 3 {
            return None;
        }
        let align = if normalized.starts_with(':') && normalized.ends_with(':') {
            TableAlign::Center
        } else if normalized.ends_with(':') {
            TableAlign::Right
        } else {
            TableAlign::Left
        };
        aligns.push(align);
    }
    Some(aligns)
}

fn parse_standalone_image(line: &str) -> Option<(String, String)> {
    parse_markdown_image_token(line)
        .filter(|(_, _, consumed)| *consumed == line.len())
        .map(|(alt, url, _)| (alt, url))
}

fn parse_markdown_image_token(source: &str) -> Option<(String, String, usize)> {
    if !source.starts_with("![") {
        return None;
    }
    let label_start = 2;
    let label_end = source[label_start..].find(']')? + label_start;
    let after_label = &source[label_end + 1..];
    if !after_label.starts_with('(') {
        return None;
    }
    let url_end = after_label[1..].find(')')? + label_end + 2;
    let alt = source[label_start..label_end].trim().to_string();
    let url = source[label_end + 2..url_end].trim().to_string();
    Some((alt, url, url_end + 1))
}

fn parse_markdown_link_token(source: &str) -> Option<(String, String, usize)> {
    if !source.starts_with('[') {
        return None;
    }
    let label_end = source[1..].find(']')? + 1;
    let after_label = &source[label_end + 1..];
    if !after_label.starts_with('(') {
        return None;
    }
    let url_end = after_label[1..].find(')')? + label_end + 2;
    let label = source[1..label_end].trim().to_string();
    let url = source[label_end + 2..url_end].trim().to_string();
    Some((label, url, url_end + 1))
}

fn simplify_inline_markdown(text: &str) -> String {
    let source = text.trim();
    if source.is_empty() {
        return String::new();
    }

    let mut output = String::with_capacity(source.len());
    let mut index = 0usize;
    while index < source.len() {
        let remain = &source[index..];
        if let Some((alt, url, consumed)) = parse_markdown_image_token(remain) {
            if !output.is_empty() && !output.ends_with(char::is_whitespace) {
                output.push(' ');
            }
            output.push_str("[图片");
            if !alt.is_empty() {
                output.push_str(": ");
                output.push_str(&alt);
            }
            output.push(']');
            if !url.is_empty() {
                output.push(' ');
                output.push_str(&url);
            }
            index += consumed;
            continue;
        }
        if let Some((label, url, consumed)) = parse_markdown_link_token(remain) {
            output.push_str(&label);
            if !url.is_empty() {
                output.push_str(" (");
                output.push_str(&url);
                output.push(')');
            }
            index += consumed;
            continue;
        }
        if let Some(rest) = remain.strip_prefix("**")
            && let Some(end) = rest.find("**")
        {
            output.push_str(&rest[..end]);
            index += 2 + end + 2;
            continue;
        }
        if let Some(rest) = remain.strip_prefix("__")
            && let Some(end) = rest.find("__")
        {
            output.push_str(&rest[..end]);
            index += 2 + end + 2;
            continue;
        }
        if let Some(rest) = remain.strip_prefix("~~")
            && let Some(end) = rest.find("~~")
        {
            output.push_str(&rest[..end]);
            index += 2 + end + 2;
            continue;
        }
        if remain.starts_with('`') {
            let tick_count = remain.chars().take_while(|ch| *ch == '`').count();
            let marker = "`".repeat(tick_count);
            let inner = &remain[tick_count..];
            if let Some(end) = inner.find(&marker) {
                output.push_str(inner[..end].trim());
                index += tick_count + end + tick_count;
                continue;
            }
        }

        let ch = remain.chars().next().expect("non-empty remain");
        output.push(ch);
        index += ch.len_utf8();
    }

    collapse_whitespace(&output)
}

fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch == '\n' {
            if !result.ends_with('\n') {
                result.push('\n');
            }
            last_was_space = false;
            continue;
        }
        if ch.is_whitespace() {
            if !last_was_space && !result.ends_with('\n') {
                result.push(' ');
            }
            last_was_space = true;
        } else {
            result.push(ch);
            last_was_space = false;
        }
    }
    result.trim().to_string()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut result = String::new();
    for ch in text.chars().take(max_chars) {
        result.push(ch);
    }
    result
}

fn wrap_text_for_style(
    text: &str,
    fonts: &ReplyFontFamily,
    style: TextStyle,
    max_width: u32,
    trim_wrapped_leading_space: bool,
) -> Vec<String> {
    if text.trim().is_empty() {
        return vec![String::new()];
    }
    let font = font_for_role(fonts, style.font_role);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let expanded = raw_line.replace('\t', "    ");
        let wrapped = wrap_single_visual_line(
            font,
            style.font_px,
            &expanded,
            max_width,
            trim_wrapped_leading_space,
        );
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_code_lines(
    text: &str,
    fonts: &ReplyFontFamily,
    style: TextStyle,
    max_width: u32,
) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let expanded = raw_line.replace('\t', "    ");
        let wrapped = wrap_single_visual_line(
            font_for_role(fonts, style.font_role),
            style.font_px,
            &expanded,
            max_width,
            false,
        );
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_single_visual_line(
    font: &FontArc,
    font_px: f32,
    raw_line: &str,
    max_width: u32,
    trim_wrapped_leading_space: bool,
) -> Vec<String> {
    if raw_line.is_empty() {
        return vec![String::new()];
    }
    if max_width == 0 {
        return vec![raw_line.to_string()];
    }

    let scaled = font.as_scaled(PxScale::from(font_px));
    let chars = raw_line.chars().collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        let mut end = start;
        let mut width = 0.0f32;
        let mut previous = None::<GlyphId>;
        let mut last_break = None::<usize>;

        while end < chars.len() {
            let ch = chars[end];
            let glyph_id = scaled.glyph_id(ch);
            let mut next_width = width;
            if let Some(last_id) = previous {
                next_width += scaled.kern(last_id, glyph_id);
            }
            next_width += scaled.h_advance(glyph_id);
            if end > start && next_width.ceil() as u32 > max_width {
                break;
            }
            width = next_width;
            previous = Some(glyph_id);
            end += 1;
            if is_wrap_break_char(ch) {
                last_break = Some(end);
            }
        }

        if end == chars.len() {
            let line = chars[start..end].iter().collect::<String>();
            lines.push(if trim_wrapped_leading_space {
                line.trim().to_string()
            } else {
                line
            });
            break;
        }

        let break_at = last_break.filter(|pos| *pos > start).unwrap_or(end);
        let mut line = chars[start..break_at].iter().collect::<String>();
        if trim_wrapped_leading_space {
            line = line.trim().to_string();
        }
        if line.is_empty() && break_at == end {
            line = chars[start..end].iter().collect::<String>();
        }
        lines.push(line);
        start = break_at;
        if trim_wrapped_leading_space {
            while start < chars.len() && chars[start].is_whitespace() {
                start += 1;
            }
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn is_wrap_break_char(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | '.'
                | ';'
                | ':'
                | '!'
                | '?'
                | '/'
                | '\\'
                | '|'
                | ')'
                | ']'
                | '}'
                | '-'
                | '，'
                | '。'
                | '；'
                | '：'
                | '！'
                | '？'
                | '）'
                | '】'
                | '》'
                | '、'
        )
        || is_cjk(ch)
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x2E80..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

fn measure_text_line_width(fonts: &ReplyFontFamily, style: TextStyle, text: &str) -> u32 {
    measure_line_width_font(font_for_role(fonts, style.font_role), style.font_px, text).ceil()
        as u32
}

fn measure_line_width_font(font: &FontArc, font_px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(font_px));
    let mut width = 0.0f32;
    let mut previous = None::<GlyphId>;
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        if let Some(last_id) = previous {
            width += scaled.kern(last_id, glyph_id);
        }
        width += scaled.h_advance(glyph_id);
        previous = Some(glyph_id);
    }
    width.max(0.0)
}

fn fill_vertical_gradient(image: &mut RgbaImage, top: Rgba<u8>, bottom: Rgba<u8>) {
    let height = image.height().max(1);
    for y in 0..image.height() {
        let t = y as f32 / (height.saturating_sub(1)).max(1) as f32;
        let row_color = lerp_rgba(top, bottom, t);
        for x in 0..image.width() {
            image.put_pixel(x, y, row_color);
        }
    }
}

fn draw_card(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32) {
    draw_filled_rect(image, x, y, w, h, rgba(31, 35, 43));
    draw_rect_border(image, x, y, w, h, rgba(58, 65, 78));
    draw_horizontal_line(image, x, x + w.saturating_sub(1), y + 1, rgba(71, 81, 98));
}

fn draw_text_lines(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    style: &TextStyle,
    x: u32,
    y: u32,
    lines: &[String],
) {
    let mut cursor_y = y;
    for line in lines {
        draw_text_line(image, fonts, style, x, cursor_y, line);
        cursor_y = cursor_y.saturating_add(style.line_height);
    }
}

fn draw_text_lines_aligned(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    style: &TextStyle,
    x: u32,
    y: u32,
    width: u32,
    lines: &[String],
    align: TableAlign,
) {
    let mut cursor_y = y;
    for line in lines {
        let line_width = measure_text_line_width(fonts, *style, line);
        let draw_x = match align {
            TableAlign::Left => x,
            TableAlign::Center => x + width.saturating_sub(line_width) / 2,
            TableAlign::Right => x + width.saturating_sub(line_width),
        };
        draw_text_line(image, fonts, style, draw_x, cursor_y, line);
        cursor_y = cursor_y.saturating_add(style.line_height);
    }
}

fn draw_text_line(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    style: &TextStyle,
    x: u32,
    y: u32,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let font = font_for_role(fonts, style.font_role);
    draw_text_mut(
        image,
        style.color,
        x as i32,
        y as i32,
        PxScale::from(style.font_px),
        font,
        text,
    );
}

fn draw_text_segments(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    style: &TextStyle,
    x: u32,
    y: u32,
    segments: &[CodeSpan],
) {
    let font = font_for_role(fonts, style.font_role);
    let mut cursor_x = x as f32;
    for segment in segments {
        if segment.text.is_empty() {
            continue;
        }
        if segment.text.chars().any(|ch| !ch.is_whitespace()) {
            draw_text_mut(
                image,
                segment.color,
                cursor_x.round() as i32,
                y as i32,
                PxScale::from(style.font_px),
                font,
                &segment.text,
            );
        }
        cursor_x += measure_line_width_font(font, style.font_px, &segment.text);
    }
}

fn draw_highlighted_code_lines(
    image: &mut RgbaImage,
    fonts: &ReplyFontFamily,
    style: &TextStyle,
    x: u32,
    y: u32,
    language: CodeLanguage,
    lines: &[String],
) {
    let mut cursor_y = y;
    for line in lines {
        let segments = highlight_code_line(line, language, style.color);
        draw_text_segments(image, fonts, style, x, cursor_y, &segments);
        cursor_y = cursor_y.saturating_add(style.line_height);
    }
}

fn draw_horizontal_line(image: &mut RgbaImage, start_x: u32, end_x: u32, y: u32, color: Rgba<u8>) {
    if y >= image.height() {
        return;
    }
    let safe_start = start_x.min(image.width().saturating_sub(1));
    let safe_end = end_x.min(image.width().saturating_sub(1));
    for x in safe_start..=safe_end {
        image.put_pixel(x, y, color);
    }
}

fn draw_vertical_line(image: &mut RgbaImage, x: u32, start_y: u32, end_y: u32, color: Rgba<u8>) {
    if x >= image.width() {
        return;
    }
    let safe_start = start_y.min(image.height().saturating_sub(1));
    let safe_end = end_y.min(image.height().saturating_sub(1));
    for y in safe_start..=safe_end {
        image.put_pixel(x, y, color);
    }
}

fn draw_filled_rect(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if w == 0 || h == 0 {
        return;
    }
    let end_x = x.saturating_add(w).min(image.width());
    let end_y = y.saturating_add(h).min(image.height());
    for py in y..end_y {
        for px in x..end_x {
            image.put_pixel(px, py, color);
        }
    }
}

fn draw_rect_border(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if w < 2 || h < 2 || x >= image.width() || y >= image.height() {
        return;
    }
    let right = x.saturating_add(w - 1).min(image.width().saturating_sub(1));
    let bottom = y
        .saturating_add(h - 1)
        .min(image.height().saturating_sub(1));
    for px in x..=right {
        image.put_pixel(px, y, color);
        image.put_pixel(px, bottom, color);
    }
    for py in y..=bottom {
        image.put_pixel(x, py, color);
        image.put_pixel(right, py, color);
    }
}

fn body_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Regular,
        font_px: 21.0,
        line_height: 31,
        color: rgba(224, 230, 239),
    }
}

fn list_marker_style(ordered: bool) -> TextStyle {
    TextStyle {
        font_role: FontRole::Bold,
        font_px: if ordered { 21.0 } else { 24.0 },
        line_height: 31,
        color: rgba(214, 222, 235),
    }
}

fn muted_body_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Regular,
        font_px: 20.0,
        line_height: 30,
        color: rgba(197, 205, 219),
    }
}

fn heading_style(level: u8) -> TextStyle {
    match level {
        1 => TextStyle {
            font_role: FontRole::Bold,
            font_px: 34.0,
            line_height: 44,
            color: rgba(243, 247, 252),
        },
        2 => TextStyle {
            font_role: FontRole::Bold,
            font_px: 28.0,
            line_height: 38,
            color: rgba(237, 242, 250),
        },
        _ => TextStyle {
            font_role: FontRole::Bold,
            font_px: 24.0,
            line_height: 34,
            color: rgba(230, 237, 248),
        },
    }
}

fn quote_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Regular,
        font_px: 20.0,
        line_height: 30,
        color: rgba(206, 220, 239),
    }
}

fn table_header_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Bold,
        font_px: 18.0,
        line_height: 27,
        color: rgba(233, 238, 245),
    }
}

fn table_body_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Regular,
        font_px: 18.0,
        line_height: 27,
        color: rgba(219, 226, 236),
    }
}

fn code_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Mono,
        font_px: 18.0,
        line_height: 25,
        color: rgba(222, 226, 235),
    }
}

fn code_label_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Bold,
        font_px: 13.0,
        line_height: 18,
        color: rgba(120, 198, 255),
    }
}

fn meta_style() -> TextStyle {
    TextStyle {
        font_role: FontRole::Regular,
        font_px: 14.0,
        line_height: 20,
        color: rgba(151, 160, 177),
    }
}

fn rgba(r: u8, g: u8, b: u8) -> Rgba<u8> {
    Rgba([r, g, b, 255])
}

fn lerp_rgba(start: Rgba<u8>, end: Rgba<u8>, t: f32) -> Rgba<u8> {
    let clamped = t.clamp(0.0, 1.0);
    let lerp = |from: u8, to: u8| -> u8 {
        let from = from as f32;
        let to = to as f32;
        (from + (to - from) * clamped).round().clamp(0.0, 255.0) as u8
    };
    Rgba([
        lerp(start[0], end[0]),
        lerp(start[1], end[1]),
        lerp(start[2], end[2]),
        255,
    ])
}

fn build_render_output_path(render_dir: &Path, text: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    let hash = sha1_hex(text);
    render_dir.join(format!("reply-{millis}-{}.png", &hash[..10]))
}

async fn cleanup_old_images(dir: &Path) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    let now = SystemTime::now();
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|item| item.to_str()) != Some("png") {
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        files.push((path, modified));
    }
    files.sort_by(|left, right| right.1.cmp(&left.1));
    for (index, (path, modified)) in files.into_iter().enumerate() {
        let expired = now
            .duration_since(modified)
            .map(|age| age.as_secs() > MAX_RENDER_AGE_SECS)
            .unwrap_or(false);
        if !expired && index < KEEP_RENDER_FILES {
            continue;
        }
        let _ = fs::remove_file(path).await;
    }
}

fn reply_font_family() -> Option<&'static ReplyFontFamily> {
    REPLY_FONT_FAMILY
        .get_or_init(load_reply_font_family)
        .as_ref()
}

fn load_reply_font_family() -> Option<ReplyFontFamily> {
    let regular = load_font_from_candidates(&[
        FontCandidate::new("C:\\Windows\\Fonts\\msyh.ttc", 0),
        FontCandidate::new("C:\\Windows\\Fonts\\simsun.ttc", 0),
        FontCandidate::new("C:\\Windows\\Fonts\\simhei.ttf", 0),
        FontCandidate::new("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    ])?;
    let bold = load_font_from_candidates(&[
        FontCandidate::new("C:\\Windows\\Fonts\\msyhbd.ttc", 0),
        FontCandidate::new("C:\\Windows\\Fonts\\simhei.ttf", 0),
        FontCandidate::new("C:\\Windows\\Fonts\\seguisb.ttf", 0),
        FontCandidate::new("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/noto/NotoSansCJK-Bold.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 0),
        FontCandidate::new("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 0),
    ])
    .unwrap_or_else(|| regular.clone());
    let mono = load_font_from_candidates(&[
        FontCandidate::new("C:\\Windows\\Fonts\\consola.ttf", 0),
        FontCandidate::new("C:\\Windows\\Fonts\\msyh.ttc", 0),
        FontCandidate::new("./assets/fonts/FiraCode/FiraCode-Regular.ttf", 0),
        FontCandidate::new("./assets/fonts/firacode/FiraCode-Regular.ttf", 0),
        FontCandidate::new("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0),
        FontCandidate::new("/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf", 0),
    ])
    .unwrap_or_else(|| regular.clone());
    Some(ReplyFontFamily {
        regular,
        bold,
        mono,
    })
}

#[derive(Clone, Copy)]
struct FontCandidate {
    path: &'static str,
    index: u32,
}

impl FontCandidate {
    const fn new(path: &'static str, index: u32) -> Self {
        Self { path, index }
    }
}

fn load_font_from_candidates(candidates: &[FontCandidate]) -> Option<FontArc> {
    for candidate in candidates {
        if let Some(font) = load_font(candidate.path, candidate.index) {
            return Some(font);
        }
    }
    None
}

fn load_font(path: &str, index: u32) -> Option<FontArc> {
    let bytes = std::fs::read(path).ok()?;
    let font = FontVec::try_from_vec_and_index(bytes, index).ok()?;
    Some(FontArc::new(font))
}

fn font_for_role(fonts: &ReplyFontFamily, role: FontRole) -> &FontArc {
    match role {
        FontRole::Regular => &fonts.regular,
        FontRole::Bold => &fonts.bold,
        FontRole::Mono => &fonts.mono,
    }
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return false;
    }
    let normalized = trimmed
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "---" | "----" | "-----" | "***" | "****" | "*****" | "___" | "____" | "_____"
    )
}

fn detect_code_language(language: &str, code: &str) -> CodeLanguage {
    let normalized = language.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "rs" | "rust" => CodeLanguage::Rust,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "javascript" | "typescript" => {
            CodeLanguage::JavaScript
        }
        "sh" | "bash" | "shell" | "zsh" | "ps1" | "powershell" | "cmd" | "bat" => {
            CodeLanguage::Shell
        }
        "json" | "jsonc" => CodeLanguage::Json,
        "py" | "python" => CodeLanguage::Python,
        "yaml" | "yml" => CodeLanguage::Yaml,
        "toml" => CodeLanguage::Toml,
        "sql" => CodeLanguage::Sql,
        _ => detect_code_language_from_source(code),
    }
}

fn detect_code_language_from_source(code: &str) -> CodeLanguage {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return CodeLanguage::Generic;
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return CodeLanguage::Json;
    }
    if trimmed.starts_with("#!/") || trimmed.contains("echo ") || trimmed.contains("fi") {
        return CodeLanguage::Shell;
    }
    if trimmed.contains("fn ") || trimmed.contains("let ") || trimmed.contains("impl ") {
        return CodeLanguage::Rust;
    }
    if trimmed.contains("const ")
        || trimmed.contains("function ")
        || trimmed.contains("=>")
        || trimmed.contains("import ")
    {
        return CodeLanguage::JavaScript;
    }
    if trimmed.contains("def ") || trimmed.contains("import ") || trimmed.contains("print(") {
        return CodeLanguage::Python;
    }
    if trimmed.contains('=') && trimmed.contains('[') {
        return CodeLanguage::Toml;
    }
    CodeLanguage::Generic
}

fn highlight_code_line(
    line: &str,
    language: CodeLanguage,
    default_color: Rgba<u8>,
) -> Vec<CodeSpan> {
    if line.is_empty() {
        return vec![CodeSpan {
            text: String::new(),
            color: default_color,
        }];
    }

    let chars = line.chars().collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if line_comment_prefix_len(&chars, index, language).is_some() {
            spans.push(CodeSpan {
                text: chars[index..].iter().collect(),
                color: rgba(106, 153, 85),
            });
            break;
        }

        let ch = chars[index];
        if is_code_string_delimiter(ch, language) {
            let end = consume_string_literal(&chars, index, ch);
            spans.push(CodeSpan {
                text: chars[index..end].iter().collect(),
                color: rgba(206, 145, 120),
            });
            index = end;
            continue;
        }

        if ch.is_ascii_whitespace() {
            let start = index;
            while index < chars.len() && chars[index].is_ascii_whitespace() {
                index += 1;
            }
            spans.push(CodeSpan {
                text: chars[start..index].iter().collect(),
                color: default_color,
            });
            continue;
        }

        if ch.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_hexdigit()
                    || matches!(chars[index], '_' | '.' | 'x' | 'o' | 'b'))
            {
                index += 1;
            }
            spans.push(CodeSpan {
                text: chars[start..index].iter().collect(),
                color: rgba(181, 206, 168),
            });
            continue;
        }

        if is_code_identifier_start(ch, language) {
            let start = index;
            index += 1;
            while index < chars.len() && is_code_identifier_char(chars[index], language) {
                index += 1;
            }
            if language == CodeLanguage::Rust && index < chars.len() && chars[index] == '!' {
                index += 1;
            }
            let token = chars[start..index].iter().collect::<String>();
            let color = classify_code_identifier(&token, &chars, index, language, default_color);
            spans.push(CodeSpan { text: token, color });
            continue;
        }

        let color = if matches!(ch, '{' | '}' | '[' | ']' | '(' | ')') {
            rgba(197, 134, 192)
        } else if matches!(
            ch,
            '+' | '-' | '*' | '/' | '%' | '=' | '!' | '<' | '>' | '&' | '|' | '^' | ':' | '?'
        ) {
            rgba(212, 212, 212)
        } else {
            default_color
        };
        spans.push(CodeSpan {
            text: ch.to_string(),
            color,
        });
        index += 1;
    }

    merge_code_spans(spans)
}

fn merge_code_spans(spans: Vec<CodeSpan>) -> Vec<CodeSpan> {
    let mut merged: Vec<CodeSpan> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut()
            && last.color == span.color
        {
            last.text.push_str(&span.text);
            continue;
        }
        merged.push(span);
    }
    merged
}

fn line_comment_prefix_len(chars: &[char], index: usize, language: CodeLanguage) -> Option<usize> {
    let next = chars.get(index + 1).copied().unwrap_or('\0');
    match language {
        CodeLanguage::Rust | CodeLanguage::JavaScript | CodeLanguage::Generic => {
            (chars[index] == '/' && next == '/').then_some(2)
        }
        CodeLanguage::Shell | CodeLanguage::Python | CodeLanguage::Yaml | CodeLanguage::Toml => {
            (chars[index] == '#').then_some(1)
        }
        CodeLanguage::Sql => (chars[index] == '-' && next == '-').then_some(2),
        CodeLanguage::Json => None,
    }
}

fn is_code_string_delimiter(ch: char, language: CodeLanguage) -> bool {
    matches!(ch, '"' | '\'') || (language == CodeLanguage::JavaScript && ch == '`')
}

fn consume_string_literal(chars: &[char], start: usize, delimiter: char) -> usize {
    let mut index = start + 1;
    let mut escaped = false;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        index += 1;
        if ch == delimiter {
            break;
        }
    }
    index
}

fn is_code_identifier_start(ch: char, language: CodeLanguage) -> bool {
    ch == '_' || ch.is_ascii_alphabetic() || (language == CodeLanguage::Shell && ch == '$')
}

fn is_code_identifier_char(ch: char, language: CodeLanguage) -> bool {
    ch == '_'
        || ch.is_ascii_alphanumeric()
        || matches!(ch, '-' | '?' | '$')
        || (language == CodeLanguage::Rust && ch == '\'')
}

fn classify_code_identifier(
    token: &str,
    chars: &[char],
    end_index: usize,
    language: CodeLanguage,
    default_color: Rgba<u8>,
) -> Rgba<u8> {
    if is_code_keyword(token, language) {
        return rgba(86, 156, 214);
    }
    if matches!(
        token,
        "true" | "false" | "null" | "None" | "none" | "Some" | "Ok" | "Err"
    ) {
        return rgba(86, 156, 214);
    }
    if token.ends_with('!') {
        return rgba(220, 220, 170);
    }
    if token.chars().next().is_some_and(|ch| ch.is_uppercase()) {
        return rgba(78, 201, 176);
    }
    if next_non_space_char(chars, end_index) == Some('(') {
        return rgba(220, 220, 170);
    }
    default_color
}

fn next_non_space_char(chars: &[char], mut index: usize) -> Option<char> {
    while index < chars.len() {
        if !chars[index].is_whitespace() {
            return Some(chars[index]);
        }
        index += 1;
    }
    None
}

fn is_code_keyword(token: &str, language: CodeLanguage) -> bool {
    match language {
        CodeLanguage::Rust => matches!(
            token,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "else"
                | "enum"
                | "extern"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "trait"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        CodeLanguage::JavaScript => matches!(
            token,
            "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "delete"
                | "else"
                | "export"
                | "extends"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "if"
                | "import"
                | "in"
                | "instanceof"
                | "let"
                | "new"
                | "of"
                | "return"
                | "switch"
                | "throw"
                | "try"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "yield"
        ),
        CodeLanguage::Shell => matches!(
            token,
            "case"
                | "do"
                | "done"
                | "echo"
                | "elif"
                | "else"
                | "esac"
                | "exit"
                | "export"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "local"
                | "read"
                | "return"
                | "set"
                | "then"
                | "while"
        ),
        CodeLanguage::Json => false,
        CodeLanguage::Python => matches!(
            token,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "class"
                | "def"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        CodeLanguage::Yaml => false,
        CodeLanguage::Toml => false,
        CodeLanguage::Sql => matches!(
            token.to_ascii_uppercase().as_str(),
            "SELECT"
                | "FROM"
                | "WHERE"
                | "GROUP"
                | "BY"
                | "ORDER"
                | "JOIN"
                | "LEFT"
                | "RIGHT"
                | "INNER"
                | "OUTER"
                | "LIMIT"
                | "INSERT"
                | "UPDATE"
                | "DELETE"
                | "VALUES"
                | "SET"
                | "AS"
                | "ON"
                | "AND"
                | "OR"
        ),
        CodeLanguage::Generic => matches!(
            token,
            "class"
                | "const"
                | "def"
                | "else"
                | "enum"
                | "fn"
                | "for"
                | "function"
                | "if"
                | "impl"
                | "import"
                | "let"
                | "match"
                | "pub"
                | "return"
                | "struct"
                | "use"
                | "while"
        ),
    }
}

fn looks_like_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|') && !trimmed.starts_with("```") && !trimmed.starts_with("~~~")
}

fn looks_like_table_separator(line: &str) -> bool {
    parse_table_separator(line).is_some()
}

#[cfg(test)]
mod tests {
    use super::{Block, parse_markdown_blocks, render_reply_markdown_image, sanitize_reply_text};

    #[test]
    fn sanitize_reply_text_strips_tool_blocks() {
        let raw = "hello\n<<<CAIN_CODEX_TOOL_START>>>secret<<<CAIN_CODEX_TOOL_END>>>\nworld";
        assert_eq!(sanitize_reply_text(raw), "hello\n\nworld");
    }

    #[test]
    fn parse_markdown_blocks_keeps_major_block_types() {
        let raw = "# title\n\n- one\n- two\n\n> quote\n\n```rs\nfn main() {}\n```";
        let blocks = parse_markdown_blocks(raw);
        assert!(blocks.len() >= 4, "expected heading/list/quote/code");
    }

    #[test]
    fn parse_markdown_blocks_parses_tables() {
        let raw = "| 名称 | 数值 |\n| :--- | ---: |\n| CPU | 92% |\n| 内存 | 7.8 GiB |";
        let blocks = parse_markdown_blocks(raw);
        assert!(
            matches!(
                blocks.first(),
                Some(Block::Table { headers, rows, .. }) if headers.len() == 2 && rows.len() == 2
            ),
            "expected first block to be a parsed table"
        );
    }

    #[tokio::test]
    async fn renders_png_when_font_is_available() {
        if super::reply_font_family().is_none() {
            return;
        }
        let path = render_reply_markdown_image("# Title\n\n你好，`world`。\n\n- item 1\n- item 2")
            .await
            .expect("render ok")
            .expect("png path");
        let metadata = tokio::fs::metadata(&path).await.expect("png exists");
        assert!(metadata.len() > 0, "png should not be empty");
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    #[ignore = "manual snapshot probe"]
    async fn render_probe_outputs_paths() {
        if super::reply_font_family().is_none() {
            return;
        }

        let cases = [
            (
                "code",
                "# Syntax Highlight\n\n```rust\nfn main() {\n    let answer = 42;\n    println!(\"value = {}\", answer);\n    // comment line\n}\n```",
            ),
            (
                "table",
                "# Table\n\n| 项目 | 状态 | 备注 |\n| :--- | :---: | ---: |\n| CPU | Busy | 92% |\n| 内存 | Stable | 7.8 GiB |\n| 网络 | Burst | 12.4 MB/s |",
            ),
            (
                "ordered-list",
                "# Ordered List\n\n1. 第一个步骤，文本稍微长一点，确认换行后的悬挂缩进是否正确。\n2. 第二个步骤继续检查 marker 是否右对齐。\n10. 第十个步骤用于确认双位数序号不会把正文挤歪。",
            ),
        ];

        for (name, markdown) in cases {
            let path = render_reply_markdown_image(markdown)
                .await
                .expect("render ok")
                .expect("png path");
            println!("{name}: {path}");
        }
    }
}
