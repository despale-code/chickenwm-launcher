//! Phase 7: Flow-Style Application Launcher (`chickenwm-launcher`)
//! - 7.1: Instant Daemon Architecture (`<10ms` summon via IPC `\\.\pipe\chickenwm-launcher-ipc`)
//!        with fade + scale animation synced to active monitor refresh rate / `fps_limit`
//! - 7.2: Multi-Source Application Indexer (Start Menu, Desktop, `PATH`, UWP packages, `SHGetFileInfoW` icons)
//! - 7.3: `nucleo-matcher` Fuzzy Search & 5 Core Plugins:
//!        App Runner (`Ctrl+Shift+Enter` RunAs Admin), Calculator (`= 144 * 1.5`),
//!        Shell Command (`> cargo build`), Window Switcher (`w `), System Commands (`lock`, `sleep`, `reboot`, `shutdown`, `reload`, `restore-explorer`)
//! - 7.4: Live CSS Styling (`~/.config/chickenwm/launcher.css`)

// Release builds are windowed apps: no console window when launched from the shell, a
// hotkey or the launcher. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icons;
mod pixel_icons;
mod quick;

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
    send_ipc_command_blocking, serve_ipc, Action, IpcCommand, IpcResponse, PIPE_NAME_DAEMON,
    PIPE_NAME_LAUNCHER, PIPE_NAME_WM,
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

/// Maximum result rows kept in the list.
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
    /// Headless render (see `chickenwm_core::headless`): parsed there, accepted here.
    #[arg(long, hide = true)]
    render_to: Option<String>,
    #[arg(long, hide = true)]
    render_size: Option<String>,
    #[arg(long, hide = true)]
    render_scale: Option<String>,
    #[arg(long, hide = true)]
    render_frames: Option<String>,
}

/// Right-hand label for an open window: the workspace it lives on.
fn workspace_label(ws: u32) -> &'static str {
    const LABELS: [&str; 10] = ["WS 1", "WS 2", "WS 3", "WS 4", "WS 5", "WS 6", "WS 7", "WS 8", "WS 9", "WS 10"];
    ws.checked_sub(1).and_then(|i| LABELS.get(i as usize)).copied().unwrap_or("Window")
}

#[derive(Debug, Clone)]
pub(crate) enum LauncherItemKind {
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
    /// Enter copies `text` (conversion results, emoji, clipboard history).
    CopyText {
        text: String,
        kind_label: &'static str,
    },
    /// A URL or file opened through the shell (web search, recent files).
    Open {
        target: String,
        kind_label: &'static str,
    },
    /// `kill <name>`: ends these processes.
    Kill {
        pids: Vec<u32>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct LauncherEntry {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) badge: &'static str,
    pub(crate) kind: LauncherItemKind,
    pub(crate) score: u32,
}

/// No console window for helper processes (`shutdown.exe` is a console program).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
use std::os::windows::process::CommandExt;

/// Opens an app, shortcut, document or URI the way Explorer does; `admin` = "Run as
/// administrator" (`runas` verb, Windows shows the UAC prompt).
fn shell_open(target: &std::path::Path, admin: bool) {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = HSTRING::from(if admin { "runas" } else { "open" });
    let file = HSTRING::from(target.as_os_str());
    let dir = target.parent().filter(|p| p.is_dir()).map(|p| HSTRING::from(p.as_os_str()));
    unsafe {
        ShellExecuteW(
            None,
            &verb,
            &file,
            windows::core::PCWSTR::null(),
            dir.as_ref().map_or(windows::core::PCWSTR::null(), |d| windows::core::PCWSTR(d.as_ptr())),
            SW_SHOWNORMAL,
        );
    }
}

/// URI opened by the Snip entry (and `SUPER SHIFT, S`).
const SNIP_URI: &str = "ms-screenclip:";

/// ChickenWM's core apps, listed in drun with pixel-art icons: (title, subtitle, exe). Optional
/// ones (Hen Browser, ...) are listed only when installed, through the Start Menu shortcuts the
/// installer creates.
const CHICKENWM_APPS: [(&str, &str, &str); 2] = [
    ("ChickenWM Files", "ChickenWM file explorer (Super + E)", "chickenwm-files.exe"),
    ("ChickenWM Settings", "ChickenWM settings (Super + I)", "chickenwm-settings.exe"),
];

/// Built-in pixel-art icon for an entry; `None` means use the app's own icon.
fn pixel_icon_for(kind: &LauncherItemKind) -> Option<PixelIcon> {
    match kind {
        LauncherItemKind::Application { exec_path } => {
            if exec_path.as_os_str() == SNIP_URI {
                return Some(PixelIcon::Snip);
            }
            // Everything else, ChickenWM's own programs included, shows its real icon (the exes and
            // their Start Menu shortcuts carry the ChickenWM artwork).
            None
        }
        LauncherItemKind::Calculator { .. } => Some(PixelIcon::Calculator),
        LauncherItemKind::ShellCommand { .. } => Some(PixelIcon::Shell),
        LauncherItemKind::WindowSwitch { .. } => Some(PixelIcon::WindowSwitch),
        LauncherItemKind::Kill { .. } => Some(PixelIcon::Shell),
        LauncherItemKind::CopyText { .. } | LauncherItemKind::Open { .. } => None,
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

/// What frecency is keyed on: the app path, or the system command id.
fn frecency_key(kind: &LauncherItemKind) -> String {
    match kind {
        LauncherItemKind::Application { exec_path } => exec_path.to_string_lossy().to_lowercase(),
        LauncherItemKind::SystemCommand { cmd_id } => format!("cmd:{cmd_id}"),
        _ => String::new(),
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
    // ChickenWM's own screenshot tool when installed, else Windows' Snipping Tool.
    let shot = std::env::current_exe()
        .ok()
        .map(|e| e.with_file_name("chickenwm-shot.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(SNIP_URI));
    entries.push(LauncherEntry {
        title: "Screenshot".to_string(),
        subtitle: "Capture a region, a window or the screen (Print Screen or Super + Shift + S)".to_string(),
        badge: "SYS",
        kind: LauncherItemKind::Application { exec_path: shot },
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
    index_uwp_apps(&mut entries, &mut seen_names);

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

fn index_uwp_apps(out: &mut Vec<LauncherEntry>, seen: &mut HashSet<String>) {
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::*;
    use windows::core::PCWSTR;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let path: Vec<u16> = "shell:AppsFolder\0".encode_utf16().collect();
        let Ok(folder): windows::core::Result<IShellItem> = SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) else {
            return;
        };
        let Ok(enum_items): windows::core::Result<IEnumShellItems> = folder.BindToHandler(None, &BHID_EnumItems) else {
            return;
        };
        let mut item_arr: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        while enum_items.Next(&mut item_arr, Some(&mut fetched)).is_ok() && fetched == 1 {
            if let Some(ref item) = item_arr[0] {
                let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok().map(|p| {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.as_ptr() as _));
                    s
                });
                let aumid = item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING).ok().map(|p| {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.as_ptr() as _));
                    s
                });
                if let (Some(name), Some(aumid)) = (name, aumid) {
                    let lower = name.to_ascii_lowercase();
                    if !name.is_empty() && !aumid.is_empty() && !lower.contains("uninstall") && seen.insert(lower) {
                        out.push(LauncherEntry {
                            title: name,
                            subtitle: format!("shell:AppsFolder\\{aumid}"),
                            badge: "APP",
                            kind: LauncherItemKind::Application {
                                exec_path: PathBuf::from(format!("shell:AppsFolder\\{aumid}")),
                            },
                            score: 0,
                        });
                    }
                }
            }
        }
    }
}

fn get_aliases(name: &str) -> &'static [&'static str] {
    let lower = name.to_ascii_lowercase();
    if lower.contains("windows terminal") || lower.contains("terminal") {
        &["wt", "wt.exe", "term"]
    } else if lower.contains("command prompt") {
        &["cmd", "cmd.exe"]
    } else if lower.contains("calculator") {
        &["calc", "calc.exe"]
    } else if lower.contains("powershell") {
        &["ps", "pwsh", "powershell.exe"]
    } else if lower.contains("visual studio code") || lower.contains("code") {
        &["code", "vscode"]
    } else if lower.contains("file explorer") || lower.contains("explorer") {
        &["explorer", "files"]
    } else if lower.contains("task manager") {
        &["taskmgr"]
    } else if lower.contains("registry editor") {
        &["regedit"]
    } else if lower.contains("control panel") {
        &["control"]
    } else if lower.contains("notepad") {
        &["notepad", "text"]
    } else {
        &[]
    }
}

fn spawn_apps_watcher(indexed: Arc<RwLock<Vec<LauncherEntry>>>) {
    std::thread::spawn(move || {
        use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut watcher) = RecommendedWatcher::new(tx, Config::default()) else {
            return;
        };

        let mut watch_paths = Vec::new();
        if let Ok(prog_data) = std::env::var("ProgramData") {
            watch_paths.push(PathBuf::from(prog_data).join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        if let Ok(app_data) = std::env::var("APPDATA") {
            watch_paths.push(PathBuf::from(app_data).join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        if let Some(desktop) = dirs::desktop_dir() {
            watch_paths.push(desktop);
        }

        for p in watch_paths {
            if p.exists() {
                let _ = watcher.watch(&p, RecursiveMode::Recursive);
            }
        }

        while let Ok(_event) = rx.recv() {
            std::thread::sleep(Duration::from_millis(500));
            while rx.try_recv().is_ok() {}

            let refreshed = index_all_applications();
            *indexed.write() = refreshed;
            info!("launcher: background watcher refreshed application index");
        }
    });
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
    /// Keeps the UI font in step with `--font-family` in launcher.css.
    font: chickenwm_core::fonts::EguiFont,
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
    /// The launcher has held the keyboard focus since it was summoned; losing it then closes it.
    had_focus: bool,
    /// Recent clipboard text (memory only), for `c `.
    clip: quick::ClipHistory,
    /// ECB exchange rates for currency conversion.
    rates: quick::SharedRates,
    /// Launch counts / recency, boosting what you open most.
    frecency: quick::Frecency,
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
            font: Default::default(),
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
            had_focus: false,
            clip: quick::spawn_clipboard_watch(),
            rates: quick::spawn_rates(),
            frecency: quick::Frecency::load(),
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

        // Prefix Cheat-Sheet (`?` or `? <filter>`)
        if let Some(help_rows) = quick::handle_cheat_sheet_prefix(q) {
            self.results = help_rows;
            self.selected_idx = 0;
            return;
        }

        // Plugin 3: Window Switcher (`w <query>`)
        if let Some(win_rows) = quick::handle_window_prefix(q) {
            self.results = win_rows;
            self.selected_idx = 0;
            return;
        }

        // Clipboard history (`c `, or `c` alone for everything).
        if let Some(clip_rows) = quick::handle_clipboard_prefix(q, &self.clip, MAX_RESULTS) {
            self.results = clip_rows;
            self.selected_idx = 0;
            return;
        }

        // Emoji (`:thumbs`).
        if let Some(eq) = q.strip_prefix(':') {
            for (emoji, keywords) in quick::emoji_search(eq, MAX_RESULTS * 2) {
                self.results.push(LauncherEntry {
                    title: format!("{emoji}  {}", keywords.split(' ').next().unwrap_or_default()),
                    subtitle: keywords.to_owned(),
                    badge: "EMOJI",
                    kind: LauncherItemKind::CopyText { text: emoji.to_owned(), kind_label: "Emoji" },
                    score: 0,
                });
            }
            self.selected_idx = 0;
            return;
        }

        // `kill <process>`
        if let Some(kill_rows) = quick::handle_kill_prefix(q, MAX_RESULTS) {
            self.results = kill_rows;
            self.selected_idx = 0;
            return;
        }

        // Recent files (`r `, or `r` alone).
        if let Some(recent_rows) = quick::handle_recent_prefix(q, MAX_RESULTS) {
            self.results = recent_rows;
            self.selected_idx = 0;
            return;
        }

        // Web search shortcuts (`g …`, `yt …`, `gh …`, `ddg …`, `wiki …`, `maps …`).
        if let Some((site, url)) = quick::web_search(q) {
            self.results.push(LauncherEntry {
                title: format!("Search {site} for '{}'", q.split_once(' ').map_or("", |x| x.1).trim()),
                subtitle: url.clone(),
                badge: "WEB",
                kind: LauncherItemKind::Open { target: url, kind_label: "Web" },
                score: u32::MAX,
            });
            self.selected_idx = 0;
            return;
        }

        // Conversions sit on top of the app matches (`10 km to mi`, `100 usd in eur`).
        let conversion = quick::convert_units(q)
            .map(|c| (c, "Convert"))
            .or_else(|| self.rates.read().as_ref().and_then(|r| quick::convert_currency(q, r)).map(|c| (c, "Currency")));
        if let Some(((result, detail), kind_label)) = conversion {
            self.results.push(LauncherEntry {
                title: format!("= {result}"),
                subtitle: detail,
                badge: "CONV",
                kind: LauncherItemKind::CopyText { text: result.split(' ').next().unwrap_or_default().to_owned(), kind_label },
                score: u32::MAX,
            });
        }

        // Plugin 4 & 5: Fuzzy App Runner & System Commands via `nucleo-matcher`
        let apps = self.indexed_apps.read();
        if q.is_empty() {
            // Most used (and recently used) first, then the usual order, plus a `?` prefix cheat-sheet row.
            let mut list: Vec<(usize, u32, &LauncherEntry)> =
                apps.iter().enumerate().map(|(i, e)| (i, self.frecency.score(&frecency_key(&e.kind)), e)).collect();
            list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            self.results.extend(list.into_iter().take(MAX_RESULTS - 1).map(|(_, _, e)| e.clone()));
            self.results.push(LauncherEntry {
                title: "Prefix Cheat-Sheet  (type ? for full help)".to_string(),
                subtitle: "w windows \u{f444} c clipboard \u{f444} r recent \u{f444} kill proc \u{f444} = math \u{f444} > cmd \u{f444} :emoji \u{f444} g/yt/gh web".to_string(),
                badge: "HELP",
                kind: LauncherItemKind::CopyText {
                    text: "?".to_string(),
                    kind_label: "Cheat-Sheet",
                },
                score: 0,
            });
            self.selected_idx = 0;
            return;
        }

        let pat = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let q_lower = q.to_ascii_lowercase();
        let mut char_buf = Vec::new();
        let mut char_buf2 = Vec::new();

        for item in apps.iter() {
            let mut best_score = None;

            // 1. Score title
            char_buf.clear();
            let haystack = Utf32Str::new(&item.title, &mut char_buf);
            if let Some(s) = pat.score(haystack, &mut self.matcher) {
                best_score = Some(s);
            }

            // 2. Score subtitle / executable path
            char_buf2.clear();
            let sub_haystack = Utf32Str::new(&item.subtitle, &mut char_buf2);
            if let Some(s) = pat.score(sub_haystack, &mut self.matcher) {
                best_score = Some(best_score.map_or(s, |b| b.max(s)));
            }

            // 3. Score aliases
            for &alias in get_aliases(&item.title) {
                if alias == q_lower {
                    best_score = Some(best_score.map_or(800, |b| b.max(800)));
                } else if alias.starts_with(&q_lower) {
                    best_score = Some(best_score.map_or(600, |b| b.max(600)));
                }
            }

            // 4. Exact prefix boost (+500 if title starts with query)
            if item.title.to_ascii_lowercase().starts_with(&q_lower) {
                best_score = Some(best_score.unwrap_or(400) + 500);
            }

            if let Some(score) = best_score {
                let mut matched = item.clone();
                // Frecency clamped to 120 as a tie-breaker
                let frec = self.frecency.score(&frecency_key(&item.kind)).min(120);
                matched.score = score + frec;
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

        if matches!(entry.kind, LauncherItemKind::Application { .. } | LauncherItemKind::SystemCommand { .. }) {
            self.frecency.record(&frecency_key(&entry.kind));
        }
        match entry.kind {
            LauncherItemKind::CopyText { text, .. } => {
                ctx.copy_text(text);
            }
            LauncherItemKind::Open { target, .. } => {
                shell_open(Path::new(&target), false);
            }
            LauncherItemKind::Kill { pids } => {
                let n = quick::kill(&pids);
                info!("killed {n} of {} process(es)", pids.len());
            }
            LauncherItemKind::Application { exec_path, .. } => {
                // Straight through the shell (like a double-click): no console window flashes
                // up, and shortcuts, .url files and URIs all just work.
                shell_open(&exec_path, run_as_admin);
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
                    let _ = Command::new("shutdown.exe").args(["/r", "/t", "0"]).creation_flags(CREATE_NO_WINDOW).spawn();
                }
                "shutdown" => {
                    let _ = Command::new("shutdown.exe").args(["/s", "/t", "0"]).creation_flags(CREATE_NO_WINDOW).spawn();
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

/// Whether a mouse button is down with the pointer outside the launcher window.
fn clicked_outside(ctx: &egui::Context) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let down = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
        .iter()
        .any(|k| unsafe { GetAsyncKeyState(i32::from(k.0)) } as u16 & 0x8000 != 0);
    if !down {
        return false;
    }
    let Some(rect) = ctx.input(|i| i.viewport().outer_rect) else {
        return false;
    };
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return false;
    }
    let ppp = ctx.pixels_per_point();
    !rect.contains(egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp))
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
            self.had_focus = false;
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

        // Clicking anywhere else closes the launcher: either it loses the focus it had, or (when
        // it never won the focus race) a mouse click lands outside its window.
        if os_focused {
            self.had_focus = true;
        } else if self.had_focus {
            self.dismiss(ctx);
            return;
        } else if self.summoned_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(750)) {
            if clicked_outside(ctx) {
                self.dismiss(ctx);
                return;
            }
            ctx.request_repaint_after(Duration::from_millis(60));
        }

        // Phase 7.1: Animate summon fade at active monitor's refresh rate
        if self.anim_progress < 1.0 {
            self.anim_progress = (self.anim_progress + dt * 8.5).min(1.0);
            ctx.request_repaint_after(Duration::from_secs_f64(1.0 / (self.target_fps as f64)));
        }

        // Keyboard navigation. Up/Down/Tab are consumed here so the always-focused search box
        // never sees them (it would otherwise move its caret instead of the selection).
        let mut trigger_exec = None;
        let mut open_containing_folder = false;
        let mut dismiss = false;
        let mut forget_selected = false;
        let mut toggle_pin_selected = false;
        let count = self.results.len();
        ctx.input_mut(|i| {
            let down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
            let up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                || i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab);
            if i.key_pressed(egui::Key::Escape) {
                dismiss = true;
            } else if i.consume_key(egui::Modifiers::CTRL, egui::Key::Delete) {
                forget_selected = true;
            } else if i.consume_key(egui::Modifiers::CTRL, egui::Key::P) {
                toggle_pin_selected = true;
            } else if down && count > 0 {
                self.selected_idx = (self.selected_idx + 1) % count;
                self.scroll_to_selected = true;
            } else if up && count > 0 {
                self.selected_idx = (self.selected_idx + count - 1) % count;
                self.scroll_to_selected = true;
            } else if i.modifiers.alt && i.key_pressed(egui::Key::Enter) {
                open_containing_folder = true;
            } else if i.key_pressed(egui::Key::Enter) {
                trigger_exec = Some(i.modifiers.ctrl || (i.modifiers.ctrl && i.modifiers.shift));
            }
        });
        if dismiss {
            self.dismiss(ctx);
            return;
        }
        if open_containing_folder {
            if let Some(entry) = self.results.get(self.selected_idx) {
                if let LauncherItemKind::Application { exec_path, .. } = &entry.kind {
                    let path_str = exec_path.to_string_lossy().to_string();
                    if path_str.starts_with("shell:") {
                        shell_open(exec_path, false);
                    } else if let Some(parent) = exec_path.parent() {
                        shell_open(parent, false);
                    }
                }
            }
            self.dismiss(ctx);
            return;
        }
        if forget_selected {
            if let Some(entry) = self.results.get(self.selected_idx) {
                let key = frecency_key(&entry.kind);
                if !key.is_empty() && self.frecency.forget(&key) {
                    self.recompute_results();
                }
            }
        }
        if toggle_pin_selected {
            if let Some(entry) = self.results.get(self.selected_idx) {
                if let LauncherItemKind::CopyText { text, kind_label } = &entry.kind {
                    if *kind_label == "Clipboard" || *kind_label == "Pinned Clip" {
                        let txt = text.clone();
                        quick::toggle_pin_clipboard(&self.clip, &txt);
                        self.recompute_results();
                    }
                }
            }
        }
        if let Some(admin_flag) = trigger_exec {
            self.execute_selected(admin_flag, ctx);
            return;
        }

        // ChickenWM launcher design: a floating card with a large search field, rich result
        // rows and a key-hint footer. Colors come from launcher.css (the system palette).
        let sheet = self.css.read().clone();
        self.font.sync(ctx, &sheet, false);
        let t = self.anim_progress;
        let ease = 1.0 - (1.0 - t).powi(3);
        let col = |name: &str, r: u8, g: u8, b: u8, a: u8| {
            let c = sheet.var_color(name, chickenwm_core::config::RgbaColor { r, g, b, a });
            egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, (c.a as f32 * ease) as u8)
        };
        let card_bg = col("--surface-color", 251, 251, 251, 250);
        let fg = col("--fg-color", 36, 35, 33, 255);
        let sub = col("--subtext-color", 98, 95, 89, 255);
        let accent = col("--accent-color", 36, 35, 33, 255);
        let selected_bg = col("--selected-bg", 36, 35, 33, 34);
        let border = col("--border-color", 36, 35, 33, 40);
        let radius = sheet.var_px("--border-radius", 16.0).clamp(0.0, 32.0);
        let row_h = sheet.var_px("--row-height", 48.0).clamp(32.0, 80.0);
        let icon_px = sheet.var_px("--icon-size", 32.0).clamp(16.0, row_h - 8.0);
        let font_size = sheet.var_px("--font-size", 15.0).clamp(11.0, 22.0);
        let icon_tint = egui::Color32::from_white_alpha((255.0 * ease) as u8);
        let title_font = egui::FontId::proportional(font_size);
        let sub_font = egui::FontId::proportional(font_size - 2.5);
        let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        const MARGIN: f32 = 16.0; // room for the card's shadow
        const GAP: f32 = 2.0;

        let mut icon_requests = Vec::new();
        let mut clicked_idx = None;
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            // Card settles in: slight rise while fading in.
            let full = ui.max_rect().shrink(MARGIN);
            let card = full.translate(egui::vec2(0.0, (1.0 - ease) * 10.0));
            let painter = ui.painter();
            let shadow = egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: egui::Color32::from_black_alpha((60.0 * ease) as u8) };
            painter.add(shadow.as_shape(card, egui::CornerRadius::same(radius as u8)));
            painter.rect(card, radius, card_bg, egui::Stroke::new(1.0_f32, border), egui::StrokeKind::Inside);

            let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(card.shrink2(egui::vec2(14.0, 12.0))));
            let ui = &mut inner;

            // ── Search field ──
            let (field, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
            let lens_c = egui::pos2(field.left() + 16.0, field.center().y - 1.0);
            ui.painter().circle_stroke(lens_c, 7.0, egui::Stroke::new(2.0_f32, sub));
            ui.painter().line_segment(
                [lens_c + egui::vec2(5.0, 5.0), lens_c + egui::vec2(10.0, 10.0)],
                egui::Stroke::new(2.4_f32, sub),
            );
            let edit_rect = egui::Rect::from_min_max(egui::pos2(field.left() + 36.0, field.top()), egui::pos2(field.right() - 90.0, field.bottom()));
            let resp = ui.put(
                edit_rect,
                egui::TextEdit::singleline(&mut self.query)
                    .font(egui::FontId::proportional(font_size + 6.0))
                    .text_color(fg)
                    .hint_text(egui::RichText::new("Search apps and windows \u{f444} = math \u{f444} > command").color(sub))
                    .frame(false)
                    .lock_focus(true)
                    .vertical_align(egui::Align::Center),
            );
            resp.request_focus();
            if resp.changed() {
                self.recompute_results();
                self.scroll_to_selected = true;
            }
            let count = self.results.len();
            ui.painter().text(
                egui::pos2(field.right() - 4.0, field.center().y),
                egui::Align2::RIGHT_CENTER,
                if count == 1 { "1 result".to_string() } else { format!("{count} results") },
                sub_font.clone(),
                sub,
            );
            ui.painter().hline(card.x_range().shrink(1.0), field.bottom() + 6.0, egui::Stroke::new(1.0_f32, border));
            ui.add_space(12.0);

            // ── Results ──
            let footer_h = 26.0;
            let list_h = (ui.available_height() - footer_h).max(row_h);
            ui.spacing_mut().scroll.bar_width = 4.0;
            ui.spacing_mut().item_spacing.y = GAP;
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                if self.results.is_empty() {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| ui.label(egui::RichText::new("Nothing matches").color(sub).size(font_size)));
                    return;
                }
                // Selection highlight glides to the selected row.
                let top = ui.cursor().top();
                let target = self.selected_idx as f32 * (row_h + GAP);
                let y = ui.ctx().animate_value_with_time(egui::Id::new("launcher-sel"), target, 0.12);
                let hl = egui::Rect::from_min_size(egui::pos2(ui.max_rect().left(), top + y), egui::vec2(ui.available_width(), row_h));
                ui.painter().rect_filled(hl, 10.0, selected_bg);
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(hl.left_top() + egui::vec2(0.0, 12.0), egui::vec2(3.0, row_h - 24.0)),
                    2.0,
                    accent,
                );

                for (idx, item) in self.results.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                    let selected = idx == self.selected_idx;
                    let painter = ui.painter_at(rect);
                    if resp.hovered() && !selected {
                        painter.rect_filled(rect, 10.0, selected_bg.gamma_multiply(0.45));
                    }
                    let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 14.0 + icon_px / 2.0, rect.center().y), egui::vec2(icon_px, icon_px));
                    if let Some(tex) = pixel_icon_for(&item.kind).and_then(|i| self.pixel_textures.get(&i)) {
                        painter.image(tex.id(), icon_rect, full_uv, icon_tint);
                    } else if let LauncherItemKind::Application { exec_path } = &item.kind {
                        match self.icon_cache.get(exec_path) {
                            Some(IconSlot::Ready(tex)) => {
                                painter.image(tex.id(), icon_rect, full_uv, icon_tint);
                            }
                            Some(IconSlot::Pending | IconSlot::Missing) => {
                                painter.rect_filled(icon_rect.shrink(3.0), 8.0, border);
                            }
                            None => icon_requests.push(exec_path.clone()),
                        }
                    }
                    let text_x = icon_rect.right() + 12.0;
                    let (kind_label, title_text, title_col, title_f) = match &item.kind {
                        LauncherItemKind::Calculator { result, .. } => ("Calculator", format!("= {result}"), accent, egui::FontId::proportional(font_size + 4.0)),
                        LauncherItemKind::Application { .. } => ("App", item.title.clone(), fg, title_font.clone()),
                        LauncherItemKind::WindowSwitch { workspace_id, .. } => (workspace_label(*workspace_id), item.title.clone(), fg, title_font.clone()),
                        LauncherItemKind::ShellCommand { .. } => ("Command", item.title.clone(), fg, title_font.clone()),
                        LauncherItemKind::SystemCommand { .. } => ("System", item.title.clone(), fg, title_font.clone()),
                        LauncherItemKind::CopyText { kind_label, .. } => (*kind_label, item.title.clone(), if item.badge == "CONV" { accent } else { fg }, title_font.clone()),
                        LauncherItemKind::Open { kind_label, .. } => (*kind_label, item.title.clone(), fg, title_font.clone()),
                        LauncherItemKind::Kill { .. } => ("Process", item.title.clone(), fg, title_font.clone()),
                    };
                    let right_w = 96.0;
                    let text_clip = egui::Rect::from_min_max(egui::pos2(text_x, rect.top()), egui::pos2(rect.right() - right_w, rect.bottom()));
                    let tp = painter.with_clip_rect(text_clip);
                    let has_sub = !item.subtitle.is_empty();
                    let title_y = if has_sub { rect.center().y - 9.0 } else { rect.center().y };
                    tp.text(egui::pos2(text_x, title_y), egui::Align2::LEFT_CENTER, title_text, title_f, title_col);
                    if has_sub {
                        let subtitle = match &item.kind {
                            LauncherItemKind::Calculator { expression, .. } => format!("{expression}  \u{f444}  Enter to copy the result"),
                            _ => item.subtitle.clone(),
                        };
                        tp.text(egui::pos2(text_x, rect.center().y + 10.0), egui::Align2::LEFT_CENTER, subtitle, sub_font.clone(), sub);
                    }
                    // Right side: kind label, or the action hint on the selected row.
                    let hint = if selected { "\u{f0311} Open" } else { kind_label };
                    let hint = if selected && matches!(item.kind, LauncherItemKind::Calculator { .. } | LauncherItemKind::CopyText { .. }) { "\u{f0311} Copy" } else { hint };
                    let hint = if selected && matches!(item.kind, LauncherItemKind::Kill { .. }) { "\u{f0311} Kill" } else { hint };
                    painter.text(egui::pos2(rect.right() - 12.0, rect.center().y), egui::Align2::RIGHT_CENTER, hint, sub_font.clone(), if selected { fg } else { sub });

                    if selected && self.scroll_to_selected {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked() {
                        clicked_idx = Some(idx);
                    }
                }
            });

            // ── Footer: key hints ──
            let (footer, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), footer_h), egui::Sense::hover());
            ui.painter().hline(card.x_range().shrink(1.0), footer.top() + 2.0, egui::Stroke::new(1.0_f32, border));
            ui.painter().text(
                egui::pos2(footer.left() + 4.0, footer.center().y + 3.0),
                egui::Align2::LEFT_CENTER,
                "\u{f07d} navigate    \u{f0311} open    Ctrl+Shift+\u{f0311} run as admin    Esc close",
                egui::FontId::proportional(font_size - 3.5),
                sub,
            );
            ui.painter().text(
                egui::pos2(footer.right() - 4.0, footer.center().y + 3.0),
                egui::Align2::RIGHT_CENTER,
                "ChickenWM",
                egui::FontId::proportional(font_size - 3.5),
                sub.gamma_multiply(0.7),
            );
        });        self.scroll_to_selected = false;

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
    // `--render-to out.png`: draw one frame headless, without the singleton or the IPC pipe.
    let render = chickenwm_core::headless::RenderRequest::from_args([700.0, 520.0]);

    // Single instance: a second launcher would fight the first for the IPC pipe, splitting
    // summons between a visible and a stuck copy. Hand the summon to the running one and exit.
    let _instance_guard = if render.is_some() { None } else { Some(unsafe {
        let name: Vec<u16> = "Local\\ChickenWMLauncherSingleton\0".encode_utf16().collect();
        let guard = CreateMutexW(None, true, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !cli.daemon {
                let _ = send_ipc_command_blocking(PIPE_NAME_LAUNCHER, &IpcCommand::SummonLauncher);
            }
            return Ok(());
        }
        guard
    }) };

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
    spawn_apps_watcher(Arc::clone(&indexed_apps));

    let summon_requested = Arc::new(AtomicBool::new(false));
    if let Some(req) = render {
        // Let the app index land so the list isn't empty in the capture.
        let start = std::time::Instant::now();
        while indexed_apps.read().is_empty() && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        return chickenwm_core::headless::render_app(&req, |cc| {
            let mut app = LauncherApp::new(indexed_apps, css, summon_requested, true, target_fps, IconLoader::spawn(cc.egui_ctx.clone()));
            // CHICKENWM_TEST_QUERY="10 km to mi": capture the results for that query.
            if let Ok(q) = std::env::var("CHICKENWM_TEST_QUERY") {
                std::thread::sleep(std::time::Duration::from_millis(800)); // clipboard / rates threads
                app.query = q;
                app.recompute_results();
            }
            app
        });
    }
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
            .with_icon(egui::IconData { rgba: include_bytes!("../assets/icon-64.rgba").to_vec(), width: 64, height: 64 })
        .with_title("ChickenWM Launcher")
        .with_inner_size([700.0, 520.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_taskbar(false)
        .with_visible(start_visible);

    let native_options = eframe::NativeOptions {
        // glow, not wgpu: egui_kittest (headless feature) also enables eframe's wgpu backend,
        // which eframe would then prefer, and wgpu draws transparent windows black on Windows.
        renderer: eframe::Renderer::Glow,
        viewport,
        // Open centered on the primary monitor.
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
                    let hwnd = windows::Win32::Foundation::HWND(win32.hwnd.get() as *mut _);
                    LAUNCHER_HWND.store(win32.hwnd.get(), Ordering::SeqCst);
                    chickenwm_core::sys::apply_acrylic(hwnd, None);
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
