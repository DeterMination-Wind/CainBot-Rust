use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use chrono::Local;
use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;

const CANVAS_WIDTH: u32 = 1180;
const MIN_CANVAS_HEIGHT: u32 = 900;
const BTOP_IMAGE_SIZE: u32 = 1280;
const BTOP_SESSION_NAME: &str = "cainbot_btop";
const BTOP_WINDOW_WIDTH: u32 = 182;
const BTOP_WINDOW_HEIGHT: u32 = 91;
const BTOP_X11_DISPLAY: &str = ":97";
const BTOP_X11_SOCKET_PATH: &str = "/tmp/.X11-unix/X97";
const BTOP_XTERM_GEOMETRY: &str = "182x91+0+0";
const BTOP_XTERM_TITLE: &str = "cainbot-btop-live";
const BTOP_XTERM_PROCESS_PATTERN: &str =
    "xterm.*cainbot-btop-live.*-geometry 182x91\\+0\\+0.*-fs 8";
const BTOP_XTERM_ANY_PROCESS_PATTERN: &str = "xterm.*cainbot-btop-live";
const BTOP_XTERM_LEGACY_PROCESS_PATTERN: &str = "xterm.*:97.*cainbot_btop";
const STATUS_CACHE_TTL_SECS: u64 = 12 * 60 * 60;
const STATUS_TREND_WINDOW_SECS: i64 = 30 * 60;
const STATUS_HISTORY_MAX_POINTS: usize = 900;

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
    captured_at_ms: i64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrendPoint {
    timestamp_ms: i64,
    cpu_percent: f64,
    memory_percent: f64,
    net_rx_mbps: f64,
    net_tx_mbps: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrendHistory {
    points: Vec<TrendPoint>,
}

#[derive(Clone)]
struct FontFamily {
    regular: FontArc,
    semibold: FontArc,
    bold: FontArc,
}

#[derive(Clone, Copy)]
enum TextWeight {
    Regular,
    SemiBold,
    Bold,
}

static STATUS_FONT_FAMILY: OnceLock<Option<FontFamily>> = OnceLock::new();

pub async fn create_status_dashboard_image() -> Result<PathBuf> {
    let output_dir = std::env::temp_dir().join("cain-status");
    fs::create_dir_all(&output_dir)
        .await
        .with_context(|| format!("创建状态图目录失败: {}", output_dir.display()))?;
    cleanup_old_status_images(&output_dir).await;

    if cfg!(target_os = "linux") {
        return capture_btop_dashboard_image(&output_dir).await;
    }

    let snapshot = collect_status_snapshot().await;
    let history_path = output_dir.join("trend-history.json");
    let trend_points = update_trend_history(&history_path, &snapshot).await;

    let file_name = format!(
        "status-{}-{}.png",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default()
    );
    let output_path = output_dir.join(file_name);
    render_status_dashboard(&output_path, &snapshot, &trend_points)?;
    Ok(output_path)
}

pub async fn ensure_btop_dashboard_runtime() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    ensure_btop_tmux_session().await?;
    ensure_btop_xvfb_server().await?;
    ensure_btop_xterm_client().await
}

async fn capture_btop_dashboard_image(output_dir: &Path) -> Result<PathBuf> {
    ensure_btop_dashboard_runtime().await?;
    let output_path = build_status_output_path(output_dir, "btop");
    capture_btop_x11_screenshot(&output_path).await?;
    Ok(output_path)
}

async fn ensure_linux_command_exists(command: &str) -> Result<()> {
    let output = Command::new("sh")
        .args(["-lc", &format!("command -v {command} >/dev/null 2>&1")])
        .output()
        .await
        .with_context(|| format!("检测命令失败: {command}"))?;
    if output.status.success() {
        return Ok(());
    }
    anyhow::bail!("缺少命令：{command}");
}

async fn pgrep_first_pid(pattern: &str) -> Result<Option<u32>> {
    let output = Command::new("pgrep")
        .args(["-o", "-f", pattern])
        .output()
        .await
        .context("执行 pgrep 失败")?;
    if !output.status.success() {
        return Ok(None);
    }
    let pid = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<u32>().ok());
    Ok(pid)
}

async fn ensure_btop_xvfb_server() -> Result<()> {
    ensure_linux_command_exists("Xvfb").await?;
    ensure_linux_command_exists("pgrep").await?;

    let has_xvfb = pgrep_first_pid(&format!("Xvfb\\s+{}", BTOP_X11_DISPLAY)).await?;
    if has_xvfb.is_some() && Path::new(BTOP_X11_SOCKET_PATH).exists() {
        return Ok(());
    }

    Command::new("Xvfb")
        .args([
            BTOP_X11_DISPLAY,
            "-screen",
            "0",
            &format!("{0}x{0}x24", BTOP_IMAGE_SIZE),
            "-nolisten",
            "tcp",
            "-ac",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 Xvfb 失败")?;

    for _ in 0..30 {
        if Path::new(BTOP_X11_SOCKET_PATH).exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Xvfb 未就绪：{}", BTOP_X11_SOCKET_PATH);
}

async fn ensure_btop_xterm_client() -> Result<()> {
    ensure_linux_command_exists("xterm").await?;
    ensure_linux_command_exists("pgrep").await?;
    ensure_linux_command_exists("pkill").await?;

    if pgrep_first_pid(BTOP_XTERM_PROCESS_PATTERN).await?.is_some() {
        return Ok(());
    }

    let _ = Command::new("pkill")
        .args(["-f", "--", BTOP_XTERM_ANY_PROCESS_PATTERN])
        .output()
        .await;
    let _ = Command::new("pkill")
        .args(["-f", "--", BTOP_XTERM_LEGACY_PROCESS_PATTERN])
        .output()
        .await;
    sleep(Duration::from_millis(260)).await;

    Command::new("xterm")
        .args([
            "-display",
            BTOP_X11_DISPLAY,
            "-title",
            BTOP_XTERM_TITLE,
            "-fullscreen",
            "-geometry",
            BTOP_XTERM_GEOMETRY,
            "+sb",
            "-fa",
            "Monospace",
            "-fs",
            "8",
            "-fg",
            "#cdd6f4",
            "-bg",
            "#0b1220",
            "-e",
            "tmux",
            "attach-session",
            "-t",
            BTOP_SESSION_NAME,
        ])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 btop xterm 客户端失败")?;

    sleep(Duration::from_millis(900)).await;
    if pgrep_first_pid(BTOP_XTERM_PROCESS_PATTERN).await?.is_none() {
        anyhow::bail!("xterm 已启动但未检测到常驻 btop 客户端");
    }
    Ok(())
}

async fn ensure_btop_tmux_session() -> Result<()> {
    let has = Command::new("tmux")
        .args(["has-session", "-t", BTOP_SESSION_NAME])
        .output()
        .await
        .context("执行 tmux has-session 失败")?;
    if !has.status.success() {
        let status_text = String::from_utf8_lossy(&has.stderr).trim().to_string();
        // tmux 在“还没有 server / session”时会返回非 0，并带上
        // `no server running on /tmp/tmux-*/default`。这里必须把它视为冷启动，
        // 否则后台维护和 `/status` 会在每次启动后都被这条假错误打断。
        if !status_text.is_empty() && !tmux_stderr_indicates_missing_session(&status_text) {
            anyhow::bail!("检测 btop tmux 会话失败: {status_text}");
        }
        let created = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                BTOP_SESSION_NAME,
                "-x",
                &BTOP_WINDOW_WIDTH.to_string(),
                "-y",
                &BTOP_WINDOW_HEIGHT.to_string(),
                "env",
                "TERM=xterm-256color",
                "btop",
            ])
            .output()
            .await
            .context("启动 btop tmux 会话失败")?;
        if !created.status.success() {
            let stderr = String::from_utf8_lossy(&created.stderr).trim().to_string();
            if status_text.is_empty() {
                anyhow::bail!("启动 btop tmux 会话失败: {stderr}");
            }
            anyhow::bail!("启动 btop tmux 会话失败: {stderr}（has-session stderr: {status_text}）");
        }
    }

    let pane_dead = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            &format!("{BTOP_SESSION_NAME}:0.0"),
            "#{pane_dead}",
        ])
        .output()
        .await
        .context("检查 btop pane 状态失败")?;
    if !pane_dead.status.success() {
        let stderr = String::from_utf8_lossy(&pane_dead.stderr)
            .trim()
            .to_string();
        anyhow::bail!("检查 btop pane 状态失败: {stderr}");
    }
    let dead_flag = String::from_utf8_lossy(&pane_dead.stdout)
        .trim()
        .to_string();
    if dead_flag == "1" {
        let respawned = Command::new("tmux")
            .args([
                "respawn-pane",
                "-k",
                "-t",
                &format!("{BTOP_SESSION_NAME}:0.0"),
                "env",
                "TERM=xterm-256color",
                "btop",
            ])
            .output()
            .await
            .context("重启 btop pane 失败")?;
        if !respawned.status.success() {
            let stderr = String::from_utf8_lossy(&respawned.stderr)
                .trim()
                .to_string();
            anyhow::bail!("重启 btop pane 失败: {stderr}");
        }
    }

    let resized = Command::new("tmux")
        .args([
            "resize-window",
            "-t",
            &format!("{BTOP_SESSION_NAME}:0"),
            "-x",
            &BTOP_WINDOW_WIDTH.to_string(),
            "-y",
            &BTOP_WINDOW_HEIGHT.to_string(),
        ])
        .output()
        .await
        .context("调整 btop tmux 窗口尺寸失败")?;
    if !resized.status.success() {
        let stderr = String::from_utf8_lossy(&resized.stderr).trim().to_string();
        anyhow::bail!("调整 btop tmux 窗口尺寸失败: {stderr}");
    }
    Ok(())
}

fn tmux_stderr_indicates_missing_session(stderr: &str) -> bool {
    let normalized = stderr.trim().to_ascii_lowercase();
    normalized.contains("can't find session") || normalized.contains("no server running on")
}

fn build_status_output_path(output_dir: &Path, kind: &str) -> PathBuf {
    let file_name = format!(
        "status-{}-{}-{}.png",
        kind,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default()
    );
    output_dir.join(file_name)
}

async fn capture_btop_x11_screenshot(output_path: &Path) -> Result<()> {
    ensure_linux_command_exists("python3").await?;
    let crop_rect = query_btop_xterm_window_rect().await?;
    let mut errors = Vec::new();

    if let Err(error) = run_mss_capture_script(output_path, crop_rect).await {
        errors.push(format!("窗口抓图失败: {error:#}"));
        if crop_rect.is_some() {
            if let Err(root_error) = run_mss_capture_script(output_path, None).await {
                errors.push(format!("根窗口回退抓图失败: {root_error:#}"));
            }
        }
    }

    if !output_path.exists() {
        if let Err(runtime_error) = ensure_btop_dashboard_runtime().await {
            errors.push(format!("重建 btop 运行时失败: {runtime_error:#}"));
        }
        sleep(Duration::from_millis(220)).await;
        if let Err(retry_error) = run_mss_capture_script(output_path, None).await {
            errors.push(format!("重建运行时后抓图仍失败: {retry_error:#}"));
            anyhow::bail!("执行 btop 截图失败：{}", errors.join(" | "));
        }
    }

    let loaded = image::open(output_path)
        .with_context(|| format!("读取 btop 截图失败: {}", output_path.display()))?
        .to_rgba8();
    if loaded.width() != BTOP_IMAGE_SIZE || loaded.height() != BTOP_IMAGE_SIZE {
        let resized = image::imageops::resize(
            &loaded,
            BTOP_IMAGE_SIZE,
            BTOP_IMAGE_SIZE,
            image::imageops::FilterType::CatmullRom,
        );
        resized
            .save(output_path)
            .with_context(|| format!("保存缩放后的 btop 截图失败: {}", output_path.display()))?;
    }
    Ok(())
}

async fn run_mss_capture_script(
    output_path: &Path,
    crop_rect: Option<(u32, u32, u32, u32)>,
) -> Result<()> {
    let output_string = output_path.to_string_lossy().to_string();
    let script = r#"import sys
import time
import mss
import mss.tools

display = sys.argv[1]
output = sys.argv[2]

requested = None
if len(sys.argv) >= 7:
    requested = {
        "left": int(sys.argv[3]),
        "top": int(sys.argv[4]),
        "width": int(sys.argv[5]),
        "height": int(sys.argv[6]),
    }

last_error = ""
for attempt in range(4):
    try:
        with mss.mss(display=display) as sct:
            root = sct.monitors[0]
            monitor = root
            if requested is not None:
                left = int(requested["left"])
                top = int(requested["top"])
                width = int(requested["width"])
                height = int(requested["height"])
                root_left = int(root.get("left", 0))
                root_top = int(root.get("top", 0))
                root_width = int(root.get("width", 0))
                root_height = int(root.get("height", 0))
                max_left = root_left + max(0, root_width - 1)
                max_top = root_top + max(0, root_height - 1)
                left = max(root_left, min(left, max_left))
                top = max(root_top, min(top, max_top))
                right = min(left + max(1, width), root_left + max(1, root_width))
                bottom = min(top + max(1, height), root_top + max(1, root_height))
                width = max(1, right - left)
                height = max(1, bottom - top)
                monitor = {
                    "left": left,
                    "top": top,
                    "width": width,
                    "height": height,
                }

            try:
                shot = sct.grab(monitor)
            except Exception:
                shot = sct.grab(root)
            mss.tools.to_png(shot.rgb, shot.size, output=output)
            sys.exit(0)
    except Exception as exc:
        last_error = f"attempt={attempt + 1}: {exc}"
        time.sleep(0.12)

print(last_error, file=sys.stderr)
sys.exit(2)
"#;

    let mut args = vec![
        "-c".to_string(),
        script.to_string(),
        BTOP_X11_DISPLAY.to_string(),
        output_string,
    ];
    if let Some((x, y, w, h)) = crop_rect {
        args.push(x.to_string());
        args.push(y.to_string());
        args.push(w.to_string());
        args.push(h.to_string());
    }

    let run = Command::new("python3")
        .args(args)
        .env("DISPLAY", BTOP_X11_DISPLAY)
        .output()
        .await
        .context("执行 btop 截图脚本失败")?;
    if !run.status.success() {
        let stderr = String::from_utf8_lossy(&run.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        anyhow::bail!("执行 btop 截图失败：{detail}");
    }
    Ok(())
}

async fn query_btop_xterm_window_rect() -> Result<Option<(u32, u32, u32, u32)>> {
    ensure_linux_command_exists("xwininfo").await?;
    let output = Command::new("xwininfo")
        .args(["-display", BTOP_X11_DISPLAY, "-name", BTOP_XTERM_TITLE])
        .output()
        .await
        .context("读取 xterm 窗口信息失败")?;
    if !output.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let x = parse_xwininfo_int(&raw, "Absolute upper-left X:")
        .unwrap_or(0)
        .max(0) as u32;
    let y = parse_xwininfo_int(&raw, "Absolute upper-left Y:")
        .unwrap_or(0)
        .max(0) as u32;
    let w = parse_xwininfo_int(&raw, "Width:")
        .unwrap_or(BTOP_IMAGE_SIZE as i32)
        .max(1) as u32;
    let h = parse_xwininfo_int(&raw, "Height:")
        .unwrap_or(BTOP_IMAGE_SIZE as i32)
        .max(1) as u32;
    Ok(Some((x, y, w, h)))
}

fn parse_xwininfo_int(raw: &str, key: &str) -> Option<i32> {
    raw.lines().find_map(|line| {
        let trimmed = line.trim_start();
        if !trimmed.starts_with(key) {
            return None;
        }
        trimmed
            .split_once(':')
            .and_then(|(_, value)| value.trim().parse::<i32>().ok())
    })
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

async fn load_trend_history(path: &Path) -> TrendHistory {
    let Ok(raw) = fs::read_to_string(path).await else {
        return TrendHistory::default();
    };
    serde_json::from_str::<TrendHistory>(&raw).unwrap_or_default()
}

async fn update_trend_history(path: &Path, snapshot: &StatusSnapshot) -> Vec<TrendPoint> {
    let mut history = load_trend_history(path).await;
    history.points.push(TrendPoint {
        timestamp_ms: snapshot.captured_at_ms,
        cpu_percent: snapshot.cpu_percent,
        memory_percent: snapshot.memory_percent,
        net_rx_mbps: snapshot.net_rx_mbps,
        net_tx_mbps: snapshot.net_tx_mbps,
    });

    let window_start = snapshot
        .captured_at_ms
        .saturating_sub(STATUS_TREND_WINDOW_SECS.saturating_mul(1_000));
    history.points.retain(|point| {
        point.timestamp_ms >= window_start && point.timestamp_ms <= snapshot.captured_at_ms + 3_000
    });
    if history.points.len() > STATUS_HISTORY_MAX_POINTS {
        let keep_from = history
            .points
            .len()
            .saturating_sub(STATUS_HISTORY_MAX_POINTS);
        history.points = history.points.split_off(keep_from);
    }
    history.points.sort_by_key(|point| point.timestamp_ms);

    if let Ok(raw) = serde_json::to_string(&history) {
        let _ = fs::write(path, raw).await;
    }
    history.points
}

async fn collect_status_snapshot() -> StatusSnapshot {
    let hostname = read_hostname()
        .await
        .unwrap_or_else(|| "unknown-host".to_string());
    let kernel = read_kernel_text()
        .await
        .unwrap_or_else(|| "Linux".to_string());
    let captured_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default();
    let cpu_percent = sample_cpu_usage_percent().await.unwrap_or(0.0);
    let (memory_used_gib, memory_total_gib, memory_percent) =
        read_memory_usage().await.unwrap_or((0.0, 0.0, 0.0));
    let (disk_used_gib, disk_total_gib, disk_percent) =
        read_disk_usage().await.unwrap_or((0.0, 0.0, 0.0));
    let (net_rx_mbps, net_tx_mbps) = sample_network_mbps().await.unwrap_or((0.0, 0.0));
    let uptime_secs = read_uptime_secs().await.unwrap_or(0);
    let load_avg = read_load_average().await.unwrap_or((0.0, 0.0, 0.0));
    let top_processes = read_top_processes().await.unwrap_or_default();

    StatusSnapshot {
        hostname,
        kernel,
        captured_at_ms,
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

fn render_status_dashboard(
    output_path: &Path,
    snapshot: &StatusSnapshot,
    trend_points: &[TrendPoint],
) -> Result<()> {
    let card_h = 138;
    let row1_y = 150;
    let chart_h = 140;
    let chart_gap = 16;
    let chart_1_y = row1_y + card_h + 20;
    let chart_2_y = chart_1_y + chart_h + chart_gap;
    let chart_3_y = chart_2_y + chart_h + chart_gap;
    let table_y = chart_3_y + chart_h + 20;
    let process_rows = snapshot.top_processes.len().clamp(2, 10) as u32;
    let table_h = 68 + process_rows * 34 + 14;
    let canvas_height = (table_y + table_h + 20).max(MIN_CANVAS_HEIGHT);

    let mut image = RgbaImage::from_pixel(CANVAS_WIDTH, canvas_height, rgba(240, 243, 247));

    // Main frame
    draw_filled_rect(
        &mut image,
        16,
        16,
        CANVAS_WIDTH - 32,
        canvas_height - 32,
        rgba(255, 255, 255),
    );
    draw_rect_border(
        &mut image,
        16,
        16,
        CANVAS_WIDTH - 32,
        canvas_height - 32,
        rgba(220, 226, 234),
    );

    // Header
    draw_text_weight(
        &mut image,
        44,
        42,
        3,
        TextWeight::Bold,
        rgba(34, 45, 61),
        "CAIN STATUS",
    );
    draw_text_weight(
        &mut image,
        44,
        84,
        2,
        TextWeight::SemiBold,
        rgba(90, 107, 125),
        &fit_text_to_width_weight(
            &format!("HOST {}  |  {}", snapshot.hostname, snapshot.kernel),
            2,
            TextWeight::SemiBold,
            CANVAS_WIDTH.saturating_sub(120),
        ),
    );
    draw_text_weight(
        &mut image,
        44,
        108,
        2,
        TextWeight::Regular,
        rgba(117, 132, 148),
        &fit_text_to_width_weight(
            &format!("UPDATED {}  |  TREND WINDOW 30M", snapshot.generated_at),
            2,
            TextWeight::Regular,
            CANVAS_WIDTH.saturating_sub(120),
        ),
    );

    // Metric cards (current)
    let card_w = 350;
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
        "current usage (instant)",
    );
    draw_metric_card(
        &mut image,
        col2_x,
        row1_y,
        card_w,
        card_h,
        "MEMORY",
        &format!("{:.1}%", snapshot.memory_percent.max(0.0)),
        &format!(
            "{:.2} / {:.2} GiB",
            snapshot.memory_used_gib, snapshot.memory_total_gib
        ),
    );
    draw_metric_card(
        &mut image,
        col3_x,
        row1_y,
        card_w,
        card_h,
        "UPTIME / DISK",
        &format_duration(snapshot.uptime_secs),
        &format!(
            "disk {:.1}%  l1 {:.2}",
            snapshot.disk_percent.max(0.0),
            snapshot.load_avg.0
        ),
    );

    // Trend charts (full width, 30 minutes)
    let chart_x = 40;
    let chart_w = CANVAS_WIDTH - 80;
    let window_start_ms = snapshot
        .captured_at_ms
        .saturating_sub(STATUS_TREND_WINDOW_SECS.saturating_mul(1_000));
    let mut points = trend_points
        .iter()
        .filter(|point| {
            point.timestamp_ms >= window_start_ms
                && point.timestamp_ms <= snapshot.captured_at_ms + 3_000
        })
        .cloned()
        .collect::<Vec<_>>();
    points.sort_by_key(|point| point.timestamp_ms);
    if points.is_empty() {
        points.push(TrendPoint {
            timestamp_ms: snapshot.captured_at_ms,
            cpu_percent: snapshot.cpu_percent,
            memory_percent: snapshot.memory_percent,
            net_rx_mbps: snapshot.net_rx_mbps,
            net_tx_mbps: snapshot.net_tx_mbps,
        });
    }

    draw_single_line_time_chart(
        &mut image,
        chart_x,
        chart_1_y,
        chart_w,
        chart_h,
        "CPU USAGE TREND (30M)",
        "0-100%",
        &points,
        snapshot.captured_at_ms,
        STATUS_TREND_WINDOW_SECS,
        100.0,
        rgba(51, 122, 255),
        |point| point.cpu_percent,
    );
    draw_single_line_time_chart(
        &mut image,
        chart_x,
        chart_2_y,
        chart_w,
        chart_h,
        "MEMORY USAGE TREND (30M)",
        "0-100%",
        &points,
        snapshot.captured_at_ms,
        STATUS_TREND_WINDOW_SECS,
        100.0,
        rgba(56, 162, 88),
        |point| point.memory_percent,
    );
    draw_network_time_chart(
        &mut image,
        chart_x,
        chart_3_y,
        chart_w,
        chart_h,
        "NET RX/TX TREND (30M)",
        &points,
        snapshot.captured_at_ms,
        STATUS_TREND_WINDOW_SECS,
    );

    // Process table
    let table_x = 40;
    let table_w = CANVAS_WIDTH - 80;
    draw_filled_rect(
        &mut image,
        table_x,
        table_y,
        table_w,
        table_h,
        rgba(250, 252, 255),
    );
    draw_rect_border(
        &mut image,
        table_x,
        table_y,
        table_w,
        table_h,
        rgba(220, 226, 234),
    );
    draw_text_weight(
        &mut image,
        table_x + 20,
        table_y + 12,
        2,
        TextWeight::SemiBold,
        rgba(56, 71, 89),
        "TOP PROCESSES",
    );
    draw_text_weight(
        &mut image,
        table_x + 20,
        table_y + 40,
        2,
        TextWeight::SemiBold,
        rgba(104, 119, 137),
        "PID",
    );
    draw_text_weight(
        &mut image,
        table_x + 180,
        table_y + 40,
        2,
        TextWeight::SemiBold,
        rgba(104, 119, 137),
        "NAME",
    );
    draw_text_weight(
        &mut image,
        table_x + 700,
        table_y + 40,
        2,
        TextWeight::SemiBold,
        rgba(104, 119, 137),
        "RSS",
    );

    let mut row_y = table_y + 68;
    let row_h = 34;
    let max_rows = ((table_h.saturating_sub(80)) / row_h).max(1) as usize;
    for item in snapshot.top_processes.iter().take(max_rows) {
        draw_text_weight(
            &mut image,
            table_x + 20,
            row_y,
            2,
            TextWeight::Regular,
            rgba(56, 71, 89),
            &item.pid,
        );
        draw_text_weight(
            &mut image,
            table_x + 180,
            row_y,
            2,
            TextWeight::Regular,
            rgba(56, 71, 89),
            &truncate_ascii(&item.name, 32),
        );
        draw_text_weight(
            &mut image,
            table_x + 700,
            row_y,
            2,
            TextWeight::Regular,
            rgba(56, 71, 89),
            &format!("{:.1} MiB", item.rss_mb.max(0.0)),
        );
        draw_horizontal_line(
            &mut image,
            table_x + 16,
            table_x + table_w - 16,
            row_y + 24,
            rgba(231, 236, 243),
        );
        row_y += row_h;
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
    let max_w = w.saturating_sub(36);
    draw_text_weight(
        image,
        x + 18,
        y + 14,
        2,
        TextWeight::SemiBold,
        rgba(100, 116, 133),
        &fit_text_to_width_weight(title, 2, TextWeight::SemiBold, max_w),
    );
    draw_text_weight(
        image,
        x + 18,
        y + 50,
        3,
        TextWeight::Bold,
        rgba(31, 47, 63),
        &fit_text_to_width_weight(value, 3, TextWeight::Bold, max_w),
    );
    draw_text_weight(
        image,
        x + 18,
        y + 108,
        2,
        TextWeight::Regular,
        rgba(121, 135, 151),
        &fit_text_to_width_weight(subtitle, 2, TextWeight::Regular, max_w),
    );
}

fn draw_single_line_time_chart<F>(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    title: &str,
    unit_text: &str,
    points: &[TrendPoint],
    now_ms: i64,
    window_secs: i64,
    max_value: f64,
    line_color: Rgba<u8>,
    selector: F,
) where
    F: Fn(&TrendPoint) -> f64,
{
    draw_filled_rect(image, x, y, w, h, rgba(250, 252, 255));
    draw_rect_border(image, x, y, w, h, rgba(220, 226, 234));
    draw_text_weight(
        image,
        x + 16,
        y + 12,
        2,
        TextWeight::SemiBold,
        rgba(56, 71, 89),
        title,
    );
    draw_text_weight(
        image,
        x + w - 170,
        y + 12,
        2,
        TextWeight::Regular,
        rgba(116, 130, 146),
        unit_text,
    );

    let plot_x = x + 66;
    let plot_y = y + 40;
    let plot_w = w.saturating_sub(86);
    let plot_h = h.saturating_sub(58);
    draw_filled_rect(image, plot_x, plot_y, plot_w, plot_h, rgba(245, 248, 252));
    draw_rect_border(image, plot_x, plot_y, plot_w, plot_h, rgba(222, 228, 236));

    for index in 0..=4 {
        let y_line = plot_y + ((plot_h.saturating_sub(1)) * index / 4);
        draw_horizontal_line(
            image,
            plot_x + 1,
            plot_x + plot_w.saturating_sub(2),
            y_line,
            rgba(231, 236, 243),
        );
    }

    draw_text_weight(
        image,
        x + 8,
        plot_y.saturating_sub(6),
        1,
        TextWeight::Regular,
        rgba(112, 126, 143),
        &format!("{:.0}", max_value.max(0.0)),
    );
    draw_text_weight(
        image,
        x + 8,
        plot_y + plot_h / 2 - 6,
        1,
        TextWeight::Regular,
        rgba(128, 140, 154),
        &format!("{:.0}", (max_value / 2.0).max(0.0)),
    );
    draw_text_weight(
        image,
        x + 8,
        plot_y + plot_h.saturating_sub(10),
        1,
        TextWeight::Regular,
        rgba(128, 140, 154),
        "0",
    );

    draw_time_grid_and_labels(image, plot_x, plot_y, plot_w, plot_h, window_secs);

    let safe_max = max_value.max(1.0);
    let sampled = build_smoothed_minute_series(points, now_ms, window_secs, |point| {
        selector(point).clamp(0.0, safe_max)
    });
    let xy_points = sampled
        .iter()
        .map(|(timestamp_ms, value)| {
            (
                map_point_x(*timestamp_ms, now_ms, window_secs, plot_x, plot_w),
                map_point_y(*value, safe_max, plot_y, plot_h),
            )
        })
        .collect::<Vec<_>>();
    draw_smooth_polyline(
        image,
        &xy_points,
        line_color,
        2,
        plot_y as i32,
        (plot_y + plot_h.saturating_sub(1)) as i32,
    );
}

fn draw_network_time_chart(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    title: &str,
    points: &[TrendPoint],
    now_ms: i64,
    window_secs: i64,
) {
    draw_filled_rect(image, x, y, w, h, rgba(250, 252, 255));
    draw_rect_border(image, x, y, w, h, rgba(220, 226, 234));

    let max_rx = points
        .iter()
        .map(|point| point.net_rx_mbps)
        .fold(0.0_f64, f64::max);
    let max_tx = points
        .iter()
        .map(|point| point.net_tx_mbps)
        .fold(0.0_f64, f64::max);
    let max_value = nice_upper_bound(max_rx.max(max_tx).max(1.0));
    draw_text_weight(
        image,
        x + 16,
        y + 12,
        2,
        TextWeight::SemiBold,
        rgba(56, 71, 89),
        title,
    );
    draw_text_right_fitted_weight(
        image,
        x + w.saturating_sub(16),
        y + 12,
        2,
        TextWeight::Regular,
        rgba(116, 130, 146),
        &format!("RX {:.2}  TX {:.2} Mbps", max_rx.max(0.0), max_tx.max(0.0)),
        260,
    );

    let plot_x = x + 66;
    let plot_y = y + 40;
    let plot_w = w.saturating_sub(86);
    let plot_h = h.saturating_sub(58);
    draw_filled_rect(image, plot_x, plot_y, plot_w, plot_h, rgba(245, 248, 252));
    draw_rect_border(image, plot_x, plot_y, plot_w, plot_h, rgba(222, 228, 236));

    for index in 0..=4 {
        let y_line = plot_y + ((plot_h.saturating_sub(1)) * index / 4);
        draw_horizontal_line(
            image,
            plot_x + 1,
            plot_x + plot_w.saturating_sub(2),
            y_line,
            rgba(231, 236, 243),
        );
    }

    draw_text_weight(
        image,
        x + 8,
        plot_y.saturating_sub(6),
        1,
        TextWeight::Regular,
        rgba(112, 126, 143),
        &format!("{:.1}", max_value.max(0.0)),
    );
    draw_text_weight(
        image,
        x + 8,
        plot_y + plot_h / 2 - 6,
        1,
        TextWeight::Regular,
        rgba(128, 140, 154),
        &format!("{:.1}", (max_value / 2.0).max(0.0)),
    );
    draw_text_weight(
        image,
        x + 8,
        plot_y + plot_h.saturating_sub(10),
        1,
        TextWeight::Regular,
        rgba(128, 140, 154),
        "0",
    );

    draw_time_grid_and_labels(image, plot_x, plot_y, plot_w, plot_h, window_secs);

    let rx_color = rgba(62, 121, 244);
    let tx_color = rgba(235, 122, 74);
    let rx_sampled = build_smoothed_minute_series(points, now_ms, window_secs, |point| {
        point.net_rx_mbps.clamp(0.0, max_value)
    });
    let tx_sampled = build_smoothed_minute_series(points, now_ms, window_secs, |point| {
        point.net_tx_mbps.clamp(0.0, max_value)
    });
    let rx_xy = rx_sampled
        .iter()
        .map(|(timestamp_ms, value)| {
            (
                map_point_x(*timestamp_ms, now_ms, window_secs, plot_x, plot_w),
                map_point_y(*value, max_value, plot_y, plot_h),
            )
        })
        .collect::<Vec<_>>();
    let tx_xy = tx_sampled
        .iter()
        .map(|(timestamp_ms, value)| {
            (
                map_point_x(*timestamp_ms, now_ms, window_secs, plot_x, plot_w),
                map_point_y(*value, max_value, plot_y, plot_h),
            )
        })
        .collect::<Vec<_>>();
    draw_smooth_polyline(
        image,
        &rx_xy,
        rx_color,
        2,
        plot_y as i32,
        (plot_y + plot_h.saturating_sub(1)) as i32,
    );
    draw_smooth_polyline(
        image,
        &tx_xy,
        tx_color,
        2,
        plot_y as i32,
        (plot_y + plot_h.saturating_sub(1)) as i32,
    );
}

fn draw_time_grid_and_labels(
    image: &mut RgbaImage,
    plot_x: u32,
    plot_y: u32,
    plot_w: u32,
    plot_h: u32,
    window_secs: i64,
) {
    let minutes = (window_secs.max(60) / 60) as u32;
    if minutes == 0 {
        return;
    }
    for minute in 0..=minutes {
        let x = plot_x + ((plot_w.saturating_sub(1)) * minute / minutes);
        let color = if minute % 5 == 0 {
            rgba(222, 228, 236)
        } else {
            rgba(235, 239, 245)
        };
        draw_vertical_line(
            image,
            x,
            plot_y + 1,
            plot_y + plot_h.saturating_sub(2),
            color,
        );
        if minute % 5 == 0 || minute == minutes {
            let remaining = minutes.saturating_sub(minute);
            let label = if remaining == 0 {
                "now".to_string()
            } else {
                format!("-{}m", remaining)
            };
            let label_w = measure_text_width_weight(1, TextWeight::Regular, &label);
            let label_x = x.saturating_sub(label_w / 2);
            draw_text_weight(
                image,
                label_x,
                plot_y + plot_h + 4,
                1,
                TextWeight::Regular,
                rgba(128, 140, 154),
                &label,
            );
        }
    }
}

fn map_point_x(timestamp_ms: i64, now_ms: i64, window_secs: i64, plot_x: u32, plot_w: u32) -> i32 {
    let window_ms = (window_secs.max(1) * 1_000) as f64;
    let start_ms = now_ms.saturating_sub(window_secs.saturating_mul(1_000)) as f64;
    let clamped = ((timestamp_ms as f64 - start_ms) / window_ms).clamp(0.0, 1.0);
    let x = plot_x as f64 + clamped * (plot_w.saturating_sub(1) as f64);
    x.round() as i32
}

fn map_point_y(value: f64, max_value: f64, plot_y: u32, plot_h: u32) -> i32 {
    let safe_max = max_value.max(1e-6);
    let clamped = (value / safe_max).clamp(0.0, 1.0);
    let y = plot_y as f64 + (1.0 - clamped) * (plot_h.saturating_sub(1) as f64);
    y.round() as i32
}

fn build_smoothed_minute_series<F>(
    points: &[TrendPoint],
    now_ms: i64,
    window_secs: i64,
    selector: F,
) -> Vec<(i64, f64)>
where
    F: Fn(&TrendPoint) -> f64,
{
    let safe_window_secs = window_secs.max(60);
    let start_ms = now_ms.saturating_sub(safe_window_secs.saturating_mul(1_000));
    let minute_steps = (safe_window_secs / 60).max(1) as usize;
    let step_ms = 60_000_i64;

    if points.is_empty() {
        return (0..=minute_steps)
            .map(|index| (start_ms + index as i64 * step_ms, 0.0))
            .collect();
    }

    let mut sorted = points.to_vec();
    sorted.sort_by_key(|point| point.timestamp_ms);

    let mut sampled = Vec::with_capacity(minute_steps + 1);
    let mut cursor = 0usize;
    for index in 0..=minute_steps {
        let target_ms = start_ms + index as i64 * step_ms;
        while cursor + 1 < sorted.len() && sorted[cursor + 1].timestamp_ms < target_ms {
            cursor += 1;
        }
        let value = interpolate_series_value(&sorted, cursor, target_ms, &selector);
        sampled.push((target_ms, value.max(0.0)));
    }

    if sampled.len() <= 2 {
        return sampled;
    }

    let raw = sampled.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    let mut pass_1 = smooth_values(&raw, &[1.0, 2.0, 3.0, 2.0, 1.0]);
    pass_1 = smooth_values(&pass_1, &[1.0, 2.0, 1.0]);
    if let Some(first) = pass_1.first_mut() {
        *first = raw[0];
    }
    if let Some(last) = pass_1.last_mut() {
        *last = *raw.last().unwrap_or(last);
    }
    for (index, (_, value)) in sampled.iter_mut().enumerate() {
        *value = pass_1[index].max(0.0);
    }
    sampled
}

fn interpolate_series_value<F>(
    points: &[TrendPoint],
    cursor: usize,
    target_ms: i64,
    selector: &F,
) -> f64
where
    F: Fn(&TrendPoint) -> f64,
{
    if points.is_empty() {
        return 0.0;
    }
    if points.len() == 1 {
        return selector(&points[0]);
    }

    let left_index = cursor.min(points.len() - 1);
    let right_index = (left_index + 1).min(points.len() - 1);
    let left = &points[left_index];
    let right = &points[right_index];

    let left_value = selector(left);
    if left_index == right_index || right.timestamp_ms <= left.timestamp_ms {
        return left_value;
    }
    if target_ms <= left.timestamp_ms {
        return left_value;
    }

    let right_value = selector(right);
    if target_ms >= right.timestamp_ms {
        return right_value;
    }

    let span = (right.timestamp_ms - left.timestamp_ms) as f64;
    if span <= 0.0 {
        return left_value;
    }
    let alpha = ((target_ms - left.timestamp_ms) as f64 / span).clamp(0.0, 1.0);
    left_value + (right_value - left_value) * alpha
}

fn smooth_values(values: &[f64], kernel: &[f64]) -> Vec<f64> {
    if values.is_empty() || kernel.is_empty() {
        return values.to_vec();
    }
    let radius = (kernel.len() / 2) as i32;
    let mut result = Vec::with_capacity(values.len());

    for index in 0..values.len() {
        let mut weighted_sum = 0.0;
        let mut weight_total = 0.0;
        for (kernel_index, weight) in kernel.iter().enumerate() {
            let offset = kernel_index as i32 - radius;
            let sample_index = index as i32 + offset;
            if sample_index < 0 || sample_index >= values.len() as i32 {
                continue;
            }
            weighted_sum += values[sample_index as usize] * weight;
            weight_total += weight;
        }
        result.push(if weight_total > 0.0 {
            weighted_sum / weight_total
        } else {
            values[index]
        });
    }
    result
}

fn draw_smooth_polyline(
    image: &mut RgbaImage,
    points: &[(i32, i32)],
    color: Rgba<u8>,
    thickness: u32,
    min_y: i32,
    max_y: i32,
) {
    if points.is_empty() {
        return;
    }
    if points.len() == 1 {
        draw_scaled_dot(
            image,
            points[0].0 - (thickness as i32 / 2),
            points[0].1.clamp(min_y, max_y) - (thickness as i32 / 2),
            thickness.max(1),
            color,
        );
        return;
    }

    let subdivisions = 8;
    let mut previous = (points[0].0, points[0].1.clamp(min_y, max_y));

    for segment in 0..points.len() - 1 {
        let p0 = if segment == 0 {
            points[segment]
        } else {
            points[segment - 1]
        };
        let p1 = points[segment];
        let p2 = points[segment + 1];
        let p3 = if segment + 2 < points.len() {
            points[segment + 2]
        } else {
            points[segment + 1]
        };

        for step in 1..=subdivisions {
            let t = step as f64 / subdivisions as f64;
            let raw_x = catmull_rom(p0.0 as f64, p1.0 as f64, p2.0 as f64, p3.0 as f64, t);
            let raw_y = catmull_rom(p0.1 as f64, p1.1 as f64, p2.1 as f64, p3.1 as f64, t);
            let clamped_x = raw_x
                .clamp(p1.0.min(p2.0) as f64, p1.0.max(p2.0) as f64)
                .round() as i32;
            let clamped_y = raw_y.clamp(min_y as f64, max_y as f64).round() as i32;
            draw_line_segment(
                image,
                previous.0,
                previous.1,
                clamped_x,
                clamped_y,
                color,
                thickness.max(1),
            );
            previous = (clamped_x, clamped_y);
        }
    }
}

fn catmull_rom(p0: f64, p1: f64, p2: f64, p3: f64, t: f64) -> f64 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

fn draw_line_segment(
    image: &mut RgbaImage,
    mut x0: i32,
    mut y0: i32,
    x1: i32,
    y1: i32,
    color: Rgba<u8>,
    thickness: u32,
) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;

    loop {
        draw_scaled_dot(
            image,
            x0 - (thickness as i32 / 2),
            y0 - (thickness as i32 / 2),
            thickness.max(1),
            color,
        );
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = error.saturating_mul(2);
        if e2 >= dy {
            error += dy;
            x0 += sx;
        }
        if e2 <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

fn nice_upper_bound(raw_max: f64) -> f64 {
    let value = raw_max.max(1e-6);
    let exponent = value.log10().floor();
    let base = 10_f64.powf(exponent);
    let normalized = value / base;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    (nice * base).max(1.0)
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
    draw_text_weight(image, x, y, scale, TextWeight::Regular, color, text);
}

fn draw_text_weight(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    scale: u32,
    weight: TextWeight,
    color: Rgba<u8>,
    text: &str,
) {
    if let Some(family) = status_font_family() {
        let font = font_by_weight(family, weight);
        let px = text_px_for_scale(scale, weight);
        let line_height = line_height_for_scale(scale);
        let mut cursor_y = y as i32;
        for line in text.lines() {
            if !line.is_empty() {
                draw_text_mut(
                    image,
                    color,
                    x as i32,
                    cursor_y,
                    PxScale::from(px),
                    font,
                    line,
                );
            }
            cursor_y += line_height;
        }
    } else {
        draw_text_bitmap(image, x, y, scale, color, text);
    }
}

fn draw_text_right_fitted(
    image: &mut RgbaImage,
    right_x: u32,
    y: u32,
    scale: u32,
    color: Rgba<u8>,
    text: &str,
    max_width: u32,
) {
    draw_text_right_fitted_weight(
        image,
        right_x,
        y,
        scale,
        TextWeight::Regular,
        color,
        text,
        max_width,
    );
}

fn draw_text_right_fitted_weight(
    image: &mut RgbaImage,
    right_x: u32,
    y: u32,
    scale: u32,
    weight: TextWeight,
    color: Rgba<u8>,
    text: &str,
    max_width: u32,
) {
    let fitted = fit_text_to_width_weight(text, scale, weight, max_width);
    let width = measure_text_width_weight(scale, weight, &fitted);
    let start_x = right_x.saturating_sub(width);
    draw_text_weight(image, start_x, y, scale, weight, color, &fitted);
}

fn measure_text_width(scale: u32, text: &str) -> u32 {
    measure_text_width_weight(scale, TextWeight::Regular, text)
}

fn measure_text_width_weight(scale: u32, weight: TextWeight, text: &str) -> u32 {
    if let Some(family) = status_font_family() {
        let font = font_by_weight(family, weight);
        let px = text_px_for_scale(scale, weight);
        text.lines()
            .map(|line| measure_line_width_font(font, px, line).ceil() as u32)
            .max()
            .unwrap_or(0)
    } else {
        let step_x = 8 * scale + scale;
        text.lines()
            .map(|line| line.chars().count() as u32 * step_x)
            .max()
            .unwrap_or(0)
    }
}

fn fit_text_to_width(text: &str, scale: u32, max_width: u32) -> String {
    fit_text_to_width_weight(text, scale, TextWeight::Regular, max_width)
}

fn fit_text_to_width_weight(text: &str, scale: u32, weight: TextWeight, max_width: u32) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut lines = Vec::new();
    for line in text.lines() {
        if measure_text_width_weight(scale, weight, line) <= max_width {
            lines.push(line.to_string());
            continue;
        }

        let chars = line.chars().collect::<Vec<_>>();
        if chars.is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut keep = chars.len();
        let shortened = loop {
            let prefix = chars.iter().take(keep).collect::<String>();
            let candidate = format!("{prefix}...");
            if measure_text_width_weight(scale, weight, &candidate) <= max_width || keep <= 1 {
                break candidate;
            }
            keep -= 1;
        };
        lines.push(shortened);
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n")
    }
}

fn draw_text_bitmap(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    scale: u32,
    color: Rgba<u8>,
    text: &str,
) {
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
                let px = cursor_x + (col as i32 * scale as i32);
                let py = cursor_y + (row as i32 * scale as i32);
                draw_scaled_dot(image, px, py, scale, color);
            }
        }
        cursor_x += step_x;
    }
}

fn status_font_family() -> Option<&'static FontFamily> {
    STATUS_FONT_FAMILY
        .get_or_init(load_status_font_family)
        .as_ref()
}

fn load_status_font_family() -> Option<FontFamily> {
    let candidates = [
        PathBuf::from("/Cainbot/assets/fonts/firacode"),
        PathBuf::from("/Cainbot/assets/fonts/FiraCode"),
        PathBuf::from("./assets/fonts/firacode"),
        PathBuf::from("./assets/fonts/FiraCode"),
        PathBuf::from("C:\\Users\\黄子豪\\Downloads\\Fira_Code_v6.2\\ttf"),
        PathBuf::from("C:\\Users\\华硕\\Downloads\\Fira_Code_v6.2\\ttf"),
    ];
    for dir in candidates {
        if let Some(family) = load_font_family_from_dir(&dir) {
            return Some(family);
        }
    }
    None
}

fn load_font_family_from_dir(dir: &Path) -> Option<FontFamily> {
    let regular = load_font(&dir.join("FiraCode-Regular.ttf"))?;
    let semibold = load_font(&dir.join("FiraCode-SemiBold.ttf"))
        .or_else(|| load_font(&dir.join("FiraCode-Medium.ttf")))
        .unwrap_or_else(|| regular.clone());
    let bold = load_font(&dir.join("FiraCode-Bold.ttf"))
        .or_else(|| load_font(&dir.join("FiraCode-SemiBold.ttf")))
        .unwrap_or_else(|| semibold.clone());
    Some(FontFamily {
        regular,
        semibold,
        bold,
    })
}

fn load_font(path: &Path) -> Option<FontArc> {
    let bytes = std::fs::read(path).ok()?;
    FontArc::try_from_vec(bytes).ok()
}

fn font_by_weight(family: &FontFamily, weight: TextWeight) -> &FontArc {
    match weight {
        TextWeight::Regular => &family.regular,
        TextWeight::SemiBold => &family.semibold,
        TextWeight::Bold => &family.bold,
    }
}

fn text_px_for_scale(scale: u32, weight: TextWeight) -> f32 {
    let base = match scale {
        0 | 1 => 14.0,
        2 => 22.0,
        _ => 38.0,
    };
    match weight {
        TextWeight::Regular => base,
        TextWeight::SemiBold => base + 0.4,
        TextWeight::Bold => base + 0.8,
    }
}

fn line_height_for_scale(scale: u32) -> i32 {
    match scale {
        0 | 1 => 18,
        2 => 28,
        _ => 44,
    }
}

fn measure_line_width_font(font: &FontArc, px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut width = 0.0f32;
    let mut previous = None;
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

fn draw_vertical_line(image: &mut RgbaImage, x: u32, start_y: u32, end_y: u32, color: Rgba<u8>) {
    if x >= image.width() {
        return;
    }
    let safe_end = end_y.min(image.height().saturating_sub(1));
    for y in start_y..=safe_end {
        image.put_pixel(x, y, color);
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
    let values = parts
        .filter_map(|item| item.parse::<u64>().ok())
        .collect::<Vec<_>>();
    if values.len() < 4 {
        return None;
    }
    let idle =
        values.get(3).copied().unwrap_or_default() + values.get(4).copied().unwrap_or_default();
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
    use super::{create_status_dashboard_image, tmux_stderr_indicates_missing_session};

    #[tokio::test]
    async fn creates_status_dashboard_png() {
        let output = create_status_dashboard_image().await.expect("status image");
        let metadata = tokio::fs::metadata(&output)
            .await
            .expect("status image metadata");
        assert!(metadata.len() > 0, "status image should not be empty");
    }

    #[tokio::test]
    #[ignore = "manual snapshot probe"]
    async fn render_probe_prints_status_path() {
        let output = create_status_dashboard_image().await.expect("status image");
        println!("status: {}", output.display());
    }

    #[test]
    fn treats_missing_tmux_server_as_missing_session() {
        assert!(tmux_stderr_indicates_missing_session(
            "no server running on /tmp/tmux-0/default"
        ));
        assert!(tmux_stderr_indicates_missing_session(
            "can't find session: cainbot_btop"
        ));
        assert!(!tmux_stderr_indicates_missing_session(
            "unknown option -- bad"
        ));
    }
}
