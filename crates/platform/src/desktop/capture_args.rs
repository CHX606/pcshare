use super::{CaptureArea, CaptureSource, Encoding};
use tokio::process::Command;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Encoder {
    Nvidia,
    Intel,
    Amd,
    MediaFoundation,
    Software,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum CapturePath {
    Graphics,
    Staged,
    Gdi,
}

const OPTIONS_NVIDIA: &[&str] = &[
    "-preset",
    "p2",
    "-tune",
    "ull",
    "-rc",
    "cbr",
    "-rc-lookahead",
    "0",
    "-zerolatency",
    "1",
];
const OPTIONS_INTEL: &[&str] = &[
    "-preset",
    "veryfast",
    "-async_depth",
    "1",
    "-look_ahead",
    "0",
];
const OPTIONS_AMD: &[&str] = &[
    "-usage",
    "ultralowlatency",
    "-quality",
    "speed",
    "-rc",
    "cbr",
];
const OPTIONS_MEDIAFOUNDATION: &[&str] = &[
    "-hw_encoding",
    "1",
    "-scenario",
    "display_remoting",
    "-rate_control",
    "ld_vbr",
];
const OPTIONS_SOFTWARE: &[&str] = &[
    "-preset",
    "ultrafast",
    "-tune",
    "zerolatency",
    "-pix_fmt",
    "yuv420p",
];

impl Encoder {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Nvidia => "h264_nvenc",
            Self::Intel => "h264_qsv",
            Self::Amd => "h264_amf",
            Self::MediaFoundation => "h264_mf",
            Self::Software => "libx264",
        }
    }

    fn options(self) -> &'static [&'static str] {
        match self {
            Self::Nvidia => OPTIONS_NVIDIA,
            Self::Intel => OPTIONS_INTEL,
            Self::Amd => OPTIONS_AMD,
            Self::MediaFoundation => OPTIONS_MEDIAFOUNDATION,
            Self::Software => OPTIONS_SOFTWARE,
        }
    }
}

pub(super) fn configure(
    cmd: &mut Command,
    source: CaptureSource,
    area: CaptureArea,
    cfg: Encoding,
    enc: Encoder,
    path: CapturePath,
) {
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-nostats"]);
    if path == CapturePath::Gdi {
        gdi_input(cmd, source, area, cfg, enc);
    } else {
        graphics_input(cmd, source, area, cfg, enc, path);
    }
    cmd.args(["-an", "-sn", "-dn", "-c:v", enc.name()])
        .args(enc.options());
    configure_output(cmd, cfg);
}

fn configure_output(cmd: &mut Command, cfg: Encoding) {
    let rate = cfg.bitrate.to_string();
    cmd.args([
        "-bf",
        "0",
        "-g",
        &cfg.fps.to_string(),
        "-b:v",
        &rate,
        "-maxrate",
        &rate,
        "-bufsize",
        &(cfg.bitrate / 4).to_string(),
        "-fps_mode",
        "vfr",
        "-flush_packets",
        "1",
        "-bsf:v",
        "dump_extra=freq=keyframe",
        "-stats_mux_pre",
        "pipe:2",
        "-stats_mux_pre_fmt",
        "PCSHARE_FRAME {size} {pts} {tb} {key} END",
        "-f",
        "h264",
        "pipe:1",
    ]);
}

fn graphics_input(
    cmd: &mut Command,
    source: CaptureSource,
    area: CaptureArea,
    cfg: Encoding,
    enc: Encoder,
    path: CapturePath,
) {
    let handle = match source {
        CaptureSource::Window(_) => "hwnd",
        CaptureSource::Monitor(_) => "hmonitor",
    };
    let filter = format!(
        "gfxcapture={handle}={}:max_framerate={}:width={}:height={}:resize_mode=scale:capture_border=0",
        area.handle, cfg.fps, cfg.width, cfg.height
    );
    let convert = match (enc, path) {
        (Encoder::Software, _) => ",hwdownload,format=bgra,format=yuv420p",
        (_, CapturePath::Staged) => ",hwdownload,format=bgra,format=nv12",
        (Encoder::Intel, _) => ",scale_d3d11=format=nv12,hwmap=derive_device=qsv,format=qsv",
        _ => "",
    };
    // Keep the output on D3D11 surfaces until the hardware encoder consumes it.
    cmd.args([
        "-filter_complex",
        &format!("{filter}{convert}[screen]"),
        "-map",
        "[screen]",
    ]);
}

fn gdi_input(
    cmd: &mut Command,
    source: CaptureSource,
    area: CaptureArea,
    cfg: Encoding,
    enc: Encoder,
) {
    cmd.args([
        "-f",
        "gdigrab",
        "-framerate",
        &cfg.fps.to_string(),
        "-draw_mouse",
        "1",
    ]);
    match source {
        CaptureSource::Window(hwnd) => {
            cmd.args(["-i", &format!("hwnd={hwnd}")]);
        }
        CaptureSource::Monitor(_) => {
            cmd.args([
                "-offset_x",
                &area.x.to_string(),
                "-offset_y",
                &area.y.to_string(),
                "-video_size",
                &format!("{}x{}", area.width, area.height),
                "-i",
                "desktop",
            ]);
        }
    }
    let pix_fmt = if enc == Encoder::Software {
        "yuv420p"
    } else {
        "nv12"
    };
    cmd.args([
        "-vf",
        &format!(
            "scale={}:{}:flags=fast_bilinear,format={pix_fmt}",
            cfg.width, cfg.height
        ),
    ]);
}
