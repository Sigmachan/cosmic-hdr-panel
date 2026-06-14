// cosmic-hdr — HDR display settings panel for COSMIC Desktop

use cosmic::app::{Core, Task};
use cosmic::iced::{Alignment, Length};
use cosmic::widget::{self, column, list_column, row, settings, text, toggler};
use cosmic::{Application, ApplicationExt, Apply, Element};
use tokio::process::Command;

const APP_ID: &str = "ru.sigmachan.CosmicHdr";
const BIN: &str = "/usr/local/bin/cosmic-hdr";
const HDR_CAL: &str = "/usr/local/lib/cosmic-hdr/hdr-cal.py";

// ── Connector / EDID detection ─────────────────────────────────────────────────

/// Returns (edid_path, sysfs_dir) for the first active connector with a valid EDID.
fn find_active_connector() -> Option<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if !s.starts_with("card") || !s.contains('-') { return None; }
            let edid = format!("/sys/class/drm/{}/edid", s);
            let ok = std::fs::read(&edid).map(|d| d.len() >= 128).unwrap_or(false);
            if ok { Some((edid, s.to_string())) } else { None }
        })
        .collect();
    found.sort();
    found.into_iter().next()
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
}

impl Default for HdrConf {
    fn default() -> Self {
        Self { sdr_nits: 203, peak_nits: 800, gamut: 100, max_bpc: 10, gamut_mode: "bt2020".into(), saturation: 100 }
    }
}

fn read_conf() -> HdrConf {
    let mut c = HdrConf::default();
    if let Ok(s) = std::fs::read_to_string("/etc/cosmic-hdr.conf") {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "SDR_NITS"   => { if let Ok(n) = v.trim().parse() { c.sdr_nits   = n; } }
                    "PEAK_NITS"  => { if let Ok(n) = v.trim().parse() { c.peak_nits  = n; } }
                    "GAMUT"      => { if let Ok(n) = v.trim().parse() { c.gamut      = n; } }
                    "MAX_BPC"    => { if let Ok(n) = v.trim().parse() { c.max_bpc    = n; } }
                    "GAMUT_MODE"  => { c.gamut_mode = v.trim().to_owned(); }
                    "SATURATION"  => { if let Ok(n) = v.trim().parse() { c.saturation = n; } }
                    _ => {}
                }
            }
        }
    }
    c
}

async fn write_conf_and_apply(c: HdrConf) -> Result<(), String> {
    let s = Command::new("pkexec")
        .args([BIN, "--save",
               "--sdr-nits",   &c.sdr_nits.to_string(),
               "--peak-nits",  &c.peak_nits.to_string(),
               "--gamut",      &c.gamut.to_string(),
               "--bpc",        &c.max_bpc.to_string(),
               "--gamut-mode", &c.gamut_mode,
               "--saturation", &c.saturation.to_string()])
        .status().await.map_err(|e| e.to_string())?;
    if s.success() { Ok(()) } else { Err(format!("cosmic-hdr exited {s}")) }
}

async fn do_reset() -> Result<(), String> {
    let s = Command::new("pkexec").args([BIN, "reset"])
        .status().await.map_err(|e| e.to_string())?;
    if s.success() { Ok(()) } else { Err(format!("cosmic-hdr reset exited {s}")) }
}

fn service_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "cosmic-hdr.service"])
        .output()
        .map(|o| o.stdout.starts_with(b"active"))
        .unwrap_or(false)
}

// ── Display info ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct DisplayInfo {
    name: String,
    connector_dir: String,    // sysfs entry, e.g. "card1-HDMI-A-2"
    hdr10: bool,
    hlg: bool,
    hdr10plus: bool,
    dolby: bool,
    bt2020: bool,
    dci_p3: bool,             // CTA-861 Colorimetry DCI-P3 bit
    dsc: bool,                // Display Stream Compression (EDID + sysfs)
    cec: bool,                // /dev/cec0 present
    max_lum_nits: u32,
    hdmi_ver: Option<String>, // "HDMI 1.4" / "HDMI 2.0" / "HDMI 2.1"
    dp_ver: Option<String>,   // "DP 1.4" etc. (DP connectors only)
}

fn parse_edid() -> Option<DisplayInfo> {
    let (edid_path, connector_dir) = find_active_connector()?;
    let raw = std::fs::read(&edid_path).ok()?;
    let mut info = DisplayInfo { connector_dir: connector_dir.clone(), ..Default::default() };

    // Monitor name from EDID descriptor tag 0xFC
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

    // CEA-861 extension blocks
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
                // Extended Data Block — data[0] is extended_tag
                7 if !data.is_empty() => {
                    let payload = &data[1..];
                    match data[0] {
                        // HDR Static Metadata (ext_tag=6)
                        6 if !payload.is_empty() => {
                            info.hdr10 = payload[0] & 0x04 != 0;
                            info.hlg   = payload[0] & 0x08 != 0;
                            if payload.len() > 2 && payload[2] != 0 {
                                info.max_lum_nits =
                                    (50.0 * 2f64.powf(payload[2] as f64 / 32.0)) as u32;
                            }
                        }
                        // Colorimetry (ext_tag=5)
                        // Byte 1 bits: 7=BT2020RGB 6=BT2020YCC 5=BT2020cYCC 4=opRGB
                        //              3=opYCC601 2=sYCC601 1=DCI-P3 0=xvYCC709
                        5 if !payload.is_empty() => {
                            info.bt2020 = payload[0] & 0x80 != 0;
                            info.dci_p3 = payload[0] & 0x02 != 0;
                        }
                        // HDR10+ (ext_tag=13)
                        13 => { info.hdr10plus = true; }
                        // VSVDB (ext_tag=1): Dolby Vision — IEEE OUI 0x00D046
                        1 if payload.len() >= 3 => {
                            let oui = u32::from_le_bytes([payload[0], payload[1], payload[2], 0]);
                            if oui == 0x0000_D046 { info.dolby = true; }
                        }
                        _ => {}
                    }
                }
                // Vendor-Specific Data Block (tag=3): OUI at data[0..3]
                3 if data.len() >= 3 => {
                    let oui = u32::from_le_bytes([data[0], data[1], data[2], 0]);
                    match oui {
                        // Dolby Vision VSDB fallback
                        0x0000_D046 => { info.dolby = true; }
                        // HDMI Licensing VSDB [03 0C 00] = HDMI 1.x
                        0x0000_0C03 => {
                            if info.hdmi_ver.is_none() {
                                info.hdmi_ver = Some("HDMI 1.4".into());
                            }
                        }
                        // HDMI Forum VSDB [00 5D C4] = HDMI 2.0+
                        // data[4] = Max TMDS Character Rate × 5 MHz; ≥600 MHz → HDMI 2.1
                        0x00C4_5D00 => {
                            let max_tmds_mhz = if data.len() >= 5 { data[4] as u32 * 5 } else { 0 };
                            info.hdmi_ver = Some(if max_tmds_mhz >= 600 {
                                "HDMI 2.1".into()
                            } else {
                                "HDMI 2.0".into()
                            });
                            // DSC 1.2 capable bit (byte 9 / data[8]) in HF-VSDB version ≥3
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

    // DSC also visible via sysfs (NVIDIA driver uses this path)
    if std::path::Path::new(&format!("/sys/class/drm/{}/dsc_enable", connector_dir)).exists() {
        info.dsc = true;
    }

    // DP version from DPCD byte 0
    if connector_dir.contains("-DP-") || connector_dir.contains("-eDP-") {
        if let Ok(dpcd) = std::fs::read(format!("/sys/class/drm/{}/dpcd", connector_dir)) {
            if !dpcd.is_empty() {
                info.dp_ver = Some(match dpcd[0] {
                    0x10 => "DP 1.0".into(),
                    0x11 => "DP 1.1".into(),
                    0x12 => "DP 1.2".into(),
                    0x13 => "DP 1.3".into(),
                    0x14 => "DP 1.4".into(),
                    v if v >= 0x20 => "DP 2.x (UHBR)".into(),
                    v => format!("DP (DPCD {v:#04x})"),
                });
            }
        }
    }

    // HDMI-CEC: kernel CEC framework exposes /dev/cec0 when the GPU driver supports it
    info.cec = std::path::Path::new("/dev/cec0").exists();

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
    BitDepth(usize),
    Apply,
    Reset,
    Applied(Result<(), String>),
    ShowCalPat(CalibPattern),
    CalibrateHdr,
    CloseCalPat,
}

struct CosmicHdr {
    core: Core,
    conf: HdrConf,
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
        let mut app = Self {
            core,
            conf: read_conf(),
            hdr_enabled: service_active(),
            display: parse_edid(),
            status: None,
            cal_child: None,
        };
        app.set_header_title("HDR Display Settings".into());
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
            Message::SdrNits(v)   => { self.conf.sdr_nits  = v; }
            Message::PeakNits(v)  => { self.conf.peak_nits = v; }
            Message::Gamut(v)     => { self.conf.gamut      = v; }
            Message::GamutMode(i) => {
                self.conf.gamut_mode = ["bt2020", "dci-p3", "srgb"][i.min(2)].into();
            }
            Message::Saturation(v) => { self.conf.saturation = v; }
            Message::BitDepth(i)   => { self.conf.max_bpc = [8u32, 10, 12][i.min(2)]; }
            Message::Apply => {
                self.status = Some("Applying…".into());
                let c = self.conf.clone();
                return cosmic::task::future(async move { Message::Applied(write_conf_and_apply(c).await) });
            }
            Message::Reset => {
                self.hdr_enabled = false;
                self.status = Some("Resetting…".into());
                return cosmic::task::future(async move { Message::Applied(do_reset().await) });
            }
            Message::Applied(Ok(())) => { self.status = Some("Applied ✓".into()); }
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
        }
        Task::none()
    }

    fn view(&self) -> Element<Message> {
        let sp = cosmic::theme::active().cosmic().spacing;
        let mut page = column::with_capacity(14)
            .spacing(sp.space_m)
            .padding([sp.space_s, sp.space_l]);

        // ── Display capabilities ──────────────────────────────────────────────
        if let Some(ref d) = self.display {
            // Small capability badge
            let cap = |label: &'static str, ok: bool| {
                text::caption(if ok { format!("{label} ✓") } else { format!("{label} —") })
            };

            // Row 1: HDR format support
            let hdr_row = row::with_capacity(4)
                .spacing(sp.space_xs)
                .push(cap("HDR10",        d.hdr10))
                .push(cap("HLG",          d.hlg))
                .push(cap("HDR10+",       d.hdr10plus))
                .push(cap("Dolby Vision", d.dolby));

            // Row 2: Colour space + connection features
            let feat_row = row::with_capacity(4)
                .spacing(sp.space_xs)
                .push(cap("BT.2020",  d.bt2020))
                .push(cap("DCI-P3",   d.dci_p3))
                .push(cap("DSC",      d.dsc))
                .push(cap("HDMI-CEC", d.cec));

            let caps_col = column::with_capacity(2)
                .spacing(sp.space_xxs)
                .push(hdr_row)
                .push(feat_row);

            // Description: interface version + EDID peak
            let iface = d.hdmi_ver.as_deref().or(d.dp_ver.as_deref()).unwrap_or("?");
            let desc = if d.max_lum_nits > 0 {
                format!("{iface} · EDID peak {} nits", d.max_lum_nits)
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

        // ── HDR toggle ────────────────────────────────────────────────────────
        page = page
            .push(text::heading("HDR Output"))
            .push(list_column().add(
                settings::item::builder("Enable HDR10")
                    .description("BT.2020 + PQ (ST2084) · cosmic-hdr.service")
                    .control(toggler(self.hdr_enabled).on_toggle(Message::HdrToggle)),
            ));

        // ── Brightness ────────────────────────────────────────────────────────
        let sdr_row = settings::item::builder("SDR White")
            .description("Brightness of desktop/SDR content in HDR mode")
            .control(
                row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                    .push(widget::slider(80..=400, self.conf.sdr_nits, Message::SdrNits)
                        .width(Length::Fill))
                    .push(text::body(format!("{} nits", self.conf.sdr_nits))
                        .apply(widget::container).width(Length::Fixed(76.0))),
            );

        let peak_row = settings::item::builder("Display Peak")
            .description("Your display's maximum HDR luminance")
            .control(
                row::with_capacity(2).spacing(sp.space_s).align_y(Alignment::Center)
                    .push(widget::slider(400..=1200, self.conf.peak_nits, Message::PeakNits)
                        .step(10u32).width(Length::Fill))
                    .push(text::body(format!("{} nits", self.conf.peak_nits))
                        .apply(widget::container).width(Length::Fixed(76.0))),
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
            .push(text::heading("Colour"))
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

        // ── Color intensity ───────────────────────────────────────────────────
        page = page
            .push(text::heading("Color Intensity"))
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

        // ── Output format ─────────────────────────────────────────────────────
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

        // ── HDR Calibration ───────────────────────────────────────────────────
        const PATTERNS: &[CalibPattern] = &[
            CalibPattern::Black, CalibPattern::DarkGray, CalibPattern::Gray50,
            CalibPattern::White, CalibPattern::Red, CalibPattern::Green,
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
        .size(cosmic::iced::Size::new(680.0, 900.0))
        .resizable(Some(8.0));
    cosmic::app::run::<CosmicHdr>(settings, ())
}
