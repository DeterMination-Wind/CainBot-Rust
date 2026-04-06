use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Local;
use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{Rgba, RgbaImage};
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;

const CANVAS_WIDTH: u32 = 1180;
const CANVAS_HEIGHT: u32 = 860;
const STATUS_CACHE_TTL_SECS: u64 = 12 * 60 * 60;

#[derive(Debug, Clone)]
struct ProcessUsage {
    pid: String,
    name: String,
    rss_mb: f64,
}

#[derive(Debug, Clone)]
struct StatusSnapshot {
    hostname: String,
    kernel: String,
    cpu_percent: f64,
    memory_percent: f64,
    memory_used_gib: f64,
    memory_total_gib: f64,
    disk_percent: f64,
    disk_used_gib: f64,
    disk_total_gib: f64,
    net_rx_mbps: f64,
    net_tx_mbps: f64,
    uptime_secs: u64,
    load_avg: (f64, f64, f64),
    generated_at: String,
    top_processes: Vec<ProcessUsage>,
}

pub async fn create_status_dashboard_image() -> Result<PathBuf> {
    let snapshot = collect_status_snapshot().await;
    let output_dir = std::env::temp_dir().join("cain-status");
    fs::create_dir_all(&output_dir)
        .await
        .with_context(|| format!("创建状态图目录失败: {}", output_dir.display()))?;
    cleanup_old_status_images(&output_dir).await;

    let file_name = format!(
        "status-{}-{}.png",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default()
    );
    let output_path = output_dir.join(file_name);
    render_status_dashboard(&output_path, &snapshot)?;
    Ok(output_path)
}

async fn cleanup_old_status_images(dir: &Path) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    let now = SystemTime::now();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("png") {
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let Ok(modified_at) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified_at) else {
            continue;
        };
        if age.as_secs() > STATUS_CACHE_TTL_SECS {
            let _ = fs::remove_file(path).await;
        }
    }
}

async fn collect_status_snapshot() -> StatusSnapshot {
    let hostname = read_hostname().await.unwrap_or_else(|| "unknown-host".to_string());
    let kernel = read_kernel_text().await.unwrap_or_else(|| "Linux".to_string());
    let cpu_percent = sample_cpu_usage_percent().await.unwrap_or(0.0);
    let (memory_used_gib, memory_total_gib, memory_percent) = read_memory_usage().await.unwrap_or((0.0, 0.0, 0.0));
    let (disk_used_gib, disk_total_gib, disk_percent) = read_disk_usage().await.unwrap_or((0.0, 0.0, 0.0));
    let (net_rx_mbps, net_tx_mbps) = sample_network_mbps().await.unwrap_or((0.0, 0.0));
    let uptime_secs = read_uptime_secs().await.unwrap_or(0);
    let load_avg = read_load_average().await.unwrap_or((0.0, 0.0, 0.0));
    let top_processes = read_top_processes().await.unwrap_or_default();

    StatusSnapshot {
        hostname,
        kernel,
        cpu_percent,
        memory_percent,
        memory_used_gib,
        memory_total_gib,
        disk_percent,
        disk_used_gib,
        disk_total_gib,
        net_rx_mbps,
        net_tx_mbps,
        uptime_secs,
        load_avg,
        generated_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        top_processes,
    }
}

fn render_status_dashboard(output_path: &Path, snapshot: &StatusSnapshot) -> Result<()> {
    let mut image = RgbaImage::from_pixel(CANVAS_WIDTH, CANVAS_HEIGHT, rgba(240, 243, 247));

    // Main frame
    draw_filled_rect(&mut image, 16, 16, CANVAS_WIDTH - 32, CANVAS_HEIGHT - 32, rgba(255, 255, 255));
    draw_rect_border(&mut image, 16, 16, CANVAS_WIDTH - 32, CANVAS_HEIGHT - 32, rgba(220, 226, 234));

    // Header
    draw_text(&mut image, 44, 42, 3, rgba(34, 45, 61), "CAIN STATUS");
    draw_text(
        &mut image,
        44,
        84,
        2,
        rgba(90, 107, 125),
        &format!("HOST {}  |  {}", snapshot.hostname, snapshot.kernel),
    );
    draw_text(
        &mut image,
        44,
        108,
        2,
        rgba(117, 132, 148),
        &format!("UPDATED {}", snapshot.generated_at),
    );

    // Metric cards
    let card_w = 350;
    let card_h = 150;
    let row1_y = 150;
    let row2_y = 320;
    let col1_x = 40;
    let col2_x = col1_x + card_w + 20;
    let col3_x = col2_x + card_w + 20;

    draw_metric_card(
        &mut image,
        col1_x,
        row1_y,
        card_w,
        card_h,
        "CPU",
        &format!("{:.1}%", snapshot.cpu_percent.max(0.0)),
        "current usage",
    );
    draw_metric_card(
        &mut image,
        col2_x,
        row1_y,
        card_w,
        card_h,
        "MEMORY",
        &format!("{:.1}%", snapshot.memory_percent.max(0.0)),
        &format!("{:.2} / {:.2} GiB", snapshot.memory_used_gib, snapshot.memory_total_gib),
    );
    draw_metric_card(
        &mut image,
        col3_x,
        row1_y,
        card_w,
        card_h,
        "DISK",
        &format!("{:.1}%", snapshot.disk_percent.max(0.0)),
        &format!("{:.1} / {:.1} GiB", snapshot.disk_used_gib, snapshot.disk_total_gib),
    );
    draw_metric_card(
        &mut image,
        col1_x,
        row2_y,
        card_w,
        card_h,
        "NET RX",
        &format!("{:.2} Mbps", snapshot.net_rx_mbps.max(0.0)),
        "downstream",
    );
    draw_metric_card(
        &mut image,
        col2_x,
        row2_y,
        card_w,
        card_h,
        "NET TX",
        &format!("{:.2} Mbps", snapshot.net_tx_mbps.max(0.0)),
        "upstream",
    );
    draw_metric_card(
        &mut image,
        col3_x,
        row2_y,
        card_w,
        card_h,
        "UPTIME",
        &format_duration(snapshot.uptime_secs),
        &format!(
            "load {:.2} {:.2} {:.2}",
            snapshot.load_avg.0, snapshot.load_avg.1, snapshot.load_avg.2
        ),
    );

    // Process table
    let table_x = 40;
    let table_y = 500;
    let table_w = CANVAS_WIDTH - 80;
    let table_h = 320;
    draw_filled_rect(&mut image, table_x, table_y, table_w, table_h, rgba(250, 252, 255));
    draw_rect_border(&mut image, table_x, table_y, table_w, table_h, rgba(220, 226, 234));
    draw_text(&mut image, table_x + 20, table_y + 18, 2, rgba(56, 71, 89), "TOP PROCESSES");
    draw_text(&mut image, table_x + 20, table_y + 52, 2, rgba(104, 119, 137), "PID");
    draw_text(&mut image, table_x + 180, table_y + 52, 2, rgba(104, 119, 137), "NAME");
    draw_text(&mut image, table_x + 700, table_y + 52, 2, rgba(104, 119, 137), "RSS");

    let mut row_y = table_y + 84;
    for item in snapshot.top_processes.iter().take(6) {
        draw_text(&mut image, table_x + 20, row_y, 2, rgba(56, 71, 89), &item.pid);
        draw_text(
            &mut image,
            table_x + 180,
            row_y,
            2,
            rgba(56, 71, 89),
            &truncate_ascii(&item.name, 32),
        );
        draw_text(
            &mut image,
            table_x + 700,
            row_y,
            2,
            rgba(56, 71, 89),
            &format!("{:.1} MiB", item.rss_mb.max(0.0)),
        );
        draw_horizontal_line(
            &mut image,
            table_x + 16,
            table_x + table_w - 16,
            row_y + 26,
            rgba(231, 236, 243),
        );
        row_y += 36;
    }

    image
        .save(output_path)
        .with_context(|| format!("写入状态图失败: {}", output_path.display()))?;
    Ok(())
}

fn draw_metric_card(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    title: &str,
    value: &str,
    subtitle: &str,
) {
    draw_filled_rect(image, x, y, w, h, rgba(250, 252, 255));
    draw_rect_border(image, x, y, w, h, rgba(220, 226, 234));
    draw_text(image, x + 18, y + 14, 2, rgba(100, 116, 133), title);
    draw_text(image, x + 18, y + 50, 3, rgba(31, 47, 63), value);
    draw_text(image, x + 18, y + 108, 2, rgba(121, 135, 151), subtitle);
}

fn draw_horizontal_line(image: &mut RgbaImage, start_x: u32, end_x: u32, y: u32, color: Rgba<u8>) {
    if y >= image.height() {
        return;
    }
    let safe_end = end_x.min(image.width().saturating_sub(1));
    for x in start_x..=safe_end {
        image.put_pixel(x, y, color);
    }
}

fn draw_text(image: &mut RgbaImage, x: u32, y: u32, scale: u32, color: Rgba<u8>, text: &str) {
    let mut cursor_x = x as i32;
    let mut cursor_y = y as i32;
    let step_x = (8 * scale + scale) as i32;
    let step_y = (10 * scale) as i32;

    for ch in text.chars() {
        if ch == '\n' {
            cursor_x = x as i32;
            cursor_y += step_y;
            continue;
        }
        let glyph = BASIC_FONTS
            .get(ch)
            .or_else(|| BASIC_FONTS.get(ch.to_ascii_uppercase()))
            .unwrap_or([0u8; 8]);
        for (row, row_bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if ((row_bits >> col) & 1) == 0 {
                    continue;
                }
                let px = cursor_x + ((7 - col) as i32 * scale as i32);
                let py = cursor_y + (row as i32 * scale as i32);
                draw_scaled_dot(image, px, py, scale, color);
            }
        }
        cursor_x += step_x;
    }
}

fn draw_scaled_dot(image: &mut RgbaImage, x: i32, y: i32, scale: u32, color: Rgba<u8>) {
    for dy in 0..scale as i32 {
        for dx in 0..scale as i32 {
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 {
                continue;
            }
            let px = px as u32;
            let py = py as u32;
            if px >= image.width() || py >= image.height() {
                continue;
            }
            image.put_pixel(px, py, color);
        }
    }
}

fn draw_filled_rect(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if w == 0 || h == 0 {
        return;
    }
    let end_x = (x + w).min(image.width());
    let end_y = (y + h).min(image.height());
    for py in y..end_y {
        for px in x..end_x {
            image.put_pixel(px, py, color);
        }
    }
}

fn draw_rect_border(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if w < 2 || h < 2 {
        return;
    }
    let right = (x + w - 1).min(image.width().saturating_sub(1));
    let bottom = (y + h - 1).min(image.height().saturating_sub(1));
    for px in x..=right {
        image.put_pixel(px, y, color);
        image.put_pixel(px, bottom, color);
    }
    for py in y..=bottom {
        image.put_pixel(x, py, color);
        image.put_pixel(right, py, color);
    }
}

fn rgba(r: u8, g: u8, b: u8) -> Rgba<u8> {
    Rgba([r, g, b, 255])
}

async fn read_hostname() -> Option<String> {
    if let Ok(value) = fs::read_to_string("/etc/hostname").await {
        let normalized = value.trim();
        if !normalized.is_empty() {
            return Some(normalized.to_string());
        }
    }
    if let Ok(value) = std::env::var("HOSTNAME") {
        let normalized = value.trim();
        if !normalized.is_empty() {
            return Some(normalized.to_string());
        }
    }
    let output = Command::new("hostname").output().await.ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

async fn read_kernel_text() -> Option<String> {
    let output = Command::new("uname").args(["-sr"]).output().await.ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

async fn sample_cpu_usage_percent() -> Option<f64> {
    let (total_1, idle_1) = read_cpu_ticks().await?;
    sleep(Duration::from_millis(250)).await;
    let (total_2, idle_2) = read_cpu_ticks().await?;
    let total_delta = total_2.saturating_sub(total_1) as f64;
    if total_delta <= 0.0 {
        return None;
    }
    let idle_delta = idle_2.saturating_sub(idle_1) as f64;
    Some(((total_delta - idle_delta) / total_delta * 100.0).clamp(0.0, 100.0))
}

async fn read_cpu_ticks() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat").await.ok()?;
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }
    let values = parts.filter_map(|item| item.parse::<u64>().ok()).collect::<Vec<_>>();
    if values.len() < 4 {
        return None;
    }
    let idle = values.get(3).copied().unwrap_or_default() + values.get(4).copied().unwrap_or_default();
    let total = values.iter().sum::<u64>();
    Some((total, idle))
}

async fn read_memory_usage() -> Option<(f64, f64, f64)> {
    let text = fs::read_to_string("/proc/meminfo").await.ok()?;
    let mut total_kib = 0f64;
    let mut available_kib = 0f64;
    for line in text.lines() {
        if line.starts_with("MemTotal:") {
            total_kib = line.split_whitespace().nth(1)?.parse::<f64>().ok()?;
        } else if line.starts_with("MemAvailable:") {
            available_kib = line.split_whitespace().nth(1)?.parse::<f64>().ok()?;
        }
    }
    if total_kib <= 0.0 {
        return None;
    }
    let used_kib = (total_kib - available_kib).max(0.0);
    let used_gib = used_kib / 1024.0 / 1024.0;
    let total_gib = total_kib / 1024.0 / 1024.0;
    let percent = (used_kib / total_kib * 100.0).clamp(0.0, 100.0);
    Some((used_gib, total_gib, percent))
}

async fn read_disk_usage() -> Option<(f64, f64, f64)> {
    let output = Command::new("df").args(["-Pk", "/"]).output().await.ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().nth(1)?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 5 {
        return None;
    }
    let total_kib = fields.get(1)?.parse::<f64>().ok()?;
    let used_kib = fields.get(2)?.parse::<f64>().ok()?;
    let percent_text = fields.get(4)?.trim_end_matches('%');
    let percent = percent_text.parse::<f64>().ok()?;
    let used_gib = used_kib / 1024.0 / 1024.0;
    let total_gib = total_kib / 1024.0 / 1024.0;
    Some((used_gib, total_gib, percent.clamp(0.0, 100.0)))
}

async fn sample_network_mbps() -> Option<(f64, f64)> {
    let (rx_1, tx_1) = read_network_bytes().await?;
    let started = SystemTime::now();
    sleep(Duration::from_millis(350)).await;
    let elapsed = started.elapsed().ok()?.as_secs_f64();
    if elapsed <= 0.0 {
        return None;
    }
    let (rx_2, tx_2) = read_network_bytes().await?;
    let rx_mbps = (rx_2.saturating_sub(rx_1) as f64 * 8.0 / elapsed / 1_000_000.0).max(0.0);
    let tx_mbps = (tx_2.saturating_sub(tx_1) as f64 * 8.0 / elapsed / 1_000_000.0).max(0.0);
    Some((rx_mbps, tx_mbps))
}

async fn read_network_bytes() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/net/dev").await.ok()?;
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;
    for line in text.lines().skip(2) {
        let (iface, payload) = line.split_once(':')?;
        let iface = iface.trim();
        if iface == "lo" || iface.is_empty() {
            continue;
        }
        let columns = payload.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 16 {
            continue;
        }
        rx_total = rx_total.saturating_add(columns[0].parse::<u64>().ok()?);
        tx_total = tx_total.saturating_add(columns[8].parse::<u64>().ok()?);
    }
    Some((rx_total, tx_total))
}

async fn read_uptime_secs() -> Option<u64> {
    let text = fs::read_to_string("/proc/uptime").await.ok()?;
    let value = text.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(value.max(0.0) as u64)
}

async fn read_load_average() -> Option<(f64, f64, f64)> {
    let text = fs::read_to_string("/proc/loadavg").await.ok()?;
    let mut parts = text.split_whitespace();
    Some((
        parts.next()?.parse::<f64>().ok()?,
        parts.next()?.parse::<f64>().ok()?,
        parts.next()?.parse::<f64>().ok()?,
    ))
}

async fn read_top_processes() -> Option<Vec<ProcessUsage>> {
    let output = Command::new("ps")
        .args(["-eo", "pid,comm,rss", "--sort=-rss", "--no-headers"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in text.lines().take(6) {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 3 {
            continue;
        }
        let pid = parts[0].to_string();
        let name = parts[1].to_string();
        let rss_kib = parts[2].parse::<f64>().unwrap_or(0.0);
        rows.push(ProcessUsage {
            pid,
            name,
            rss_mb: rss_kib / 1024.0,
        });
    }
    Some(rows)
}

fn truncate_ascii(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return input.to_string();
    }
    chars.truncate(max_chars.saturating_sub(3));
    format!("{}...", chars.into_iter().collect::<String>())
}

fn format_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let mins = (total_secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::create_status_dashboard_image;

    #[tokio::test]
    async fn creates_status_dashboard_png() {
        let output = create_status_dashboard_image().await.expect("status image");
        let metadata = tokio::fs::metadata(&output).await.expect("status image metadata");
        assert!(metadata.len() > 0, "status image should not be empty");
    }
}
