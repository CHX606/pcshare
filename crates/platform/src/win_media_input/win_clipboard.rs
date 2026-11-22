use super::*;

// ──────────────────────────── 剪贴板读写（文字 + PNG/DIB，任务 3.7）────────────────────────────

struct ClipboardGuard;
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open_clipboard_with_retry() -> Result<ClipboardGuard, PlatformError> {
    for _ in 0..8 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Ok(ClipboardGuard);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(PlatformError::Busy(
        "剪贴板正被其他软件占用，请稍后再试".to_string(),
    ))
}

pub fn clipboard_sequence_impl() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

pub fn get_clipboard_impl() -> Result<ClipboardData, PlatformError> {
    let _guard = open_clipboard_with_retry()?;
    let sequence = unsafe { GetClipboardSequenceNumber() };

    let png_fmt_name = wide_null("PNG");
    let png_fmt = unsafe { RegisterClipboardFormatW(PCWSTR(png_fmt_name.as_ptr())) };
    let has_text = unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_ok();
    let has_png = png_fmt != 0 && unsafe { IsClipboardFormatAvailable(png_fmt) }.is_ok();
    let has_dib = unsafe { IsClipboardFormatAvailable(CF_DIB.0 as u32) }.is_ok();

    if has_png
        && !has_text
        && let Ok(bytes) = unsafe { read_clipboard_raw_bytes(png_fmt) }
        && let Ok((w, h)) = inspect_png_dimensions(&bytes)
    {
        return Ok(ClipboardData {
            sequence,
            content: ClipboardContent::ImagePng {
                png_bytes: bytes,
                width: w,
                height: h,
            },
        });
    }

    if has_text {
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }
            .map_err(|e| PlatformError::System(format!("读取剪贴板文字失败：{e}")))?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = unsafe { GlobalLock(hglobal) } as *const u16;
        if ptr.is_null() {
            return Err(PlatformError::System("锁定剪贴板文字内存失败".to_string()));
        }
        let max_u16 = (unsafe { GlobalSize(hglobal) }) / 2;
        let slice = unsafe { std::slice::from_raw_parts(ptr, max_u16) };
        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..len]);
        let _ = unsafe { GlobalUnlock(hglobal) };
        return Ok(ClipboardData {
            sequence,
            content: ClipboardContent::Text(text),
        });
    }

    if has_dib {
        let raw = unsafe { read_clipboard_raw_bytes(CF_DIB.0 as u32) }?;
        let (png_bytes, width, height) = dib_to_png(&raw)?;
        return Ok(ClipboardData {
            sequence,
            content: ClipboardContent::ImagePng {
                png_bytes,
                width,
                height,
            },
        });
    }

    Ok(ClipboardData {
        sequence,
        content: ClipboardContent::Empty,
    })
}

unsafe fn read_clipboard_raw_bytes(fmt: u32) -> Result<Vec<u8>, PlatformError> {
    let handle = unsafe { GetClipboardData(fmt) }
        .map_err(|e| PlatformError::System(format!("读取剪贴板图像数据失败：{e}")))?;
    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        return Err(PlatformError::System("剪贴板数据为空".to_string()));
    }
    let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
    if ptr.is_null() {
        return Err(PlatformError::System("锁定剪贴板内存失败".to_string()));
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(bytes)
}

fn inspect_png_dimensions(png_bytes: &[u8]) -> Result<(u32, u32), PlatformError> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let reader = decoder
        .read_info()
        .map_err(|e| PlatformError::InvalidInput(format!("解析 PNG 图片头失败：{e}")))?;
    let info = reader.info();
    Ok((info.width, info.height))
}

fn validate_clipboard_image_dims(width: u32, height: u32) -> Result<usize, PlatformError> {
    if width == 0 || height == 0 {
        return Err(PlatformError::InvalidInput(
            "剪贴板图像宽或高为 0".to_string(),
        ));
    }
    if width > MAX_CLIPBOARD_IMAGE_DIM || height > MAX_CLIPBOARD_IMAGE_DIM {
        return Err(PlatformError::InvalidInput(format!(
            "剪贴板图像尺寸 {width}×{height} 超过上限 {MAX_CLIPBOARD_IMAGE_DIM}×{MAX_CLIPBOARD_IMAGE_DIM}"
        )));
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板图像像素数溢出".to_string()))?;
    if pixels > MAX_CLIPBOARD_IMAGE_PIXELS {
        return Err(PlatformError::InvalidInput(format!(
            "剪贴板图像总像素数 {pixels} 超过安全上限 {MAX_CLIPBOARD_IMAGE_PIXELS}"
        )));
    }
    Ok(pixels)
}

fn dib_to_png(dib: &[u8]) -> Result<(Vec<u8>, u32, u32), PlatformError> {
    let base_hdr_size = std::mem::size_of::<BITMAPINFOHEADER>();
    if dib.len() < base_hdr_size {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图数据长度不足".to_string(),
        ));
    }
    let hdr: BITMAPINFOHEADER =
        unsafe { std::ptr::read_unaligned(dib.as_ptr() as *const BITMAPINFOHEADER) };
    let bi_size = hdr.biSize as usize;
    if bi_size < base_hdr_size || bi_size > dib.len() {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图头长度无效".to_string(),
        ));
    }
    if hdr.biWidth <= 0 || hdr.biHeight == 0 || hdr.biHeight == i32::MIN {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图尺寸无效".to_string(),
        ));
    }
    let width = hdr.biWidth as u32;
    let top_down = hdr.biHeight < 0;
    let height = hdr.biHeight.unsigned_abs();
    let bpp = hdr.biBitCount as usize;
    if bpp != 24 && bpp != 32 {
        return Err(PlatformError::Unsupported(format!(
            "暂不支持 {} 位深度的特殊剪贴板位图格式",
            hdr.biBitCount
        )));
    }
    let pixel_count = validate_clipboard_image_dims(width, height)?;

    // 仅允许 BI_RGB（24/32 位）或 32 位 BI_BITFIELDS；并仅在 BITMAPINFOHEADER(40 字节) 时追加 12 字节掩码偏移
    let (r_idx, g_idx, b_idx, a_idx) = if hdr.biCompression == BI_RGB.0 {
        (2usize, 1usize, 0usize, 3usize)
    } else if hdr.biCompression == BI_BITFIELDS.0 && bpp == 32 {
        if dib.len() < base_hdr_size + 12 {
            return Err(PlatformError::InvalidInput(
                "剪贴板 BI_BITFIELDS 位图缺少通道掩码".to_string(),
            ));
        }
        let r_mask = u32::from_le_bytes(dib[40..44].try_into().unwrap());
        let g_mask = u32::from_le_bytes(dib[44..48].try_into().unwrap());
        let b_mask = u32::from_le_bytes(dib[48..52].try_into().unwrap());
        match (r_mask, g_mask, b_mask) {
            (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) => (2, 1, 0, 3),
            (0x0000_00FF, 0x0000_FF00, 0x00FF_0000) => (0, 1, 2, 3),
            _ => {
                return Err(PlatformError::Unsupported(
                    "暂不支持非标准通道掩码的剪贴板位图".to_string(),
                ));
            }
        }
    } else {
        return Err(PlatformError::Unsupported(format!(
            "暂不支持压缩类型 {} 的剪贴板位图",
            hdr.biCompression
        )));
    };

    let extra_masks = if hdr.biSize as usize == base_hdr_size && hdr.biCompression == BI_BITFIELDS.0
    {
        12usize
    } else {
        0usize
    };
    let clr_bytes = (hdr.biClrUsed as usize)
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图调色板长度溢出".to_string()))?;
    let header_offset = bi_size
        .checked_add(extra_masks)
        .and_then(|v| v.checked_add(clr_bytes))
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图偏移溢出".to_string()))?;
    let row_bits = (width as usize)
        .checked_mul(bpp)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图行宽溢出".to_string()))?;
    let row_stride = row_bits
        .div_ceil(32)
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图行步长溢出".to_string()))?;
    let pixel_bytes = row_stride
        .checked_mul(height as usize)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图数据长度溢出".to_string()))?;
    let needed = header_offset
        .checked_add(pixel_bytes)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图总长度溢出".to_string()))?;
    if dib.len() < needed {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图像素数据不完整".to_string(),
        ));
    }

    let rgba_len = pixel_count
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板图像缓冲区溢出".to_string()))?;
    let mut rgba = vec![0u8; rgba_len];
    let mut any_nonzero_alpha = false;
    for y in 0..(height as usize) {
        let src_y = if top_down { y } else { (height as usize) - 1 - y };
        let src_row = &dib[header_offset + src_y * row_stride..];
        let dst_row = &mut rgba[y * (width as usize) * 4..(y + 1) * (width as usize) * 4];
        for x in 0..(width as usize) {
            if bpp == 32 {
                let px = &src_row[x * 4..x * 4 + 4];
                let a = px[a_idx];
                if a != 0 {
                    any_nonzero_alpha = true;
                }
                dst_row[x * 4] = px[r_idx];
                dst_row[x * 4 + 1] = px[g_idx];
                dst_row[x * 4 + 2] = px[b_idx];
                dst_row[x * 4 + 3] = a;
            } else {
                let b = src_row[x * 3];
                let g = src_row[x * 3 + 1];
                let r = src_row[x * 3 + 2];
                dst_row[x * 4] = r;
                dst_row[x * 4 + 1] = g;
                dst_row[x * 4 + 2] = b;
                dst_row[x * 4 + 3] = 255;
            }
        }
    }
    // 仅当整张 32 位位图全部像素 alpha 均为 0（传统 XRGB 填充）时才统一补 255，保留真实透明通道
    if bpp == 32 && !any_nonzero_alpha {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }

    let mut png_out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| PlatformError::System(format!("写入剪贴板 PNG 头失败：{e}")))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| PlatformError::System(format!("编码剪贴板 PNG 失败：{e}")))?;
    }
    Ok((png_out, width, height))
}

pub fn set_clipboard_text_impl(text: &str) -> Result<u32, PlatformError> {
    let wide = wide_null(text);
    let byte_len = wide.len() * 2;
    let _guard = open_clipboard_with_retry()?;
    unsafe {
        EmptyClipboard().map_err(|e| PlatformError::System(format!("清空剪贴板失败：{e}")))?;
        write_bytes_to_clipboard_format(
            CF_UNICODETEXT.0 as u32,
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, byte_len),
        )?;
        Ok(GetClipboardSequenceNumber())
    }
}

pub fn set_clipboard_png_impl(png_bytes: &[u8]) -> Result<u32, PlatformError> {
    let dib_bytes = png_to_dib(png_bytes)?;
    let _guard = open_clipboard_with_retry()?;
    unsafe {
        EmptyClipboard().map_err(|e| PlatformError::System(format!("清空剪贴板失败：{e}")))?;
        let png_fmt_name = wide_null("PNG");
        let png_fmt = RegisterClipboardFormatW(PCWSTR(png_fmt_name.as_ptr()));
        if png_fmt != 0 {
            write_bytes_to_clipboard_format(png_fmt, png_bytes)?;
        }
        write_bytes_to_clipboard_format(CF_DIB.0 as u32, &dib_bytes)?;
        Ok(GetClipboardSequenceNumber())
    }
}

unsafe fn write_bytes_to_clipboard_format(fmt: u32, bytes: &[u8]) -> Result<(), PlatformError> {
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
        .map_err(|e| PlatformError::System(format!("分配剪贴板全局内存失败：{e}")))?;
    let ptr = unsafe { GlobalLock(hglobal) } as *mut u8;
    if ptr.is_null() {
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(PlatformError::System("锁定剪贴板写入内存失败".to_string()));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    if let Err(err) = unsafe { SetClipboardData(fmt, Some(HANDLE(hglobal.0))) } {
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(PlatformError::System(format!("设置剪贴板数据失败：{err}")));
    }
    Ok(())
}

fn png_to_dib(png_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(png_bytes),
        png::Limits {
            bytes: MAX_CLIPBOARD_PNG_DECODE_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| PlatformError::InvalidInput(format!("解析剪贴板 PNG 图片失败：{e}")))?;

    let header_w = reader.info().width;
    let header_h = reader.info().height;
    let pixel_count = validate_clipboard_image_dims(header_w, header_h)?;
    let dib_pixel_bytes = pixel_count
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 PNG 像素字节数溢出".to_string()))?;
    let bi_size_image = u32::try_from(dib_pixel_bytes)
        .map_err(|_| PlatformError::InvalidInput("剪贴板 PNG 像素长度超出 DIB 上限".to_string()))?;

    let buf_size = reader
        .output_buffer_size()
        .filter(|&sz| sz <= MAX_CLIPBOARD_PNG_DECODE_BYTES)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 PNG 图像尺寸过大".to_string()))?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| PlatformError::InvalidInput(format!("解码剪贴板 PNG 帧失败：{e}")))?;
    if info.width != header_w || info.height != header_h {
        return Err(PlatformError::InvalidInput(
            "剪贴板 PNG 帧尺寸与图像头不一致".to_string(),
        ));
    }
    let width = info.width;
    let height = info.height;
    let src = &buf[..info.buffer_size()];

    let hdr = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: height as i32, // bottom-up DIB for maximum Windows app compatibility
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: bi_size_image,
        ..Default::default()
    };

    let hdr_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let total_len = hdr_size
        .checked_add(dib_pixel_bytes)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 DIB 总长度溢出".to_string()))?;
    let mut out = vec![0u8; total_len];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const BITMAPINFOHEADER as *const u8,
            out.as_mut_ptr(),
            hdr_size,
        );
    }
    let pixels = &mut out[hdr_size..];
    for y in 0..(height as usize) {
        let dst_y = (height as usize) - 1 - y;
        let dst_row =
            &mut pixels[dst_y * (width as usize) * 4..(dst_y + 1) * (width as usize) * 4];
        for x in 0..(width as usize) {
            let (r, g, b, a) = match info.color_type {
                png::ColorType::Rgba => {
                    let i = (y * (width as usize) + x) * 4;
                    (src[i], src[i + 1], src[i + 2], src[i + 3])
                }
                png::ColorType::Rgb => {
                    let i = (y * (width as usize) + x) * 3;
                    (src[i], src[i + 1], src[i + 2], 255)
                }
                png::ColorType::GrayscaleAlpha => {
                    let i = (y * (width as usize) + x) * 2;
                    (src[i], src[i], src[i], src[i + 1])
                }
                png::ColorType::Grayscale | png::ColorType::Indexed => {
                    let v = src[y * (width as usize) + x];
                    (v, v, v, 255)
                }
            };
            dst_row[x * 4] = b;
            dst_row[x * 4 + 1] = g;
            dst_row[x * 4 + 2] = r;
            dst_row[x * 4 + 3] = a;
        }
    }
    Ok(out)
}

// ──────────────────────────── 拖出文件到系统桌面 / 资源管理器 ────────────────────────────

pub fn start_file_drag_out_sta(paths: Vec<PathBuf>) -> Result<bool, PlatformError> {
    if paths.is_empty() {
        return Err(PlatformError::InvalidInput("未选择要拖出的文件".to_string()));
    }
    let mut cleaned_paths = Vec::with_capacity(paths.len());
    for p in &paths {
        if std::fs::symlink_metadata(p).is_err() {
            return Err(PlatformError::NotFound(format!(
                "找不到准备拖出的本地文件：{}",
                p.display()
            )));
        }
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.clone());
        cleaned_paths.push(strip_verbatim_prefix(&abs));
    }

    // RAII 守卫必须在所有 OLE/COM 对象之前声明，保证任意早返回分支均会先释放对象再 OleUninitialize
    let _ole = OleGuard::init()?;

    unsafe {
        // 确保当前 STA 线程已建立消息队列并持有隐藏消息窗口，避免无窗口线程拖出被立即取消
        let mut dummy_msg = MSG::default();
        let _ = PeekMessageW(&mut dummy_msg, None, 0, 0, PM_REMOVE);
        let static_cls = wide_null("STATIC");
        let helper_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_cls.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .ok()
        .map(HwndGuard);

        let mut pidls = PidlList(Vec::with_capacity(cleaned_paths.len()));
        for clean in &cleaned_paths {
            let wide = wide_null(clean);
            let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
            if pidl.is_null() {
                return Err(PlatformError::NotFound(format!(
                    "无法为待拖出路径创建系统外壳项：{clean}"
                )));
            }
            pidls.0.push(pidl);
        }

        let const_pidls: Vec<*const ITEMIDLIST> = pidls.0.iter().map(|&p| p as *const _).collect();
        let item_array = SHCreateShellItemArrayFromIDLists(&const_pidls)
            .map_err(|e| PlatformError::System(format!("构建拖拽文件列表失败：{e}")))?;
        drop(pidls);

        let data_obj: IDataObject = item_array
            .BindToHandler(None, &BHID_DataObject)
            .map_err(|e| PlatformError::System(format!("创建系统拖拽数据对象失败：{e}")))?;

        let effect = SHDoDragDrop(
            helper_hwnd.as_ref().map(|g| g.0),
            &data_obj,
            None::<&IDropSource>,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
        )
        .map_err(|e| PlatformError::System(format!("系统拖拽会话失败：{e}")))?;
        Ok(effect.0 != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_capturable_desktop_only_allows_default_in_interactive_session() {
        let default_ok = DesktopEnvironment {
            session_id: 1,
            console_session_id: 1,
            elevated: false,
            input_desktop: Some("Default".to_string()),
        };
        assert!(ensure_capturable_desktop(&default_ok).is_ok());

        for blocked in [
            DesktopEnvironment {
                session_id: 0,
                console_session_id: 1,
                elevated: true,
                input_desktop: Some("Default".to_string()),
            },
            DesktopEnvironment {
                session_id: 1,
                console_session_id: 1,
                elevated: false,
                input_desktop: None,
            },
            DesktopEnvironment {
                session_id: 1,
                console_session_id: 1,
                elevated: false,
                input_desktop: Some("Winlogon".to_string()),
            },
            DesktopEnvironment {
                session_id: 1,
                console_session_id: 1,
                elevated: false,
                input_desktop: Some("Screen-saver".to_string()),
            },
        ] {
            assert!(
                matches!(
                    ensure_capturable_desktop(&blocked),
                    Err(PlatformError::PermissionDenied(_))
                ),
                "expected PermissionDenied for {blocked:?}"
            );
        }
    }

    #[test]
    fn dib_to_png_handles_v4_v5_bitfields_and_preserves_true_alpha() {
        // 构造 2x1 的 BITMAPV5HEADER (124 字节) + BI_BITFIELDS，含 1 个透明像素 (a=0) 与 1 个半透明像素 (a=128)
        let mut dib = vec![0u8; 124 + 8];
        let hdr = BITMAPINFOHEADER {
            biSize: 124,
            biWidth: 2,
            biHeight: -1, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_BITFIELDS.0,
            biSizeImage: 8,
            ..Default::default()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const BITMAPINFOHEADER as *const u8,
                dib.as_mut_ptr(),
                40,
            );
        }
        // V4/V5 通道掩码位于头部内偏移 40..52（BGRA）
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        // 像素 0: BGRA(10, 20, 30, 0) 完全透明；像素 1: BGRA(40, 50, 60, 128) 半透明
        dib[124..132].copy_from_slice(&[10, 20, 30, 0, 40, 50, 60, 128]);

        let (png_bytes, w, h) = dib_to_png(&dib).expect("V5 BI_BITFIELDS DIB should convert");
        assert_eq!((w, h), (2, 1));
        let mut reader = png::Decoder::new(Cursor::new(&png_bytes))
            .read_info()
            .unwrap();
        let mut out = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!(&out[..info.buffer_size()], &[30, 20, 10, 0, 60, 50, 40, 128]);
    }

    #[test]
    fn dib_to_png_fills_alpha_only_when_entire_image_alpha_is_zero() {
        // 2x1 BITMAPINFOHEADER (40 字节) + BI_BITFIELDS (12 字节掩码) + 全 0 alpha
        let mut dib = vec![0u8; 40 + 12 + 8];
        let hdr = BITMAPINFOHEADER {
            biSize: 40,
            biWidth: 2,
            biHeight: -1,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_BITFIELDS.0,
            ..Default::default()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const BITMAPINFOHEADER as *const u8,
                dib.as_mut_ptr(),
                40,
            );
        }
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        dib[52..60].copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);

        let (png_bytes, w, h) = dib_to_png(&dib).expect("V1 BI_BITFIELDS DIB should convert");
        assert_eq!((w, h), (2, 1));
        let mut reader = png::Decoder::new(Cursor::new(&png_bytes))
            .read_info()
            .unwrap();
        let mut out = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!(&out[..info.buffer_size()], &[3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn dib_to_png_rejects_unsupported_compression_and_png_to_dib_rejects_oversized() {
        let mut bad_dib = vec![0u8; 40 + 16];
        let hdr = BITMAPINFOHEADER {
            biSize: 40,
            biWidth: 2,
            biHeight: 2,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 4, // BI_JPEG
            ..Default::default()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const BITMAPINFOHEADER as *const u8,
                bad_dib.as_mut_ptr(),
                40,
            );
        }
        assert!(matches!(
            dib_to_png(&bad_dib),
            Err(PlatformError::Unsupported(_))
        ));

        // 校验尺寸与总像素上限防护
        assert!(matches!(
            validate_clipboard_image_dims(MAX_CLIPBOARD_IMAGE_DIM + 1, 1),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_clipboard_image_dims(8192, 8192),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(validate_clipboard_image_dims(7680, 4320).is_ok());
    }

    #[test]
    fn combo_inputs_set_extended_flag_and_compute_stuck_keys_on_partial_send() {
        let vks = parse_key_combo("Ctrl+Shift+Delete").expect("parse combo");
        let inputs = build_combo_inputs(&vks);
        assert_eq!(inputs.len(), 6);
        // Delete 是扩展键，按下和抬起都必须带 KEYEVENTF_EXTENDEDKEY
        let del_down = unsafe { inputs[2].Anonymous.ki };
        let del_up = unsafe { inputs[3].Anonymous.ki };
        assert_eq!(del_down.wVk, VK_DELETE);
        assert_ne!(del_down.dwFlags.0 & KEYEVENTF_EXTENDEDKEY.0, 0);
        assert_ne!(del_up.dwFlags.0 & KEYEVENTF_EXTENDEDKEY.0, 0);
        assert_ne!(del_up.dwFlags.0 & KEYEVENTF_KEYUP.0, 0);

        // 若 SendInput 只发出前 2 项（Ctrl down, Shift down），应按逆序补释放 [Shift, Ctrl]
        let stuck = compute_stuck_keys_to_release(&inputs, 2);
        assert_eq!(stuck, vec![VK_SHIFT, VK_CONTROL]);
        // 若发出前 4 项（Ctrl/Shift/Del down + Del up），剩 [Shift, Ctrl]
        assert_eq!(
            compute_stuck_keys_to_release(&inputs, 4),
            vec![VK_SHIFT, VK_CONTROL]
        );
        // 全部发出则无卡键
        assert!(compute_stuck_keys_to_release(&inputs, 6).is_empty());
    }
}
