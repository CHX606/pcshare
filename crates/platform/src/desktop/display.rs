//! 看屏时把被控端某块显示器临时切到控制端的分辨率，结束后改回。
//!
//! 只用 `CDS_FULLSCREEN` 做临时切换：不写注册表，不改用户保存的显示设置；
//! 正常结束时由调用方显式还原，进程意外退出时 Windows 也会丢弃这个临时模式。

/// 一个显示模式（物理像素）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    pub bits: u32,
}

/// 一次切换的结果。`applied` 为 `None` 表示没有更合适的模式，屏幕保持原样。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayFitOutcome {
    pub device: String,
    pub previous: DisplayMode,
    pub applied: Option<DisplayMode>,
    pub note: Option<String>,
}

/// 两个宽高比的相对差（0 表示完全一致）。
fn aspect_gap(w: u32, h: u32, tw: u32, th: u32) -> f64 {
    let a = w as f64 / h.max(1) as f64;
    let t = tw as f64 / th.max(1) as f64;
    (a - t).abs() / t
}

/// 面积差的对数距离，越小越接近。
fn area_gap(w: u32, h: u32, tw: u32, th: u32) -> f64 {
    ((w as f64 * h as f64) / (tw as f64 * th as f64).max(1.0)).ln().abs()
}

/// 视为“同一宽高比”的容差（1366×768 与 16:9 相差约 0.1%）。
const SAME_ASPECT: f64 = 0.01;

/// 从显示器支持的模式里挑出最贴近 `want` 的一个；与当前模式相同或不比当前更贴近时返回 `None`。
///
/// 规则：有完全相同的分辨率就用它；否则在同宽高比的模式里挑面积最接近的；
/// 都没有时，只有宽高比明显比当前更接近才切换。刷新率尽量保持当前值，色深不变。
pub fn choose_mode(current: DisplayMode, modes: &[DisplayMode], want: (u32, u32)) -> Option<DisplayMode> {
    let (tw, th) = want;
    if tw == 0 || th == 0 {
        return None;
    }
    // 横竖方向不同（比如对方竖屏）时不动
    if (current.width >= current.height) != (tw >= th) {
        return None;
    }
    let same_bits: Vec<DisplayMode> = modes.iter().copied().filter(|m| m.bits == current.bits).collect();
    let pool = if same_bits.is_empty() { modes.to_vec() } else { same_bits };
    let pick_size = |pred: &dyn Fn(&DisplayMode) -> bool| {
        pool.iter()
            .filter(|m| pred(m))
            .min_by(|a, b| {
                area_gap(a.width, a.height, tw, th)
                    .total_cmp(&area_gap(b.width, b.height, tw, th))
                    .then(b.width.cmp(&a.width))
            })
            .map(|m| (m.width, m.height))
    };
    let exact = pick_size(&|m| m.width == tw && m.height == th);
    let size = exact
        .or_else(|| pick_size(&|m| aspect_gap(m.width, m.height, tw, th) <= SAME_ASPECT))
        .or_else(|| {
            let best = pool
                .iter()
                .map(|m| aspect_gap(m.width, m.height, tw, th))
                .min_by(f64::total_cmp)?;
            let now = aspect_gap(current.width, current.height, tw, th);
            if best + SAME_ASPECT >= now {
                return None;
            }
            pick_size(&|m| (aspect_gap(m.width, m.height, tw, th) - best).abs() < 1e-9)
        })?;
    if size == (current.width, current.height) {
        return None;
    }
    // 同一同宽高比的候选比当前更差时（例如当前已同比例且更接近面积）不切换
    if exact.is_none()
        && aspect_gap(current.width, current.height, tw, th) <= SAME_ASPECT
        && area_gap(current.width, current.height, tw, th) <= area_gap(size.0, size.1, tw, th)
    {
        return None;
    }
    let same_size = pool.iter().filter(|m| (m.width, m.height) == size);
    let hz = same_size
        .clone()
        .map(|m| m.hz)
        .min_by_key(|&hz| (hz != current.hz, hz > current.hz, u32::MAX - hz))?;
    same_size.copied().find(|m| m.hz == hz)
}

#[cfg(windows)]
pub use win::{fit_monitor, restore_display};

#[cfg(windows)]
mod win {
    use super::{DisplayFitOutcome, DisplayMode, choose_mode};
    use crate::PlatformError;
    use crate::win_util::wide_null;
    use windows::Win32::Graphics::Gdi::{
        CDS_FULLSCREEN, CDS_TEST, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE,
        DISP_CHANGE_SUCCESSFUL, DMDO_DEFAULT, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
        DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
        ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsExW,
    };
    use windows::core::PCWSTR;

    /// `dmDisplayFlags` 里的隔行扫描标记。
    const INTERLACED: u32 = 0x2;

    fn read_mode(device: &[u16], index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsExW(
                PCWSTR(device.as_ptr()),
                index,
                &mut dm,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        };
        ok.as_bool().then_some(dm)
    }

    fn plain(dm: &DEVMODEW) -> DisplayMode {
        DisplayMode {
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            hz: dm.dmDisplayFrequency,
            bits: dm.dmBitsPerPel,
        }
    }

    fn change(device: &[u16], dm: Option<&DEVMODEW>, flags: CDS_TYPE) -> DISP_CHANGE {
        unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(device.as_ptr()),
                dm.map(|d| d as *const DEVMODEW),
                None,
                flags,
                None,
            )
        }
    }

    /// 把第 `index` 块显示器临时切到最贴近 `width × height` 的受支持模式。
    pub fn fit_monitor(index: u32, width: u32, height: u32) -> Result<DisplayFitOutcome, PlatformError> {
        let name = crate::win_media_input::monitor_device_name(index)
            .ok_or_else(|| PlatformError::NotFound("找不到所选的显示器".into()))?;
        let device = wide_null(&name);
        let current = read_mode(&device, ENUM_CURRENT_SETTINGS)
            .ok_or_else(|| PlatformError::System("读不到对方显示器的当前分辨率".into()))?;
        let previous = plain(&current);
        let outcome = |applied, note: Option<&str>| DisplayFitOutcome {
            device: name.clone(),
            previous,
            applied,
            note: note.map(Into::into),
        };
        // 旋转过的屏幕，枚举出来的模式方向不可靠，不动它
        let orientation = unsafe { current.Anonymous1.Anonymous2.dmDisplayOrientation };
        if orientation != DMDO_DEFAULT {
            return Ok(outcome(None, Some("对方屏幕处于旋转状态，保持原分辨率")));
        }
        let mut modes = Vec::new();
        for i in 0..4096u32 {
            let Some(dm) = read_mode(&device, ENUM_DISPLAY_SETTINGS_MODE(i)) else { break };
            let interlaced = unsafe { dm.Anonymous2.dmDisplayFlags } & INTERLACED != 0;
            if !interlaced && dm.dmPelsWidth >= 640 && dm.dmPelsHeight >= 480 {
                modes.push(plain(&dm));
            }
        }
        let Some(target) = choose_mode(previous, &modes, (width, height)) else {
            let note = if (previous.width, previous.height) == (width, height) {
                None
            } else {
                Some("对方显示器没有更接近本机的分辨率，保持原样")
            };
            return Ok(outcome(None, note));
        };
        let mut dm = current;
        dm.dmPelsWidth = target.width;
        dm.dmPelsHeight = target.height;
        dm.dmDisplayFrequency = target.hz;
        dm.dmBitsPerPel = target.bits;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL;
        if change(&device, Some(&dm), CDS_TEST) != DISP_CHANGE_SUCCESSFUL {
            return Ok(outcome(None, Some("对方显卡拒绝了这个分辨率，保持原样")));
        }
        let result = change(&device, Some(&dm), CDS_FULLSCREEN);
        if result != DISP_CHANGE_SUCCESSFUL {
            return Err(PlatformError::System(format!("切换对方分辨率失败（代码 {}）", result.0)));
        }
        Ok(outcome(Some(target), None))
    }

    /// 让显示器回到用户保存的设置（撤销 `fit_monitor` 的临时模式）。
    pub fn restore_display(device: &str) -> Result<(), PlatformError> {
        let wide = wide_null(device);
        let result = change(&wide, None, CDS_TYPE(0));
        if result != DISP_CHANGE_SUCCESSFUL {
            return Err(PlatformError::System(format!("还原对方分辨率失败（代码 {}）", result.0)));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn fit_monitor(_: u32, _: u32, _: u32) -> Result<DisplayFitOutcome, crate::PlatformError> {
    Err(crate::PlatformError::Unsupported("调整分辨率需要 Windows".into()))
}

#[cfg(not(windows))]
pub fn restore_display(_: &str) -> Result<(), crate::PlatformError> {
    Err(crate::PlatformError::Unsupported("调整分辨率需要 Windows".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: u32, height: u32, hz: u32) -> DisplayMode {
        DisplayMode { width, height, hz, bits: 32 }
    }

    fn modes() -> Vec<DisplayMode> {
        vec![
            m(2560, 1440, 144),
            m(2560, 1440, 60),
            m(1920, 1080, 144),
            m(1920, 1080, 60),
            m(1600, 900, 60),
            m(1366, 768, 60),
            m(1280, 1024, 60),
            m(1280, 720, 60),
            m(1024, 768, 60),
        ]
    }

    #[test]
    fn exact_size_wins_and_keeps_refresh_rate() {
        assert_eq!(choose_mode(m(2560, 1440, 144), &modes(), (1920, 1080)), Some(m(1920, 1080, 144)));
        assert_eq!(choose_mode(m(2560, 1440, 60), &modes(), (1920, 1080)), Some(m(1920, 1080, 60)));
    }

    #[test]
    fn same_aspect_picks_closest_area() {
        // 本机 3840×2160：对方最大同比例是 2560×1440
        assert_eq!(choose_mode(m(1920, 1080, 60), &modes(), (3840, 2160)), Some(m(2560, 1440, 60)));
        // 1366×768 与 16:9 视为同比例
        assert_eq!(choose_mode(m(1920, 1080, 60), &modes(), (1360, 765)), Some(m(1366, 768, 60)));
    }

    #[test]
    fn already_matching_or_no_better_mode_keeps_current() {
        assert_eq!(choose_mode(m(1920, 1080, 60), &modes(), (1920, 1080)), None);
        // 本机 16:10，对方没有 16:10 模式，当前 16:9 已经是最接近的比例
        assert_eq!(choose_mode(m(1920, 1080, 60), &modes(), (1920, 1200)), None);
        // 本机竖屏不动
        assert_eq!(choose_mode(m(1920, 1080, 60), &modes(), (1080, 1920)), None);
        assert_eq!(choose_mode(m(1920, 1080, 60), &[], (1280, 720)), None);
    }

    #[test]
    fn switches_aspect_only_when_clearly_closer() {
        // 对方当前 5:4，本机 16:9，有 16:9 模式时切过去
        assert_eq!(choose_mode(m(1280, 1024, 60), &modes(), (1920, 1080)), Some(m(1920, 1080, 60)));
        // 本机 4:3，对方当前 16:9，有 1024×768
        assert_eq!(choose_mode(m(1920, 1080, 60), &modes(), (1600, 1200)), Some(m(1024, 768, 60)));
    }

    #[test]
    fn falls_back_to_other_color_depths_and_refresh_rates() {
        let only16 = vec![DisplayMode { width: 1920, height: 1080, hz: 75, bits: 16 }];
        assert_eq!(choose_mode(m(2560, 1440, 60), &only16, (1920, 1080)), Some(only16[0]));
        let hz = vec![m(1920, 1080, 50), m(1920, 1080, 30), m(1920, 1080, 100)];
        assert_eq!(choose_mode(m(2560, 1440, 60), &hz, (1920, 1080)), Some(m(1920, 1080, 50)));
    }
}
