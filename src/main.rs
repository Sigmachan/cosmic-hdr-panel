// cosmic-hdr — HDR display settings panel for COSMIC Desktop

use cosmic::app::{Core, Task};
use cosmic::iced::{Alignment, Length};
use cosmic::widget::{self, column, list_column, row, settings, text, toggler};
use cosmic::{Application, ApplicationExt, Apply, Element};
use tokio::process::Command;

const APP_ID: &str = "ru.sigmachan.KmsHdr";
const BIN: &str = "/usr/local/bin/kms-hdr";
const HDR_CAL: &str = "/usr/local/lib/kms-hdr/hdr-cal.py";
const PID_FILE: &str = "/run/kms-hdr.pid";

// ── PQ math (SMPTE ST 2084 forward EOTF) ──────────────────────────────────────

/// Convert nits to PQ-encoded percentage (0–100%).
/// Useful as a readout: ~58% PQ = 203 nits (BT.2408 SDR reference),
/// ~75% PQ = 1000 nits, 100% PQ = 10 000 nits.
fn nits_to_pq_percent(nits: u32) -> f64 {
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;
    let y = (nits as f64 / 10_000.0).clamp(0.0, 1.0);
    let ym = y.powf(M1);
    ((C1 + C2 * ym) / (1.0 + C3 * ym)).max(0.0).powf(M2) * 100.0
}

// ── Connector / EDID detection ─────────────────────────────────────────────────

/// Returns (edid_path, sysfs_dir) for the first active real connector with a valid EDID.
/// Skips virtual connectors (X11 backend, winit-in-X, headless, Wayland backend).
fn find_active_connector() -> Option<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if !s.starts_with("card") || !s.contains('-') { return None; }

            // Extract connector part after "cardN-"
            let connector = match s.find('-') {
                Some(p) => &s[p + 1..],
                None => return None,
            };
            // Skip virtual / headless connectors
            if connector.starts_with("X11-")
                || connector.starts_with("Virtual-")
                || connector.starts_with("HEADLESS-")
                || connector.starts_with("WL-")
            {
                return None;
            }

            let edid = format!("/sys/class/drm/{}/edid", s);
            let ok = std::fs::read(&edid).map(|d| d.len() >= 128).unwrap_or(false);
            if ok { Some((edid, s.to_string())) } else { None }
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

// ── Daemon helpers ─────────────────────────────────────────────────────────────

/// True if kms-hdr daemon is running (PID file exists and /proc/<pid> is alive).
fn daemon_alive() -> bool {
    std::fs::read_to_string(PID_FILE).ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
        .unwrap_or(false)
}

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct HdrConf {
    sdr_nits: u32,
    peak_nits: u32,
    gamut: u32,
    max_bpc: u32,
    gamut_mode: String,
    saturation: u32,
    midtone_gamma: u32,   // 100=neutral, >100=HDR punch, <100=lift. Range 30–250.
    oled_preset: bool,
    oled_dim_min: u32,
}

impl Default for HdrConf {
    fn default() -> Self {
        Self {
            sdr_nits: 203, peak_nits: 800, gamut: 100, max_bpc: 10,
            gamut_mode: "bt2020".into(), saturation: 100,
            midtone_gamma: 100, oled_preset: false, oled_dim_min: 0,
        }
    }
}

/// NVIDIA gaming features persisted to /etc/hdr-game.conf
#[derive(Debug, Clone)]
struct NvidiaConf {
    smooth_motion: bool,
    reflex: bool,
    vibrance: i32,
    upscale: String,
    dldsr: bool,
    gs_width: u32,
    gs_height: u32,
    gs_fps: u32,
}

impl Default for NvidiaConf {
    fn default() -> Self {
        Self {
            smooth_motion: true, reflex: true, vibrance: 0,
            upscale: "fsr".into(), dldsr: false,
            gs_width: 3840, gs_height: 2160, gs_fps: 120,
        }
    }
}

fn is_nvidia() -> bool {
    std::path::Path::new("/dev/nvidia0").exists()
}

fn gpu_vendor() -> &'static str {
    if std::path::Path::new("/dev/nvidia0").exists() { return "nvidia"; }
    if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if !s.starts_with("card") || s.contains('-') { continue; }
            let vendor_path = format!("/sys/class/drm/{}/device/vendor", s);
            if let Ok(v) = std::fs::read_to_string(&vendor_path) {
                let v = v.trim();
                if v == "0x1002" { return "amd"; }
                if v == "0x8086" { return "intel"; }
            }
        }
    }
    "unknown"
}

fn nvibrant_available() -> bool {
    std::process::Command::new("which").arg("nvibrant")
        .output().map(|o| o.status.success()).unwrap_or(false)
}

fn read_conf() -> HdrConf {
    let mut c = HdrConf::default();
    if let Ok(s) = std::fs::read_to_string("/etc/kms-hdr.conf") {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "SDR_NITS"      => { if let Ok(n) = v.trim().parse() { c.sdr_nits      = n; } }
                    "PEAK_NITS"     => { if let Ok(n) = v.trim().parse() { c.peak_nits     = n; } }
                    "GAMUT"         => { if let Ok(n) = v.trim().parse() { c.gamut         = n; } }
                    "MAX_BPC"       => { if let Ok(n) = v.trim().parse() { c.max_bpc       = n; } }
                    "GAMUT_MODE"    => { c.gamut_mode = v.trim().to_owned(); }
                    "SATURATION"    => { if let Ok(n) = v.trim().parse() { c.saturation    = n; } }
                    "MIDTONE_GAMMA" => { if let Ok(n) = v.trim().parse() { c.midtone_gamma = n; } }
                    "OLED_PRESET"   => { c.oled_preset  = v.trim() == "1"; }
                    "OLED_DIM_MIN"  => { if let Ok(n) = v.trim().parse() { c.oled_dim_min  = n; } }
                    _ => {}
                }
            }
        }
    }
    c
}

fn read_nvidia_conf() -> NvidiaConf {
    let mut c = NvidiaConf::default();
    if let Ok(s) = std::fs::read_to_string("/etc/hdr-game.conf") {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim();
                match k.trim() {
                    "SMOOTH_MOTION" => { c.smooth_motion = v != "0"; }
                    "REFLEX"        => { c.reflex        = v != "0"; }
                    "VIBRANCE"      => { if let Ok(n) = v.parse() { c.vibrance = n; } }
                    "UPSCALE"       => { c.upscale = v.to_owned(); }
                    "DLDSR"         => { c.dldsr   = v == "1"; }
                    "GS_WIDTH"      => { if let Ok(n) = v.parse() { c.gs_width  = n; } }
                    "GS_HEIGHT"     => { if let Ok(n) = v.parse() { c.gs_height = n; } }
                    "GS_FPS"        => { if let Ok(n) = v.parse() { c.gs_fps    = n; } }
                    _ => {}
                }
            }
        }
    }
    c
}

fn conf_args(c: &HdrConf) -> Vec<String> {
    vec![
        "--sdr-nits".into(),      c.sdr_nits.to_string(),
        "--peak-nits".into(),     c.peak_nits.to_string(),
        "--gamut".into(),         c.gamut.to_string(),
        "--bpc".into(),           c.max_bpc.to_string(),
        "--gamut-mode".into(),    c.gamut_mode.clone(),
        "--saturation".into(),    c.saturation.to_string(),
        "--midtone-gamma".into(), c.midtone_gamma.to_string(),
        "--oled-dim-min".into(),  c.oled_dim_min.to_string(),
    ]
}

async fn write_nvidia_conf(c: NvidiaConf) -> Result<(), String> {
    let status = Command::new("pkexec")
        .args([
            BIN, "--save-game",
            &format!("SMOOTH_MOTION={}", c.smooth_motion as u8),
            &format!("REFLEX={}", c.reflex as u8),
            &format!("VIBRANCE={}", c.vibrance),
            &format!("UPSCALE={}", c.upscale),
            &format!("DLDSR={}", c.dldsr as u8),
            &format!("GS_WIDTH={}", c.gs_width),
            &format!("GS_HEIGHT={}", c.gs_height),
            &format!("GS_FPS={}", c.gs_fps),
        ])
        .status().await.map_err(|e| e.to_string())?;
    if nvibrant_available() {
        let _ = Command::new("nvibrant").arg(c.vibrance.to_string()).status().await;
    }
    if status.success() { Ok(()) } else { Err(format!("kms-hdr --save-game exited {status}")) }
}

/// Write conf and apply HDR.
/// If daemon is running: fast path — write conf only then send SIGUSR1 via --reload.
/// Otherwise: one-shot apply with VT switch.
async fn write_conf_and_apply(c: HdrConf) -> Result<(), String> {
    let oled_dim = c.oled_dim_min;
    let args = conf_args(&c);

    if daemon_alive() {
        // Fast path: write conf, signal daemon. Panel returns immediately.
        let mut save_cmd = vec![BIN, "--save-only"];
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        save_cmd.extend_from_slice(&arg_refs);
        let status = Command::new("pkexec")
            .args(&save_cmd)
            .status().await.map_err(|e| e.to_string())?;
        if !status.success() { return Err(format!("kms-hdr --save-only exited {status}")); }

        let reload = Command::new("pkexec")
            .args([BIN, "--reload"])
            .status().await.map_err(|e| e.to_string())?;
        if !reload.success() {
            // Daemon died between check and reload — fall through to direct apply
            return direct_apply(args, oled_dim).await;
        }
        setup_oled_dim(oled_dim).await;
        Ok(())
    } else {
        direct_apply(args, oled_dim).await
    }
}

async fn direct_apply(args: Vec<String>, oled_dim: u32) -> Result<(), String> {
    let mut cmd = vec![BIN, "--save"];
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    cmd.extend_from_slice(&arg_refs);
    let status = Command::new("pkexec")
        .args(&cmd)
        .status().await.map_err(|e| e.to_string())?;
    if !status.success() { return Err(format!("kms-hdr exited {status}")); }
    setup_oled_dim(oled_dim).await;
    Ok(())
}

async fn setup_oled_dim(minutes: u32) {
    let home = std::env::var("HOME").unwrap_or_default();
    let svc_dir = format!("{home}/.config/systemd/user");
    let svc_path = format!("{svc_dir}/kms-hdr-dim.service");
    let _ = std::fs::create_dir_all(&svc_dir);

    if minutes == 0 {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "kms-hdr-dim.service"]).status().await;
        let _ = std::fs::remove_file(&svc_path);
        return;
    }

    let secs = minutes * 60;
    let content = format!(
        "[Unit]\nDescription=kms-hdr OLED auto-dim (swayidle)\n\
         [Service]\nType=simple\nRestart=on-failure\n\
         ExecStart=swayidle -w timeout {secs} \"pkexec {BIN} --dim-to 50\" resume \"pkexec {BIN}\"\n\
         [Install]\nWantedBy=default.target\n"
    );
    if std::fs::write(&svc_path, content).is_ok() {
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status().await;
        let _ = Command::new("systemctl")
            .args(["--user", "enable", "--now", "kms-hdr-dim.service"]).status().await;
    }
}

async fn do_reset() -> Result<(), String> {
    let s = Command::new("pkexec").args([BIN, "reset"])
        .status().await.map_err(|e| e.to_string())?;
    if s.success() { Ok(()) } else { Err(format!("kms-hdr reset exited {s}")) }
}

fn service_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "kms-hdr.service"])
        .output()
        .map(|o| o.stdout.starts_with(b"active"))
        .unwrap_or(false)
}

// ── Display info ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct DisplayInfo {
    name: String,
    connector_dir: String,
    hdr10: bool,
    hlg: bool,
    hdr10plus: bool,
    dolby: bool,
    bt2020: bool,
    dci_p3: bool,
    dsc: bool,
    cec: bool,
    max_lum_nits: u32,
    hdmi_ver: Option<String>,
    dp_ver: Option<String>,
    is_oled: bool,
}

fn parse_edid() -> Option<DisplayInfo> {
    let (edid_path, connector_dir) = find_active_connector()?;
    let raw = std::fs::read(&edid_path).ok()?;
    let mut info = DisplayInfo { connector_dir: connector_dir.clone(), ..Default::default() };

    'desc: for i in (54..126usize).step_by(18) {
        if i + 17 >= raw.len() { break; }
        if raw[i..i+3] == [0x00, 0x00, 0x00] && raw[i+3] == 0xfc {
            let bytes: Vec<u8> = raw[i+5..].iter()
                .take(13).take_while(|&&b| b != b'\n').cloned().collect();
            info.name = String::from_utf8_lossy(&bytes).trim().to_owned();
            break 'desc;
        }
    }
    if info.name.is_empty() {
        info.name = connector_dir.find('-')
            .map(|p| connector_dir[p+1..].replace('-', " "))
            .unwrap_or_else(|| "Display".into());
    }

    let mut bs = 128usize;
    while bs + 128 <= raw.len() {
        if raw[bs] != 0x02 { bs += 128; continue; }
        let dtd = raw[bs + 2] as usize;
        let mut i = 4usize;
        while i < dtd && bs + i < raw.len() {
            let tag    = (raw[bs + i] >> 5) & 0x7;
            let length = (raw[bs + i] & 0x1f) as usize;
            if bs + i + 1 + length > raw.len() { break; }
            let data = &raw[bs + i + 1 .. bs + i + 1 + length];

            match tag {
                7 if !data.is_empty() => {
                    let payload = &data[1..];
                    match data[0] {
                        6 if !payload.is_empty() => {
                            info.hdr10 = payload[0] & 0x04 != 0;
                            info.hlg   = payload[0] & 0x08 != 0;
                            if payload.len() > 2 && payload[2] != 0 {
                                info.max_lum_nits =
                                    (50.0 * 2f64.powf(payload[2] as f64 / 32.0)) as u32;
                            }
                        }
                        5 if !payload.is_empty() => {
                            info.bt2020 = payload[0] & 0x80 != 0;
                            info.dci_p3 = payload[0] & 0x02 != 0;
                        }
                        13 => { info.hdr10plus = true; }
                        1 if payload.len() >= 3 => {
                            let oui = u32::from_le_bytes([payload[0], payload[1], payload[2], 0]);
                            if oui == 0x0000_D046 { info.dolby = true; }
                        }
                        _ => {}
                    }
                }
                3 if data.len() >= 3 => {
                    let oui = u32::from_le_bytes([data[0], data[1], data[2], 0]);
                    match oui {
                        0x0000_D046 => { info.dolby = true; }
                        0x0000_0C03 => {
                            if info.hdmi_ver.is_none() {
                                info.hdmi_ver = Some("HDMI 1.4".into());
                            }
                        }
                        0x00C4_5D00 => {
                            let max_tmds_mhz = if data.len() >= 5 { data[4] as u32 * 5 } else { 0 };
                            info.hdmi_ver = Some(if max_tmds_mhz >= 600 {
                                "HDMI 2.1".into()
                            } else {
                                "HDMI 2.0".into()
                            });
                            if data.len() >= 9 && data[8] & 0x80 != 0 { info.dsc = true; }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1 + length;
        }
        bs += 128;
    }

    if std::path::Path::new(&format!("/sys/class/drm/{}/dsc_enable", connector_dir)).exists() {
        info.dsc = true;
    }

    if connector_dir.contains("-DP-") || connector_dir.contains("-eDP-") {
        if let Ok(dpcd) = std::fs::read(format!("/sys/class/drm/{}/dpcd", connector_dir)) {
            if !dpcd.is_empty() {
                info.dp_ver = Some(match dpcd[0] {
                    0x10 => "DP 1.0".into(), 0x11 => "DP 1.1".into(),
                    0x12 => "DP 1.2".into(), 0x13 => "DP 1.3".into(),
                    0x14 => "DP 1.4".into(),
                    v if v >= 0x20 => "DP 2.x (UHBR)".into(),
                    v => format!("DP (DPCD {v:#04x})"),
                });
            }
        }
    }

    info.cec = std::path::Path::new("/dev/cec0").exists();

    info.is_oled = info.name.to_ascii_lowercase().contains("oled");
    if !info.is_oled {
        let panel_type_path = format!("/sys/class/drm/{}/panel_type", connector_dir);
        if let Ok(pt) = std::fs::read_to_string(&panel_type_path) {
            info.is_oled = pt.to_ascii_lowercase().contains("oled");
        }
    }

    Some(info)
}

// ── Calibration patterns ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum CalibPattern {
    Black, DarkGray, Gray50, White, Red, Green, Blue, SdrHdrSplit,
}

impl CalibPattern {
    fn label(self) -> &'static str {
        match self {
            Self::Black       => "Black",
            Self::DarkGray    => "5% Gray",
            Self::Gray50      => "50% Gray",
            Self::White       => "White",
            Self::Red         => "Red",
            Self::Green       => "Green",
            Self::Blue        => "Blue",
            Self::SdrHdrSplit => "SDR│HDR",
        }
    }
    fn arg(self) -> &'static str {
        match self {
            Self::Black       => "black",
            Self::DarkGray    => "darkgray",
            Self::Gray50      => "gray50",
            Self::White       => "white",
            Self::Red         => "red",
            Self::Green       => "green",
            Self::Blue        => "blue",
            Self::SdrHdrSplit => "sdr_hdr",
        }
    }
}

// ── App ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Message {
    HdrToggle(bool),
    SdrNits(u32),
    PeakNits(u32),
    Gamut(u32),
    GamutMode(usize),
    Saturation(u32),
    MidtoneGamma(u32),
    BitDepth(usize),
    Apply,
    Reset,
    Applied(Result<(), String>),
    ShowCalPat(CalibPattern),
    CalibrateHdr,
    CloseCalPat,
    OledPreset(bool),
    OledDimTimeout(u32),
    NvSmoothMotion(bool),
    NvReflex(bool),
    NvVibrance(i32),
    NvUpscale(usize),
    NvDldsr(bool),
    NvGsResolution(u32, u32),
    NvGsFps(u32),
    NvApply,
    NvApplied(Result<(), String>),
}

struct CosmicHdr {
    core: Core,
    conf: HdrConf,
    nvidia_conf: NvidiaConf,
    is_nvidia: bool,
    gpu_vendor: &'static str,
    hdr_enabled: bool,
    display: Option<DisplayInfo>,
    status: Option<String>,
    cal_child: Option<std::process::Child>,
}

impl Application for CosmicHdr {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core { &self.core }
    fn core_mut(&mut self) -> &mut Core { &mut self.core }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let gpu = gpu_vendor();
        let mut app = Self {
            core,
            conf: read_conf(),
            nvidia_conf: read_nvidia_conf(),
            is_nvidia: gpu == "nvidia",
            gpu_vendor: gpu,
            hdr_enabled: service_active(),
            display: parse_edid(),
            status: None,
            cal_child: None,
        };
        app.set_header_title("HDR & Color Pipeline".into());
        (app, Task::none())
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::HdrToggle(on) => {
                self.hdr_enabled = on;
                let c = self.conf.clone();
                return cosmic::task::future(async move {
                    Message::Applied(if on { write_conf_and_apply(c).await } else { do_reset().await })
                });
            }
            Message::SdrNits(v)      => { self.conf.sdr_nits      = v; }
            Message::PeakNits(v)     => { self.conf.peak_nits     = v; }
            Message::Gamut(v)        => { self.conf.gamut         = v; }
            Message::GamutMode(i)    => {
                self.conf.gamut_mode = ["bt2020", "dci-p3", "srgb"][i.min(2)].into();
            }
            Message::Saturation(v)   => { self.conf.saturation   = v; }
            Message::MidtoneGamma(v) => { self.conf.midtone_gamma = v; }
            Message::BitDepth(i)     => { self.conf.max_bpc = [8u32, 10, 12][i.min(2)]; }
            Message::Apply => {
                self.status = Some(if daemon_alive() {
                    "Signalling daemon…".into()
                } else {
                    "Applying…".into()
                });
                let c = self.conf.clone();
                return cosmic::task::future(async move { Message::Applied(write_conf_and_apply(c).await) });
            }
            Message::Reset => {
                self.hdr_enabled = false;
                self.status = Some("Resetting…".into());
                return cosmic::task::future(async move { Message::Applied(do_reset().await) });
            }
            Message::Applied(Ok(())) => {
                self.status = Some(if daemon_alive() {
                    "Conf saved — daemon re-applying ✓".into()
                } else {
                    "Applied ✓".into()
                });
            }
            Message::Applied(Err(e)) => { self.status = Some(format!("Error: {e}")); }
            Message::ShowCalPat(pat) => {
                if let Some(mut c) = self.cal_child.take() { let _ = c.kill(); }
                match std::process::Command::new("python3").args([HDR_CAL, pat.arg()]).spawn() {
                    Ok(child) => { self.cal_child = Some(child); }
                    Err(e)    => { self.status = Some(format!("hdr-cal: {e}")); }
                }
            }
            Message::CalibrateHdr => {
                if let Some(mut c) = self.cal_child.take() { let _ = c.kill(); }
                let c = self.conf.clone();
                match std::process::Command::new("python3")
                    .args([
                        HDR_CAL, "--calibrate",
                        "--sdr-nits",   &c.sdr_nits.to_string(),
                        "--peak-nits",  &c.peak_nits.to_string(),
                        "--gamut",      &c.gamut.to_string(),
                        "--bpc",        &c.max_bpc.to_string(),
                        "--gamut-mode", &c.gamut_mode,
                    ])
                    .spawn()
                {
                    Ok(child) => { self.cal_child = Some(child); }
                    Err(e)    => { self.status = Some(format!("hdr-cal: {e}")); }
                }
            }
            Message::CloseCalPat => {
                if let Some(mut c) = self.cal_child.take() { let _ = c.kill(); }
            }
            Message::OledPreset(on) => {
                self.conf.oled_preset = on;
                if on { self.conf.sdr_nits = 150; self.conf.peak_nits = 600; }
                else  { self.conf.sdr_nits = 203; self.conf.peak_nits = 800; }
            }
            Message::OledDimTimeout(v) => { self.conf.oled_dim_min = v; }
            Message::NvSmoothMotion(v) => { self.nvidia_conf.smooth_motion = v; }
            Message::NvReflex(v)       => { self.nvidia_conf.reflex         = v; }
            Message::NvVibrance(v)     => { self.nvidia_conf.vibrance       = v; }
            Message::NvUpscale(i)      => {
                self.nvidia_conf.upscale = ["none", "fsr", "nis", "dlss", "integer"][i.min(4)].into();
            }
            Message::NvDldsr(v)           => { self.nvidia_conf.dldsr          = v; }
            Message::NvGsResolution(w, h) => { self.nvidia_conf.gs_width = w; self.nvidia_conf.gs_height = h; }
            Message::NvGsFps(v)           => { self.nvidia_conf.gs_fps         = v; }
            Message::NvApply => {
                self.status = Some("Saving NVIDIA settings…".into());
                let nc = self.nvidia_conf.clone();
                return cosmic::task::future(async move { Message::NvApplied(write_nvidia_conf(nc).await) });
            }
            Message::NvApplied(Ok(())) => { self.status = Some("NVIDIA settings saved ✓".into()); }
            Message::NvApplied(Err(e)) => { self.status = Some(format!("NVIDIA error: {e}")); }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let sp = cosmic::theme::active().cosmic().spacing;
        let mut page = column::with_capacity(16)
            .spacing(sp.space_m)
            .padding([sp.space_s, sp.space_l]);

        // ── Display capabilities ──────────────────────────────────────────────
        if let Some(ref d) = self.display {
            let cap = |label: &'static str, ok: bool| {
                text::caption(if ok { format!("{label} ✓") } else { format!("{label} —") })
            };
            let hdr_row = row::with_capacity(4).spacing(sp.space_xs)
                .push(cap("HDR10",        d.hdr10))
                .push(cap("HLG",          d.hlg))
                .push(cap("HDR10+",       d.hdr10plus))
                .push(cap("Dolby Vision", d.dolby));
            let feat_row = row::with_capacity(4).spacing(sp.space_xs)
                .push(cap("BT.2020",  d.bt2020))
                .push(cap("DCI-P3",   d.dci_p3))
                .push(cap("DSC",      d.dsc))
                .push(cap("HDMI-CEC", d.cec));
            let caps_col = column::with_capacity(2).spacing(sp.space_xxs)
                .push(hdr_row).push(feat_row);

            let iface = d.hdmi_ver.as_deref().or(d.dp_ver.as_deref()).unwrap_or("?");
            let desc = if d.max_lum_nits > 0 {
                format!("{iface} · EDID peak {} nits  ({:.1}% PQ)", d.max_lum_nits,
                        nits_to_pq_percent(d.max_lum_nits))
            } else {
                format!("{iface} · peak luminance not specified in EDID")
            };

            page = page
                .push(text::heading("Display"))
                .push(list_column().add(
                    settings::item::builder(d.name.as_str())
                        .description(desc)
                        .control(caps_col),
                ));
        }

        // ── GPU vendor badge ──────────────────────────────────────────────────
        let gpu_label = match self.gpu_vendor {
            "amd"    => "AMD  ·  Full pipeline: DEGAMMA + CTM + GAMMA + saturation + midtone",
            "intel"  => "Intel  ·  Full pipeline: DEGAMMA + CTM + GAMMA + saturation + midtone",
            "nvidia" => "NVIDIA  ·  Gamma-only on desktop (PQ + midtone); full HDR + gaming via hdr-game",
            _        => "GPU vendor unknown",
        };
        let daemon_badge = if daemon_alive() { "  ·  daemon ✓ (live reload active)" } else { "" };
        page = page.push(
            text::caption(format!("{gpu_label}{daemon_badge}"))
                .apply(widget::container)
                .padding([0, 0, sp.space_xs, 0])
        );

        // ── HDR toggle ────────────────────────────────────────────────────────
        page = page
            .push(text::heading("HDR Output"))
            .push(list_column().add(
                settings::item::builder("Enable HDR10")
                    .description("BT.2020 + PQ (ST2084) · kms-hdr.service")
                    .control(toggler(self.hdr_enabled).on_toggle(Message::HdrToggle)),
            ));

        // ── Brightness ────────────────────────────────────────────────────────
        let sdr_pq = nits_to_pq_percent(self.conf.sdr_nits);
        let sdr_row = settings::item::builder("SDR White")
            .description("Brightness of desktop/SDR content in HDR mode")
            .control(
                row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                    .push(widget::slider(80..=400, self.conf.sdr_nits, Message::SdrNits)
                        .width(Length::Fill))
                    .push(text::body(format!("{} nits  ({:.1}% PQ)", self.conf.sdr_nits, sdr_pq))
                        .apply(widget::container).width(Length::Fixed(140.0))),
            );

        let peak_pq = nits_to_pq_percent(self.conf.peak_nits);
        let peak_row = settings::item::builder("Display Peak")
            .description("Your display's maximum HDR luminance")
            .control(
                row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                    .push(widget::slider(400..=1200, self.conf.peak_nits, Message::PeakNits)
                        .step(10u32).width(Length::Fill))
                    .push(text::body(format!("{} nits  ({:.1}% PQ)", self.conf.peak_nits, peak_pq))
                        .apply(widget::container).width(Length::Fixed(140.0))),
            );

        page = page
            .push(text::heading("Brightness"))
            .push(list_column().add(sdr_row).add(peak_row));

        // ── Colour ────────────────────────────────────────────────────────────
        let gamut_opts = vec![
            "BT.2020  (full wide colour — UHDTV / DCI cinemas)".to_string(),
            "DCI-P3 D65  (Apple / cinema mid-gamut)".to_string(),
            "sRGB  (no gamut expansion)".to_string(),
        ];
        let gamut_sel = match self.conf.gamut_mode.as_str() {
            "dci-p3" => Some(1usize),
            "srgb"   => Some(2usize),
            _        => Some(0usize),
        };

        page = page
            .push(text::heading(if self.is_nvidia { "Colour  (AMD/Intel only)" } else { "Colour" }))
            .push(list_column()
                .add(settings::item::builder("Target Gamut")
                    .description("Colour space the CTM matrix expands sRGB into")
                    .control(widget::dropdown(gamut_opts, gamut_sel, Message::GamutMode)
                        .width(Length::Fixed(290.0))))
                .add(settings::item::builder("Expansion")
                    .description("0% = sRGB identical · 100% = full target gamut")
                    .control(
                        row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                            .push(widget::slider(0..=100, self.conf.gamut, Message::Gamut)
                                .width(Length::Fill))
                            .push(text::body(format!("{}%", self.conf.gamut))
                                .apply(widget::container).width(Length::Fixed(48.0))),
                    )),
            );

        // ── Color Intensity ───────────────────────────────────────────────────
        page = page
            .push(text::heading(if self.is_nvidia { "Color Intensity  (AMD/Intel only)" } else { "Color Intensity" }))
            .push(list_column().add(
                settings::item::builder("Saturation")
                    .description("Color vividness via BT.709 saturation matrix · 100% = neutral · 150% = vivid")
                    .control(
                        row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                            .push(widget::slider(50..=200u32, self.conf.saturation, Message::Saturation)
                                .step(5u32).width(Length::Fill))
                            .push(text::body(format!("{}%", self.conf.saturation))
                                .apply(widget::container).width(Length::Fixed(52.0))),
                    ),
            ));

        // ── Tone Mapping ──────────────────────────────────────────────────────
        let mg_desc = match self.conf.midtone_gamma {
            v if v > 110 => format!("{}%  — HDR punch (darkened midtones, higher contrast)", v),
            v if v < 90  => format!("{}%  — lifted midtones (lower contrast, may look washed)", v),
            v            => format!("{}%  — neutral", v),
        };
        page = page
            .push(text::heading("Tone Mapping"))
            .push(list_column().add(
                settings::item::builder("Midtone Gamma")
                    .description(mg_desc)
                    .control(
                        row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                            .push(widget::slider(30..=250u32, self.conf.midtone_gamma, Message::MidtoneGamma)
                                .step(5u32).width(Length::Fill))
                            .push(text::body(format!("{}%", self.conf.midtone_gamma))
                                .apply(widget::container).width(Length::Fixed(52.0))),
                    ),
            ));

        // ── Output Format ─────────────────────────────────────────────────────
        let bpc_opts = vec![
            "8 bpc  (legacy displays)".to_string(),
            "10 bpc  (recommended — HDR10)".to_string(),
            "12 bpc  (reference monitors / HDR+)".to_string(),
        ];
        let bpc_sel = match self.conf.max_bpc { 8 => Some(0), 12 => Some(2), _ => Some(1) };

        page = page
            .push(text::heading("Output Format"))
            .push(list_column().add(
                settings::item::builder("Bit Depth")
                    .description("Requested via max_requested_bpc on the connector")
                    .control(widget::dropdown(bpc_opts, bpc_sel, Message::BitDepth)
                        .width(Length::Fixed(290.0))),
            ));

        // ── NVIDIA Gaming ─────────────────────────────────────────────────────
        if self.is_nvidia {
            let upscale_opts = vec![
                "None  (native res)".to_string(),
                "FSR  (AMD FidelityFX Super Resolution)".to_string(),
                "NIS  (NVIDIA Image Scaling)".to_string(),
                "DLSS  (Deep Learning Super Sampling)".to_string(),
                "Integer  (pixel-perfect integer scale)".to_string(),
            ];
            let upscale_sel = Some(match self.nvidia_conf.upscale.as_str() {
                "fsr" => 1usize, "nis" => 2, "dlss" => 3, "integer" => 4, _ => 0,
            });
            let vibrance_pct = ((self.nvidia_conf.vibrance + 1024) as f32 / 2047.0 * 100.0) as u32;

            page = page
                .push(text::heading("NVIDIA Gaming"))
                .push(list_column()
                    .add(settings::item::builder("RTX Smooth Motion")
                        .description("Frame generation via VK_LAYER_NV_present — Vulkan + DXVK/Proton")
                        .control(toggler(self.nvidia_conf.smooth_motion).on_toggle(Message::NvSmoothMotion)))
                    .add(settings::item::builder("NVIDIA Reflex")
                        .description("Low-latency via NvAPI (PROTON_ENABLE_NVAPI + DXVK_ENABLE_NVAPI)")
                        .control(toggler(self.nvidia_conf.reflex).on_toggle(Message::NvReflex)))
                    .add(settings::item::builder("DLDSR")
                        .description("Deep Learning Dynamic Super Resolution — renders higher, displays native")
                        .control(toggler(self.nvidia_conf.dldsr).on_toggle(Message::NvDldsr)))
                    .add(settings::item::builder("Digital Vibrance")
                        .description("Colour saturation via nvibrant ioctl · 0% = neutral · 100% = max")
                        .control(
                            row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                                .push(widget::slider(-1024i32..=1023i32, self.nvidia_conf.vibrance,
                                    Message::NvVibrance).step(32i32).width(Length::Fill))
                                .push(text::body(format!("{}%", vibrance_pct))
                                    .apply(widget::container).width(Length::Fixed(48.0))),
                        ))
                    .add(settings::item::builder("Upscaling")
                        .description("Algorithm used inside Gamescope for resolution scaling")
                        .control(widget::dropdown(upscale_opts, upscale_sel, Message::NvUpscale)
                            .width(Length::Fixed(290.0))))
                );

            const GS_RESOLUTIONS: &[(u32, u32, &str)] = &[
                (1920, 1080, "1920 × 1080  (1080p)"),
                (2560, 1440, "2560 × 1440  (1440p)"),
                (3840, 2160, "3840 × 2160  (4K UHD)"),
                (5120, 2880, "5120 × 2880  (5K)"),
                (7680, 4320, "7680 × 4320  (8K)"),
            ];
            let res_opts: Vec<String> = GS_RESOLUTIONS.iter().map(|(_, _, s)| s.to_string()).collect();
            let res_sel = GS_RESOLUTIONS.iter().position(|(w, h, _)| {
                *w == self.nvidia_conf.gs_width && *h == self.nvidia_conf.gs_height
            });

            page = page
                .push(text::heading("Gamescope (hdr-game)"))
                .push(list_column()
                    .add(settings::item::builder("Output Resolution")
                        .description("Gamescope target resolution — should match your display native res")
                        .control(widget::dropdown(res_opts, res_sel, |i| {
                            let (w, h, _) = GS_RESOLUTIONS[i];
                            Message::NvGsResolution(w, h)
                        }).width(Length::Fixed(290.0))))
                    .add(settings::item::builder("Target FPS")
                        .description("Gamescope framerate cap")
                        .control(
                            row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                                .push(widget::slider(30..=360u32, self.nvidia_conf.gs_fps,
                                    Message::NvGsFps).step(30u32).width(Length::Fill))
                                .push(text::body(format!("{} fps", self.nvidia_conf.gs_fps))
                                    .apply(widget::container).width(Length::Fixed(68.0))),
                        ))
                    .add(settings::item::builder("Apply NVIDIA Settings")
                        .description("Saves to /etc/hdr-game.conf — picked up by hdr-game on next launch")
                        .control(widget::button::suggested("Save").on_press(Message::NvApply)))
                );
        }

        // ── OLED Care ─────────────────────────────────────────────────────────
        let display_is_oled = self.display.as_ref().map(|d| d.is_oled).unwrap_or(false);
        if display_is_oled {
            let dim_label = if self.conf.oled_dim_min == 0 {
                "Off".to_string()
            } else {
                format!("{} min", self.conf.oled_dim_min)
            };
            page = page
                .push(text::heading("OLED Care"))
                .push(list_column()
                    .add(settings::item::builder("Longevity Preset")
                        .description("SDR 150 nits · HDR peak 600 nits — reduces panel stress for daily desktop use")
                        .control(toggler(self.conf.oled_preset).on_toggle(Message::OledPreset)))
                    .add(settings::item::builder("Auto-Dim")
                        .description("Dim to 50 nits after idle timeout via swayidle · requires swayidle installed")
                        .control(
                            row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                                .push(widget::slider(0..=60u32, self.conf.oled_dim_min,
                                    Message::OledDimTimeout).step(5u32).width(Length::Fill))
                                .push(text::body(dim_label)
                                    .apply(widget::container).width(Length::Fixed(52.0))),
                        ))
                    .add(settings::item::builder("Pixel Shift")
                        .description("Handled by COSMIC/KDE compositor — enable in Display → Screen Saver settings")
                        .control(text::caption("compositor setting")))
                );
        }

        // ── HDR Calibration ───────────────────────────────────────────────────
        const PATTERNS: &[CalibPattern] = &[
            CalibPattern::Black, CalibPattern::DarkGray, CalibPattern::Gray50,
            CalibPattern::White, CalibPattern::Red,      CalibPattern::Green,
            CalibPattern::Blue,  CalibPattern::SdrHdrSplit,
        ];
        let mut pat_row = row::with_capacity(10).spacing(sp.space_xxs).align_y(Alignment::Center);
        for &p in PATTERNS {
            pat_row = pat_row.push(widget::button::standard(p.label()).on_press(Message::ShowCalPat(p)));
        }
        if self.cal_child.is_some() {
            pat_row = pat_row.push(widget::button::destructive("✕ Close").on_press(Message::CloseCalPat));
        }

        page = page
            .push(text::heading("HDR Calibration"))
            .push(list_column()
                .add(settings::item::builder("Calibrate HDR")
                    .description("Adjust SDR content brightness interactively — like Windows HDR Calibration")
                    .control(widget::button::suggested("Calibrate…").on_press(Message::CalibrateHdr)))
                .add(settings::item::builder("Test Patterns")
                    .description("Full-screen colour fields — click or press Esc to close")
                    .control(pat_row)),
            );

        // ── Status + action buttons ───────────────────────────────────────────
        let mut btn_row = row::with_capacity(3)
            .spacing(sp.space_s).align_y(Alignment::Center)
            .padding([0, 0, sp.space_s, 0]);

        if let Some(ref s) = self.status {
            btn_row = btn_row.push(text::caption(s.as_str()).apply(widget::container).width(Length::Fill));
        } else {
            btn_row = btn_row.push(widget::Space::new().width(Length::Fill));
        }
        btn_row = btn_row
            .push(widget::button::destructive("Reset to SDR").on_press(Message::Reset))
            .push(widget::button::suggested("Apply").on_press(Message::Apply));

        page = page.push(btn_row);

        widget::scrollable(page).width(Length::Fill).height(Length::Fill).into()
    }
}

fn main() -> cosmic::iced::Result {
    let settings = cosmic::app::Settings::default()
        .size(cosmic::iced::Size::new(680.0, 960.0))
        .resizable(Some(8.0));
    cosmic::app::run::<CosmicHdr>(settings, ())
}
