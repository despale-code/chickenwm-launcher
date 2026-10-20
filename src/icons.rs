//! Phase 7.2: Application icon extraction for launcher rows.
//!
//! Icons are pulled with `SHGetFileInfoW` (which resolves `.lnk` targets and per-file icons) on a
//! dedicated COM thread, converted to straight-alpha RGBA, and handed back to the UI thread, which
//! turns them into egui textures. Keeping the shell calls off the UI thread means a slow or
//! network-backed shortcut never stalls typing.

use chickenwm_core::handles::SafeHicon;
use chickenwm_core::icon_theme::{IconRequest, IconTheme};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO};

/// Decoded icon pixels, straight (non-premultiplied) RGBA, row-major top-down.
pub struct IconImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Where a shortcut's icon really lives: its icon location, or else its target. Reading the
/// `.lnk` itself would give Windows' version with the shortcut-arrow overlay.
fn shortcut_icon_source(lnk: &Path) -> Option<(PathBuf, i32)> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>().ok()?.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ).ok()?;
        let expand = |raw: &[u16]| -> Option<PathBuf> {
            let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            if len == 0 {
                return None;
            }
            let s = String::from_utf16_lossy(&raw[..len]);
            let mut out = [0u16; 1024];
            let n = ExpandEnvironmentStringsW(&HSTRING::from(s.as_str()), Some(&mut out)) as usize;
            let expanded = if n > 1 { String::from_utf16_lossy(&out[..n - 1]) } else { s };
            let p = PathBuf::from(expanded);
            p.exists().then_some(p)
        };
        let mut buf = [0u16; 1024];
        let mut index = 0i32;
        if link.GetIconLocation(&mut buf, &mut index).is_ok() {
            if let Some(p) = expand(&buf) {
                return Some((p, index));
            }
        }
        let mut target = [0u16; 1024];
        if link.GetPath(&mut target, std::ptr::null_mut(), 0).is_ok() {
            if let Some(p) = expand(&target) {
                return Some((p, 0));
            }
        }
        None
    }
}

/// A crisp 48 px icon straight from an icon resource (exe / dll / ico + index).
fn extract_resource_icon(file: &Path, index: i32) -> Option<IconImage> {
    use windows::Win32::UI::WindowsAndMessaging::PrivateExtractIconsW;
    let mut wide = [0u16; 260];
    let s: Vec<u16> = file.as_os_str().to_string_lossy().encode_utf16().collect();
    if s.len() >= wide.len() {
        return None;
    }
    wide[..s.len()].copy_from_slice(&s);
    let mut icon = [HICON::default(); 1];
    let got = unsafe { PrivateExtractIconsW(&wide, index, 48, 48, Some(&mut icon), None, 0) };
    if got == 0 || got == u32::MAX || icon[0].is_invalid() {
        return None;
    }
    unsafe {
        let _guard = SafeHicon::new(icon[0]);
        hicon_to_rgba(icon[0])
    }
}

fn shell_item_image(path: &Path) -> Option<IconImage> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT};

    let path_str = path.to_string_lossy();
    let wide = HSTRING::from(path_str.as_ref());
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&wide, None).ok()?;
        let size = SIZE { cx: 48, cy: 48 };
        let hbmp = factory.GetImage(size, SIIGBF_RESIZETOFIT).ok()?;
        let rgba = read_bitmap_bgra(hbmp, 48, 48);
        let _ = DeleteObject(hbmp);
        let mut px = rgba?;
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
        }
        Some(IconImage {
            width: 48,
            height: 48,
            rgba: px,
        })
    }
}

/// The app's icon for `path` (an `.exe`, `.lnk`, `shell:AppsFolder\...` or `.url`), without the shortcut arrow.
pub fn extract_icon_rgba(path: &Path) -> Option<IconImage> {
    let path_str = path.to_string_lossy();
    if path_str.starts_with("shell:") {
        if let Some(img) = shell_item_image(path) {
            return Some(img);
        }
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if ext == "lnk" {
        if let Some((source, index)) = shortcut_icon_source(path) {
            let source_ext = source.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
            if matches!(source_ext.as_str(), "exe" | "dll" | "ico" | "cpl" | "icl") {
                if let Some(img) = extract_resource_icon(&source, index) {
                    return Some(img);
                }
            }
            // Documents, folders, URLs: the target's own shell icon (no shortcut overlay).
            if let Some(img) = shell_icon(&source).or_else(|| shell_item_image(&source)) {
                return Some(img);
            }
        }
    } else if ext == "exe" {
        if let Some(img) = extract_resource_icon(path, 0) {
            return Some(img);
        }
    }
    shell_icon(path).or_else(|| shell_item_image(path))
}

fn shell_icon(path: &Path) -> Option<IconImage> {
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut sfi = SHFILEINFOW::default();
    unsafe {
        let res = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if res == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let _guard = SafeHicon::new(sfi.hIcon);
        hicon_to_rgba(sfi.hIcon)
    }
}

fn read_bitmap_bgra(hbm: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (width * height * 4) as usize];
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let lines = GetDIBits(
            dc,
            hbm,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        (lines > 0).then_some(buf)
    }
}

unsafe fn hicon_to_rgba(hicon: HICON) -> Option<IconImage> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut info) }.ok()?;

    let decoded = (|| {
        // Monochrome-only icons have no colour bitmap; skip them rather than render garbage.
        if info.hbmColor.is_invalid() {
            return None;
        }
        let mut bm = BITMAP::default();
        let got = unsafe {
            GetObjectW(
                info.hbmColor,
                size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
            )
        };
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut px = read_bitmap_bgra(info.hbmColor, w, h)?;

        // Older icons carry no alpha channel; transparency then lives in the AND mask
        // (mask bit set = transparent).
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            let mask = read_bitmap_bgra(info.hbmMask, w, h)?;
            for (p, m) in px.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
        }
        Some(IconImage {
            width: w as usize,
            height: h as usize,
            rgba: px,
        })
    })();

    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask);
        }
    }
    decoded
}

/// An icon-theme override for an app: `app-<name>.png` where `<name>` is the lower-case file
/// stem (`code`, `visual studio code`) or its dashed form (`visual-studio-code`).
fn themed_icon(path: &Path, theme: &IconTheme) -> Option<IconImage> {
    if theme.is_system() {
        return None;
    }
    let stem = path.file_stem()?.to_string_lossy().to_ascii_lowercase();
    let dashed = stem.split_whitespace().collect::<Vec<_>>().join("-");
    let icon_path = [stem, dashed]
        .into_iter()
        .find_map(|name| theme.resolve(&IconRequest::App(name)))?;
    let (width, height, rgba) = chickenwm_core::icon_theme::load_icon_image(&icon_path, 64)?;
    Some(IconImage {
        width,
        height,
        rgba,
    })
}

/// Background icon worker: send paths in, receive decoded icons back.
pub struct IconLoader {
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Option<IconImage>)>,
}

impl IconLoader {
    /// Spawns the worker; it wakes the UI via `ctx.request_repaint()` as each icon lands.
    pub fn spawn(ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("chickenwm-launcher-icons".to_string())
            .spawn(move || {
                // SHGetFileInfoW requires COM on the calling thread.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
                let theme = IconTheme::current();
                while let Ok(path) = req_rx.recv() {
                    let icon = themed_icon(&path, &theme).or_else(|| extract_icon_rgba(&path));
                    if res_tx.send((path, icon)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
        Self {
            requests: req_tx,
            results: res_rx,
        }
    }

    pub fn request(&self, path: PathBuf) {
        let _ = self.requests.send(path);
    }

    pub fn drain(&self) -> impl Iterator<Item = (PathBuf, Option<IconImage>)> + '_ {
        self.results.try_iter()
    }
}
