use anyhow::Context as _;
use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, HBITMAP, HDC, SelectObject,
    },
    System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW,
    },
    UI::{
        Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW},
        WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx, HICON},
    },
};

const APP_NAME: &str = "SSH Tunnel Keeper";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const ICON_SIZE: u32 = 64;

pub(super) fn autostart_is_enabled() -> anyhow::Result<bool> {
    let Some(key) = open_run_key(KEY_QUERY_VALUE)? else {
        return Ok(false);
    };
    let name = wide_null(APP_NAME);
    let mut value_size = 0;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut value_size,
        )
    };
    match status {
        ERROR_SUCCESS => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        error => Err(registry_error("query the Windows startup registry", error)),
    }
}

pub(super) fn set_autostart(executable: Option<&Path>) -> anyhow::Result<()> {
    let Some(executable) = executable else {
        return delete_run_value();
    };
    let mut command_line = OsString::from("\"");
    command_line.push(executable);
    command_line.push("\" --hidden");
    set_run_value(&command_line)
}

pub(super) fn extract_executable_icon(path: &Path) -> Option<Vec<u8>> {
    let path = wide_null(path.as_os_str());
    let mut file_info = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            path.as_ptr(),
            0,
            &mut file_info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if result == 0 || file_info.hIcon.is_null() {
        return None;
    }
    let icon = WindowsIcon(file_info.hIcon);
    let black = draw_icon(&icon, 0)?;
    let white = draw_icon(&icon, 255)?;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for (black, white) in black.chunks_exact(4).zip(white.chunks_exact(4)) {
        let transparent = [
            white[0].saturating_sub(black[0]),
            white[1].saturating_sub(black[1]),
            white[2].saturating_sub(black[2]),
        ]
        .into_iter()
        .max()
        .unwrap_or(255);
        let alpha = 255u8.saturating_sub(transparent);
        let restore = |channel: u8| -> u8 {
            if alpha == 0 {
                0
            } else {
                ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255)
                    as u8
            }
        };
        rgba.extend_from_slice(&[
            restore(black[2]),
            restore(black[1]),
            restore(black[0]),
            alpha,
        ]);
    }
    let image = image::RgbaImage::from_raw(ICON_SIZE, ICON_SIZE, rgba)?;
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .ok()?;
    Some(output.into_inner())
}

fn open_run_key(access: u32) -> anyhow::Result<Option<RegistryKey>> {
    let subkey = wide_null(RUN_KEY);
    let mut key = ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key) };
    match status {
        ERROR_SUCCESS => Ok(Some(RegistryKey(key))),
        ERROR_FILE_NOT_FOUND => Ok(None),
        error => Err(registry_error("open the Windows startup registry", error)),
    }
}

fn set_run_value(command_line: &OsStr) -> anyhow::Result<()> {
    let subkey = wide_null(RUN_KEY);
    let mut key = ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(registry_error(
            "open the Windows startup registry for writing",
            status,
        ));
    }
    let key = RegistryKey(key);
    let name = wide_null(APP_NAME);
    let value = wide_null(command_line);
    let value_size = u32::try_from(value.len() * size_of::<u16>())
        .context("Windows startup command is too long")?;
    let status = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            value_size,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error(
            "update the Windows startup registry",
            status,
        ))
    }
}

fn delete_run_value() -> anyhow::Result<()> {
    let Some(key) = open_run_key(KEY_SET_VALUE)? else {
        return Ok(());
    };
    let name = wide_null(APP_NAME);
    let status = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
    match status {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        error => Err(registry_error("update the Windows startup registry", error)),
    }
}

fn draw_icon(icon: &WindowsIcon, background: u8) -> Option<Vec<u8>> {
    let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
    if dc.is_null() {
        return None;
    }
    let dc = MemoryDeviceContext(dc);
    let bitmap_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: ICON_SIZE as i32,
            biHeight: -(ICON_SIZE as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: ICON_SIZE * ICON_SIZE * 4,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut bits = ptr::null_mut();
    let bitmap = unsafe {
        CreateDIBSection(
            dc.0,
            &bitmap_info,
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        )
    };
    if bitmap.is_null() || bits.is_null() {
        return None;
    }
    let bitmap = GdiBitmap(bitmap);
    let previous = unsafe { SelectObject(dc.0, bitmap.0) };
    if previous.is_null() {
        return None;
    }

    let byte_len = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), byte_len) };
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[background, background, background, 255]);
    }
    let drawn = unsafe {
        DrawIconEx(
            dc.0,
            0,
            0,
            icon.0,
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            0,
            ptr::null_mut(),
            DI_NORMAL,
        )
    } != 0;
    let output = drawn.then(|| pixels.to_vec());
    unsafe {
        SelectObject(dc.0, previous);
    }
    output
}

fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn registry_error(action: &str, status: WIN32_ERROR) -> anyhow::Error {
    let error = std::io::Error::from_raw_os_error(status as i32);
    anyhow::anyhow!("failed to {action}: {error}")
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

struct WindowsIcon(HICON);

impl Drop for WindowsIcon {
    fn drop(&mut self) {
        unsafe {
            DestroyIcon(self.0);
        }
    }
}

struct MemoryDeviceContext(HDC);

impl Drop for MemoryDeviceContext {
    fn drop(&mut self) {
        unsafe {
            DeleteDC(self.0);
        }
    }
}

struct GdiBitmap(HBITMAP);

impl Drop for GdiBitmap {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}
