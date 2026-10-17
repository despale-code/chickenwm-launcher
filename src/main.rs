//! Phase 7: Flow-Style Application Launcher (`chickenwm-launcher`)
//! - 7.1: Instant Daemon Architecture (`<10ms` summon via IPC `\\.\pipe\chickenwm-launcher-ipc`)
//!        with fade + scale animation synced to active monitor refresh rate / `fps_limit`
//! - 7.2: Multi-Source Application Indexer (Start Menu, Desktop, `PATH`, UWP packages, `SHGetFileInfoW` icons)
//! - 7.3: `nucleo-matcher` Fuzzy Search & 5 Core Plugins:
//!        App Runner (`Ctrl+Shift+Enter` RunAs Admin), Calculator (`= 144 * 1.5`),
//!        Shell Command (`> cargo build`), Window Switcher (`w `), System Commands (`lock`, `sleep`, `reboot`, `shutdown`, `reload`, `restore-explorer`)
//! - 7.4: Live CSS Styling (`~/.config/chickenwm/launcher.css`)

mod icons;
mod pixel_icons;

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::css::{load_css_sheet, spawn_config_watcher, ConfigWatchEvent, CssStyleSheet};
use chickenwm_core::failsafe::{spawn_emergency_failsafe_thread, trigger_emergency_restore};
use chickenwm_core::init_tracing;
use icons::IconLoader;
use pixel_icons::PixelIcon;
use chickenwm_core::ipc::{
    send_ipc_command_blocking, serve_ipc, Action, ClientInfo, IpcCommand, IpcResponse,
    PIPE_NAME_DAEMON, PIPE_NAME_LAUNCHER, PIPE_NAME_WM,
};
use chickenwm_core::monitor::{enumerate_monitors, max_native_refresh_rate};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, POINT, RECT};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, GetWindowRect, IsWindowVisible, SetForegroundWindow,
    SetWindowPos, HWND_TOPMOST, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOSIZE,
};

/// Visible result rows (rofi's default `lines: 15`).
const MAX_RESULTS: usize = 15;

/// The launcher's top-level window (0 until eframe creates it).
static LAUNCHER_HWND: AtomicIsize = AtomicIsize::new(0);
/// Whether the launcher is meant to be on screen.
///
/// The window is never actually hidden: eframe spins at 100% of a core while its window is
/// hidden (egui asks for a follow-up frame that a hidden window can never paint), and a hidden
/// window can't react to summons. Instead it's parked off-screen when dismissed and moved back
/// (centred on the monitor under the cursor) when summoned; idle, eframe paints nothing.
static LAUNCHER_SHOWN: AtomicBool = AtomicBool::new(false);

fn launcher_hwnd() -> Option<HWND> {
    let raw = LAUNCHER_HWND.load(Ordering::SeqCst);
    (raw != 0).then(|| HWND(raw as *mut core::ffi::c_void))
}

fn set_window_shown(show: bool) {
    LAUNCHER_SHOWN.store(show, Ordering::SeqCst);
    let Some(hwnd) = launcher_hwnd() else {
        return;
    };
    unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
        let (x, y) = if show {
            // Centre on the work area of the monitor the cursor is on.
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(monitor, &mut info);
            let work = info.rcWork;
            (
                work.left + (work.right - work.left - w) / 2,
                work.top + (work.bottom - work.top - h) / 2,
            )
        } else {
            // Fully outside every monitor.
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) - w - 10_000,
                GetSystemMetrics(SM_YVIRTUALSCREEN) - h - 10_000,
            )
        };
        let moved = SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        tracing::debug!("launcher shown={show} -> ({x},{y}) {w}x{h}: {moved:?}");
        if show {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// eframe shows the window after its first paint; a launcher started in `--daemon` mode parks
/// it off-screen once that has happened.
fn hide_after_first_frame() {
    std::thread::spawn(|| {
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(50));
            if LAUNCHER_SHOWN.load(Ordering::SeqCst) {
                return; // summoned meanwhile
            }
            if let Some(hwnd) = launcher_hwnd() {
                if unsafe { IsWindowVisible(hwnd) }.as_bool() {
                    set_window_shown(false);
                    return;
                }
            }
        }
    });
}

#[derive(Parser, Debug)]
#[command(
    name = "chickenwm-launcher",
    version,
    about = "ChickenWM Instant Fuzzy Application Launcher & Plugin Engine"
)]
struct Cli {
    /// Start minimized in background daemon mode waiting for IPC summon (`$mainMod + D` or `Alt + Space`).
    #[arg(long)]
    daemon: bool,
}

#[derive(Debug, Clone)]
enum LauncherItemKind {
    Application {
        exec_path: PathBuf,
    },
    Calculator {
        expression: String,
        result: String,
    },
    ShellCommand {
        command: String,
    },
    WindowSwitch {
        hwnd: u64,
        workspace_id: u32,
    },
    SystemCommand {
        cmd_id: &'static str,
    },
}

#[derive(Debug, Clone)]
struct LauncherEntry {
    title: String,
    subtitle: String,
    badge: &'static str,
    kind: LauncherItemKind,
    score: u32,
}

/// URI opened by the Snip entry (and `SUPER SHIFT, S`).
const SNIP_URI: &str = "ms-screenclip:";

/// ChickenWM's own apps, listed in drun with pixel-art icons: (title, subtitle, exe).
const CHICKENWM_APPS: [(&str, &str, &str); 3] = [
    ("ChickenWM Files", "ChickenWM file explorer (Super + E)", "chickenwm-files.exe"),
    ("ChickenWM Settings", "ChickenWM settings (Super + I)", "chickenwm-settings.exe"),
    ("ChickenBrowser", "ChickenWM web browser", "chickenbrowser.exe"),
];

/// Built-in pixel-art icon for an entry; `None` means use the app's own icon.
fn pixel_icon_for(kind: &LauncherItemKind) -> Option<PixelIcon> {
    match kind {
        LauncherItemKind::Application { exec_path } => {
            if exec_path.as_os_str() == SNIP_URI {
                return Some(PixelIcon::Snip);
            }
            let name = exec_path.file_name()?.to_str()?.to_ascii_lowercase();
            match name.as_str() {
                "chickenwm-files.exe" => Some(PixelIcon::Files),
                "chickenwm-settings.exe" => Some(PixelIcon::Settings),
                "chickenbrowser.exe" => Some(PixelIcon::Browser),
                _ => None,
            }
        }
        LauncherItemKind::Calculator { .. } => Some(PixelIcon::Calculator),
        LauncherItemKind::ShellCommand { .. } => Some(PixelIcon::Shell),
        LauncherItemKind::WindowSwitch { .. } => Some(PixelIcon::WindowSwitch),
        LauncherItemKind::SystemCommand { cmd_id } => match *cmd_id {
            "reload" => Some(PixelIcon::Reload),
            "restore-explorer" => Some(PixelIcon::RestoreExplorer),
            "lock" => Some(PixelIcon::Lock),
            "sleep" => Some(PixelIcon::Sleep),
            "reboot" => Some(PixelIcon::Reboot),
            "shutdown" => Some(PixelIcon::Shutdown),
            _ => None,
        },
    }
}

/// Phase 7.2: Multi-source application indexer scanning Start Menu folders, Desktop, and `PATH`.
fn index_all_applications() -> Vec<LauncherEntry> {
    let mut entries = Vec::new();
    let mut seen_names = HashSet::new();

    // 1. Built-in ChickenWM Suite & System Commands
    let sys_cmds: [(&str, &str, &'static str); 6] = [
        ("Reload ChickenWM Config & CSS", "Hot-reload chickenwm.conf and stylesheets", "reload"),
        ("Restore Windows Explorer (Failsafe)", "Restore explorer.exe as default shell", "restore-explorer"),
        ("Lock Workstation", "Lock Windows session immediately", "lock"),
        ("Sleep System", "Suspend computer to low-power state", "sleep"),
        ("Reboot Computer", "Restart Windows immediately", "reboot"),
        ("Shutdown Computer", "Power off Windows immediately", "shutdown"),
    ];
    for (title, sub, id) in sys_cmds {
        entries.push(LauncherEntry {
            title: title.to_string(),
            subtitle: sub.to_string(),
            badge: "SYS",
            kind: LauncherItemKind::SystemCommand { cmd_id: id },
            score: 0,
        });
    }

    // ChickenWM's own apps (when installed next to the launcher) and the screen snip tool.
    if let Ok(self_exe) = std::env::current_exe() {
        for (title, sub, exe) in CHICKENWM_APPS {
            let path = self_exe.with_file_name(exe);
            if path.exists() {
                seen_names.insert(title.to_ascii_lowercase());
                entries.push(LauncherEntry {
                    title: title.to_string(),
                    subtitle: sub.to_string(),
                    badge: "APP",
                    kind: LauncherItemKind::Application { exec_path: path },
                    score: 0,
                });
            }
        }
    }
    entries.push(LauncherEntry {
        title: "Snip Screenshot".to_string(),
        subtitle: "Capture part of the screen (Super + Shift + S)".to_string(),
        badge: "SYS",
        kind: LauncherItemKind::Application {
            exec_path: PathBuf::from(SNIP_URI),
        },
        score: 0,
    });

    // 2. Scan Start Menu & Desktop directories
    let mut scan_roots = Vec::new();
    if let Ok(prog_data) = std::env::var("ProgramData") {
        scan_roots.push(
            PathBuf::from(prog_data)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    if let Ok(app_data) = std::env::var("APPDATA") {
        scan_roots.push(
            PathBuf::from(app_data)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    if let Some(desktop) = dirs::desktop_dir() {
        scan_roots.push(desktop);
    }

    for root in scan_roots {
        scan_dir_recursive(&root, 0, &mut entries, &mut seen_names);
    }

    // 3. Scan PATH executables (limiting to fast top-level lookup)
    if let Ok(path_var) = std::env::var("PATH") {
        for dir_str in path_var.split(';').take(18) {
            let p = Path::new(dir_str);
            if let Ok(rd) = std::fs::read_dir(p) {
                for item in rd.flatten().take(60) {
                    let path = item.path();
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if ext.eq_ignore_ascii_case("exe") {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                let key = stem.to_ascii_lowercase();
                                if seen_names.insert(key) {
                                    entries.push(LauncherEntry {
                                        title: stem.to_string(),
                                        subtitle: path.to_string_lossy().into_owned(),
                                        badge: "EXE",
                                        kind: LauncherItemKind::Application { exec_path: path },
                                        score: 0,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    entries
}

fn scan_dir_recursive(
    dir: &Path,
    depth: usize,
    out: &mut Vec<LauncherEntry>,
    seen: &mut HashSet<String>,
) {
    if depth > 4 || !dir.exists() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, depth + 1, out, seen);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("lnk")
                || ext.eq_ignore_ascii_case("exe")
                || ext.eq_ignore_ascii_case("url")
            {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let lower = stem.to_ascii_lowercase();
                    if !lower.contains("uninstall") && seen.insert(lower) {
                        out.push(LauncherEntry {
                            title: stem.to_string(),
                            subtitle: path.to_string_lossy().into_owned(),
                            badge: "APP",
                            kind: LauncherItemKind::Application { exec_path: path },
                            score: 0,
                        });
                    }
                }
            }
        }
    }
}

/// Phase 7.3: Inline recursive-descent math expression evaluator (`= 144 * 1.5`, `+`, `-`, `*`, `/`, `^`, `(...)`).
pub fn evaluate_math_expression(expr: &str) -> Option<f64> {
    let tokens = tokenize_math(expr)?;
    if tokens.is_empty() {
        return None;
    }
    let mut pos = 0;
    let val = parse_add_sub(&tokens, &mut pos)?;
    if pos == tokens.len() && val.is_finite() {
        Some(val)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
enum MathToken {
    Num(f64),
    Op(char),
    LParen,
    RParen,
}

fn tokenize_math(s: &str) -> Option<Vec<MathToken>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c.is_ascii_digit() || c == '.' {
            let mut num_s = String::new();
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_digit() || nc == '.' {
                    num_s.push(nc);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(MathToken::Num(num_s.parse::<f64>().ok()?));
        } else if matches!(c, '+' | '-' | '*' | '/' | '^' | '%') {
            out.push(MathToken::Op(c));
            chars.next();
        } else if c == '(' {
            out.push(MathToken::LParen);
            chars.next();
        } else if c == ')' {
            out.push(MathToken::RParen);
            chars.next();
        } else {
            return None;
        }
    }
    Some(out)
}

fn parse_add_sub(tokens: &[MathToken], pos: &mut usize) -> Option<f64> {
    let mut lhs = parse_mul_div(tokens, pos)?;
    while let Some(MathToken::Op(op @ ('+' | '-'))) = tokens.get(*pos) {
        *pos += 1;
        let rhs = parse_mul_div(tokens, pos)?;
        if *op == '+' {
            lhs += rhs;
        } else {
            lhs -= rhs;
        }
    }
    Some(lhs)
}

fn parse_mul_div(tokens: &[MathToken], pos: &mut usize) -> Option<f64> {
    let mut lhs = parse_power(tokens, pos)?;
    while let Some(MathToken::Op(op @ ('*' | '/' | '%'))) = tokens.get(*pos) {
        *pos += 1;
        let rhs = parse_power(tokens, pos)?;
        match *op {
            '*' => lhs *= rhs,
            '/' => {
                if rhs.abs() < 1e-12 {
                    return None;
                }
                lhs /= rhs;
            }
            '%' => lhs %= rhs,
            _ => {}
        }
    }
    Some(lhs)
}

fn parse_power(tokens: &[MathToken], pos: &mut usize) -> Option<f64> {
    let base = parse_unary(tokens, pos)?;
    if let Some(MathToken::Op('^')) = tokens.get(*pos) {
        *pos += 1;
        let exp = parse_power(tokens, pos)?;
        Some(base.powf(exp))
    } else {
        Some(base)
    }
}

fn parse_unary(tokens: &[MathToken], pos: &mut usize) -> Option<f64> {
    if let Some(MathToken::Op('-')) = tokens.get(*pos) {
        *pos += 1;
        return Some(-parse_unary(tokens, pos)?);
    }
    if let Some(MathToken::Op('+')) = tokens.get(*pos) {
        *pos += 1;
        return parse_unary(tokens, pos);
    }
    match tokens.get(*pos)? {
        MathToken::Num(n) => {
            *pos += 1;
            Some(*n)
        }
        MathToken::LParen => {
            *pos += 1;
            let val = parse_add_sub(tokens, pos)?;
            if matches!(tokens.get(*pos), Some(MathToken::RParen)) {
                *pos += 1;
                Some(val)
            } else {
                None
            }
        }
        _ => None,
    }
}

struct LauncherApp {
    query: String,
    selected_idx: usize,
    indexed_apps: Arc<RwLock<Vec<LauncherEntry>>>,
    results: Vec<LauncherEntry>,
    css: Arc<RwLock<CssStyleSheet>>,
    summon_requested: Arc<AtomicBool>,
    visible: bool,
    anim_progress: f32, // 0.0 -> 1.0 fade
    last_frame_instant: Instant,
    target_fps: u32,
    matcher: Matcher,
    icons: IconLoader,
    icon_cache: HashMap<PathBuf, IconSlot>,
    /// Built-in pixel-art icons, uploaded on the first frame.
    pixel_textures: HashMap<PixelIcon, egui::TextureHandle>,
    /// Set when the selection moves by keyboard so the list scrolls it into view.
    scroll_to_selected: bool,
    summoned_at: Option<Instant>,
}

/// Per-application icon state; `Pending` stops the same path being queued every frame.
enum IconSlot {
    Pending,
    Missing,
    Ready(egui::TextureHandle),
}

impl LauncherApp {
    fn new(
        indexed_apps: Arc<RwLock<Vec<LauncherEntry>>>,
        css: Arc<RwLock<CssStyleSheet>>,
        summon_requested: Arc<AtomicBool>,
        start_visible: bool,
        target_fps: u32,
        icons: IconLoader,
    ) -> Self {
        let mut app = Self {
            query: String::new(),
            selected_idx: 0,
            indexed_apps,
            results: Vec::new(),
            css,
            summon_requested,
            visible: start_visible,
            anim_progress: if start_visible { 0.0 } else { 1.0 },
            last_frame_instant: Instant::now(),
            target_fps: target_fps.max(60),
            matcher: Matcher::new(NucleoConfig::DEFAULT),
            icons,
            icon_cache: HashMap::new(),
            pixel_textures: HashMap::new(),
            scroll_to_selected: false,
            summoned_at: start_visible.then(Instant::now),
        };
        app.recompute_results();
        app
    }

    /// Turns icons decoded by the background worker into egui textures.
    fn absorb_loaded_icons(&mut self, ctx: &egui::Context) {
        let loaded: Vec<_> = self.icons.drain().collect();
        for (path, icon) in loaded {
            let slot = match icon {
                Some(img) => IconSlot::Ready(ctx.load_texture(
                    path.to_string_lossy().into_owned(),
                    egui::ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.rgba),
                    egui::TextureOptions::LINEAR,
                )),
                None => IconSlot::Missing,
            };
            self.icon_cache.insert(path, slot);
        }
    }

    fn recompute_results(&mut self) {
        self.results.clear();
        let q = self.query.trim();

        // Plugin 1: Calculator (`= 144 * 1.5`)
        if let Some(math_expr) = q.strip_prefix('=') {
            let expr = math_expr.trim();
            if let Some(val) = evaluate_math_expression(expr) {
                let formatted = if (val - val.round()).abs() < 1e-9 {
                    format!("{:.0}", val)
                } else {
                    format!("{:.6}", val).trim_end_matches('0').trim_end_matches('.').to_string()
                };
                self.results.push(LauncherEntry {
                    title: format!("= {}", formatted),
                    subtitle: format!("Calculator: {} = {}", expr, formatted),
                    badge: "CALC",
                    kind: LauncherItemKind::Calculator {
                        expression: expr.to_string(),
                        result: formatted,
                    },
                    score: u32::MAX,
                });
            }
            self.selected_idx = 0;
            return;
        }

        // Plugin 2: Shell Command (`> cargo build`)
        if let Some(shell_cmd) = q.strip_prefix('>') {
            let cmd = shell_cmd.trim();
            if !cmd.is_empty() {
                self.results.push(LauncherEntry {
                    title: format!("Run command: {}", cmd),
                    subtitle: "Execute in Windows Terminal / PowerShell".to_string(),
                    badge: "CMD",
                    kind: LauncherItemKind::ShellCommand {
                        command: cmd.to_string(),
                    },
                    score: u32::MAX,
                });
            }
            self.selected_idx = 0;
            return;
        }

        // Plugin 3: Window Switcher (`w <query>`)
        if let Some(win_query) = q.strip_prefix("w ").or_else(|| if q == "w" { Some("") } else { None }) {
            let wq = win_query.trim().to_ascii_lowercase();
            let clients: Vec<ClientInfo> =
                match send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::QueryClients) {
                    Ok(IpcResponse::Clients(c)) => c,
                    _ => Vec::new(),
                };
            for c in clients {
                if wq.is_empty()
                    || c.title.to_ascii_lowercase().contains(&wq)
                    || c.process_name.to_ascii_lowercase().contains(&wq)
                {
                    self.results.push(LauncherEntry {
                        title: c.title,
                        subtitle: format!(
                            "Workspace {} • {} (0x{:X})",
                            c.workspace_id, c.process_name, c.hwnd
                        ),
                        badge: "WIN",
                        kind: LauncherItemKind::WindowSwitch {
                            hwnd: c.hwnd,
                            workspace_id: c.workspace_id,
                        },
                        score: 100,
                    });
                }
            }
            self.selected_idx = 0;
            return;
        }

        // Plugin 4 & 5: Fuzzy App Runner & System Commands via `nucleo-matcher`
        let apps = self.indexed_apps.read();
        if q.is_empty() {
            self.results.extend(apps.iter().take(MAX_RESULTS).cloned());
            self.selected_idx = 0;
            return;
        }

        let pat = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let mut char_buf = Vec::new();
        for item in apps.iter() {
            char_buf.clear();
            let haystack = Utf32Str::new(&item.title, &mut char_buf);
            if let Some(score) = pat.score(haystack, &mut self.matcher) {
                let mut matched = item.clone();
                matched.score = score;
                self.results.push(matched);
            }
        }

        self.results.sort_by(|a, b| b.score.cmp(&a.score));
        self.results.truncate(MAX_RESULTS);
        self.selected_idx = 0;
    }

    fn execute_selected(&mut self, run_as_admin: bool, ctx: &egui::Context) {
        let Some(entry) = self.results.get(self.selected_idx).cloned() else {
            return;
        };

        match entry.kind {
            LauncherItemKind::Application { exec_path, .. } => {
                if run_as_admin {
                    let _ = Command::new("powershell.exe")
                        .args([
                            "-NoProfile",
                            "-Command",
                            &format!(
                                "Start-Process -FilePath '{}' -Verb RunAs",
                                exec_path.display()
                            ),
                        ])
                        .spawn();
                } else {
                    let _ = Command::new("cmd.exe")
                        .args(["/C", "start", "", &exec_path.to_string_lossy()])
                        .spawn();
                }
            }
            LauncherItemKind::Calculator { result, .. } => {
                ctx.copy_text(result);
            }
            LauncherItemKind::ShellCommand { command } => {
                let _ = Command::new("powershell.exe")
                    .args(["-NoExit", "-Command", &command])
                    .spawn();
            }
            LauncherItemKind::WindowSwitch { hwnd, .. } => {
                let _ = send_ipc_command_blocking(
                    PIPE_NAME_WM,
                    &IpcCommand::Dispatch(Action::FocusWindow(hwnd)),
                );
            }
            LauncherItemKind::SystemCommand { cmd_id } => match cmd_id {
                "reload" => {
                    let _ = send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::ReloadConfig);
                }
                "restore-explorer" => {
                    trigger_emergency_restore(true);
                }
                "lock" => {
                    let _ = Command::new("rundll32.exe")
                        .args(["user32.dll,LockWorkStation"])
                        .spawn();
                }
                "sleep" => {
                    let _ = Command::new("rundll32.exe")
                        .args(["powrprof.dll,SetSuspendState", "0,1,0"])
                        .spawn();
                }
                "reboot" => {
                    let _ = Command::new("shutdown.exe").args(["/r", "/t", "0"]).spawn();
                }
                "shutdown" => {
                    let _ = Command::new("shutdown.exe").args(["/s", "/t", "0"]).spawn();
                }
                _ => {}
            },
        }

        self.dismiss(ctx);
    }

    fn dismiss(&mut self, _ctx: &egui::Context) {
        self.visible = false;
        self.query.clear();
        self.recompute_results();
        set_window_shown(false);
    }
}

impl eframe::App for LauncherApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_instant).as_secs_f32();
        self.last_frame_instant = now;

        self.absorb_loaded_icons(ctx);
        if self.pixel_textures.is_empty() {
            // Nearest when magnified keeps pixel edges hard; linear when shrunk to the row size.
            let options = egui::TextureOptions {
                magnification: egui::TextureFilter::Nearest,
                ..egui::TextureOptions::LINEAR
            };
            for icon in PixelIcon::ALL {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [pixel_icons::SIZE, pixel_icons::SIZE],
                    &icon.rgba(),
                );
                let tex = ctx.load_texture(format!("pixel-icon-{icon:?}"), image, options);
                self.pixel_textures.insert(icon, tex);
            }
        }

        if self.summon_requested.swap(false, Ordering::SeqCst) {
            if self.visible {
                self.dismiss(ctx);
                return;
            }
            self.visible = true;
            self.anim_progress = 0.0;
            self.summoned_at = Some(now);
            set_window_shown(true);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        // Hidden: request nothing. A hidden window can't paint, and asking eframe to repaint it
        // makes its event loop spin at 100% of a core. Summons re-show the window from the IPC
        // thread, which brings frames back.
        if !self.visible {
            return;
        }

        // Activation can lose the foreground race to whatever had focus at summon time; keep
        // asking briefly until Windows hands the keyboard to the launcher.
        let os_focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if !os_focused
            && self
                .summoned_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(750))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint_after(Duration::from_millis(30));
        }

        // Phase 7.1: Animate summon fade at active monitor's refresh rate
        if self.anim_progress < 1.0 {
            self.anim_progress = (self.anim_progress + dt * 8.5).min(1.0);
            ctx.request_repaint_after(Duration::from_secs_f64(1.0 / (self.target_fps as f64)));
        }

        // Keyboard navigation. Up/Down/Tab are consumed here so the always-focused search box
        // never sees them (it would otherwise move its caret instead of the selection).
        let mut trigger_exec = None;
        let mut dismiss = false;
        let count = self.results.len();
        ctx.input_mut(|i| {
            let down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
            let up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                || i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab);
            if i.key_pressed(egui::Key::Escape) {
                dismiss = true;
            } else if down && count > 0 {
                self.selected_idx = (self.selected_idx + 1) % count;
                self.scroll_to_selected = true;
            } else if up && count > 0 {
                self.selected_idx = (self.selected_idx + count - 1) % count;
                self.scroll_to_selected = true;
            } else if i.key_pressed(egui::Key::Enter) {
                trigger_exec = Some(i.modifiers.ctrl && i.modifiers.shift);
            }
        });
        if dismiss {
            self.dismiss(ctx);
            return;
        }
        if let Some(admin_flag) = trigger_exec {
            self.execute_selected(admin_flag, ctx);
            return;
        }

        // Rofi's built-in default theme (Solarized light). Overridable from launcher.css.
        let sheet = self.css.read().clone();
        let fade = self.anim_progress;
        let color = |name: &str, r: u8, g: u8, b: u8| {
            let c = sheet.var_color(name, chickenwm_core::config::RgbaColor { r, g, b, a: 255 });
            egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, (c.a as f32 * fade) as u8)
        };
        let background = color("--rofi-background", 0xFD, 0xF6, 0xE3);
        let foreground = color("--rofi-foreground", 0x00, 0x2B, 0x36);
        let lightbg = color("--rofi-lightbg", 0xEE, 0xE8, 0xD5);
        let lightfg = color("--rofi-lightfg", 0x58, 0x68, 0x75);
        // Selected row: the system accent (written by the theme engine); rofi's colors otherwise.
        let selected_bg = if sheet.custom_properties.contains_key("--rofi-selected-bg") {
            color("--rofi-selected-bg", 0, 0, 0)
        } else {
            lightfg
        };
        let selected_fg = if sheet.custom_properties.contains_key("--rofi-selected-fg") {
            color("--rofi-selected-fg", 0, 0, 0)
        } else {
            lightbg
        };
        let icon_tint = egui::Color32::from_white_alpha((255.0 * fade) as u8);
        let font = egui::FontId::monospace(15.0);
        let row_h = 24.0;
        let icon_px = row_h - 4.0;

        let window_frame = egui::Frame::NONE
            .fill(background)
            .stroke(egui::Stroke::new(1.0_f32, foreground))
            .corner_radius(egui::CornerRadius::ZERO)
            .inner_margin(egui::Margin::same(5));

        let mut icon_requests = Vec::new();
        let mut clicked_idx = None;
        egui::CentralPanel::default()
            .frame(window_frame)
            .show(ctx, |ui| {
                // inputbar: "drun:" prompt + entry
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(egui::RichText::new("drun").font(font.clone()).color(foreground));
                    ui.label(egui::RichText::new(":").font(font.clone()).color(foreground));
                    ui.add_space(5.0);
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .font(font.clone())
                            .text_color(foreground)
                            .frame(false)
                            .lock_focus(true)
                            .desired_width(f32::INFINITY),
                    );
                    resp.request_focus();
                    if resp.changed() {
                        self.recompute_results();
                        self.scroll_to_selected = true;
                    }
                });

                // listview border: 2px dashed separator above the list
                ui.add_space(3.0);
                let (sep, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 2.0), egui::Sense::hover());
                ui.painter().extend(egui::Shape::dashed_line(
                    &[sep.left_center(), sep.right_center()],
                    egui::Stroke::new(2.0_f32, foreground),
                    6.0,
                    4.0,
                ));
                ui.add_space(2.0);

                ui.spacing_mut().scroll.bar_width = 4.0;
                ui.spacing_mut().item_spacing.y = 2.0;
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    for (idx, item) in self.results.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), row_h),
                            egui::Sense::click(),
                        );
                        let selected = idx == self.selected_idx;
                        let (fill, text) = if selected {
                            (selected_bg, selected_fg)
                        } else if idx % 2 == 1 {
                            (lightbg, foreground)
                        } else {
                            (background, foreground)
                        };
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(rect, 0.0, fill);

                        let icon_rect = egui::Rect::from_min_size(
                            rect.left_top() + egui::vec2(3.0, (row_h - icon_px) / 2.0),
                            egui::vec2(icon_px, icon_px),
                        );
                        let full_uv =
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        if let Some(tex) =
                            pixel_icon_for(&item.kind).and_then(|i| self.pixel_textures.get(&i))
                        {
                            painter.image(tex.id(), icon_rect, full_uv, icon_tint);
                        } else if let LauncherItemKind::Application { exec_path } = &item.kind {
                            match self.icon_cache.get(exec_path) {
                                Some(IconSlot::Ready(tex)) => {
                                    painter.image(
                                        tex.id(),
                                        icon_rect,
                                        egui::Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        icon_tint,
                                    );
                                }
                                Some(IconSlot::Pending | IconSlot::Missing) => {}
                                None => icon_requests.push(exec_path.clone()),
                            }
                        }
                        painter.text(
                            egui::pos2(icon_rect.right() + 6.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            &item.title,
                            font.clone(),
                            text,
                        );

                        if selected && self.scroll_to_selected {
                            resp.scroll_to_me(None);
                        }
                        let resp = resp.on_hover_text(&item.subtitle);
                        if resp.clicked() {
                            clicked_idx = Some(idx);
                        }
                    }
                });
            });
        self.scroll_to_selected = false;

        for path in icon_requests {
            self.icon_cache.insert(path.clone(), IconSlot::Pending);
            self.icons.request(path);
        }
        if let Some(ci) = clicked_idx {
            self.selected_idx = ci;
            self.execute_selected(false, ctx);
        }
    }
}

fn main() -> Result<()> {
    init_tracing("chickenwm_launcher");
    let _failsafe = spawn_emergency_failsafe_thread();
    let cli = Cli::parse();

    // Single instance: a second launcher would fight the first for the IPC pipe, splitting
    // summons between a visible and a stuck copy. Hand the summon to the running one and exit.
    let _instance_guard = unsafe {
        let name: Vec<u16> = "Local\\ChickenWMLauncherSingleton\0".encode_utf16().collect();
        let guard = CreateMutexW(None, true, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !cli.daemon {
                let _ = send_ipc_command_blocking(PIPE_NAME_LAUNCHER, &IpcCommand::SummonLauncher);
            }
            return Ok(());
        }
        guard
    };

    let (_doc, cfg) = bootstrap_config_and_themes()?;
    let monitors = enumerate_monitors();
    let native_hz = max_native_refresh_rate(&monitors);
    let target_fps = if cfg.misc.fps_limit > 0 {
        cfg.misc.fps_limit
    } else {
        native_hz
    };

    let css = Arc::new(RwLock::new(
        load_css_sheet("launcher.css").unwrap_or_default(),
    ));
    let css_watcher_ref = Arc::clone(&css);
    let _watcher = spawn_config_watcher(move |ev| {
        if let ConfigWatchEvent::CssChanged { file_name, .. } = ev {
            if file_name.eq_ignore_ascii_case("launcher.css") {
                if let Ok(new_sheet) = load_css_sheet("launcher.css") {
                    *css_watcher_ref.write() = new_sheet;
                }
            }
        }
    });

    let indexed_apps = Arc::new(RwLock::new(Vec::new()));
    let indexed_bg = Arc::clone(&indexed_apps);
    std::thread::spawn(move || {
        let items = index_all_applications();
        info!("Indexed {} applications and commands", items.len());
        *indexed_bg.write() = items;
    });

    let summon_requested = Arc::new(AtomicBool::new(false));
    let summon_ipc = Arc::clone(&summon_requested);
    let css_ipc = Arc::clone(&css);
    // Shared slot for the egui `Context`, populated once the window is created. The IPC thread
    // uses it to wake the winit event loop on summon: a hidden viewport stops producing frames on
    // its own, so timer-based repaints are dropped and `update()` would never observe the summon
    // flag again — `request_repaint()` from here is what guarantees the launcher re-appears.
    let egui_ctx_slot: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    let summon_ctx = Arc::clone(&egui_ctx_slot);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let _ = serve_ipc(PIPE_NAME_LAUNCHER, move |cmd| {
                let summon_ipc = Arc::clone(&summon_ipc);
                let css_ipc = Arc::clone(&css_ipc);
                let summon_ctx = Arc::clone(&summon_ctx);
                async move {
                    match cmd {
                        IpcCommand::SummonLauncher => {
                            summon_ipc.store(true, Ordering::SeqCst);
                            // A hidden window gets no frames, so show it here; its next frame
                            // sees the summon. (When already open, update() toggles it closed.)
                            if !LAUNCHER_SHOWN.load(Ordering::SeqCst) {
                                set_window_shown(true);
                            }
                            if let Some(ctx) = summon_ctx.get() {
                                ctx.request_repaint();
                            }
                            IpcResponse::Ok {
                                message: "Launcher summoned (<10ms)".to_string(),
                            }
                        }
                        IpcCommand::ReloadConfig => {
                            if let Ok(new_sheet) = load_css_sheet("launcher.css") {
                                *css_ipc.write() = new_sheet;
                            }
                            IpcResponse::Ok {
                                message: "Reloaded launcher.css".to_string(),
                            }
                        }
                        _ => IpcResponse::Ok {
                            message: "Ignored".to_string(),
                        },
                    }
                }
            })
            .await;
        });
    });

    let start_visible = !cli.daemon;
    let viewport = egui::ViewportBuilder::default()
        .with_title("ChickenWM Launcher")
        .with_inner_size([640.0, 440.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_taskbar(false)
        .with_visible(start_visible);

    let native_options = eframe::NativeOptions {
        viewport,
        // Open centered on the primary monitor, like rofi.
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "ChickenWM Launcher",
        native_options,
        Box::new(move |cc| {
            // Hand the live egui context to the IPC thread so summons can wake a hidden window.
            let _ = egui_ctx_slot.set(cc.egui_ctx.clone());
            if let Ok(handle) = cc.window_handle() {
                if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                    LAUNCHER_HWND.store(win32.hwnd.get(), Ordering::SeqCst);
                }
            }
            LAUNCHER_SHOWN.store(start_visible, Ordering::SeqCst);
            if !start_visible {
                hide_after_first_frame();
            }
            Ok(Box::new(LauncherApp::new(
                indexed_apps,
                css,
                summon_requested,
                start_visible,
                target_fps,
                IconLoader::spawn(cc.egui_ctx.clone()),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculator_plugin() {
        let res = evaluate_math_expression("144 * 1.5").unwrap();
        assert!((res - 216.0).abs() < 1e-6);
        let res2 = evaluate_math_expression("(10 + 2) * 3 ^ 2").unwrap();
        assert!((res2 - 108.0).abs() < 1e-6);
    }
}
