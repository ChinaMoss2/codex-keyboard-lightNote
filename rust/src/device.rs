// SPDX-License-Identifier: GPL-2.0-or-later
// 报文沿用已验证的 OpenRGB CHERRY 协议。
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::PathBuf,
};

const NUMPAD: [usize; 17] = [
    103, 109, 115, 121, 104, 110, 116, 105, 111, 117, 122, 106, 112, 118, 113, 119, 124,
];
fn checked(mut p: [u8; 64]) -> [u8; 64] {
    let sum = p[3..].iter().map(|v| *v as i8 as i32).sum::<i32>() as u16;
    p[1..3].copy_from_slice(&sum.to_le_bytes());
    p
}
pub fn reports(state: &str) -> Vec<[u8; 64]> {
    let blue = [0, 32, 127];
    let green = [0, 127, 16];
    let mut result = Vec::new();
    if matches!(state, "running_subagents" | "subagents_only") {
        let colors: Vec<u8> = (0..126)
            .flat_map(|i| if NUMPAD.contains(&i) { green } else { blue })
            .collect();
        let colors: Vec<u8> = if state == "subagents_only" {
            (0..126)
                .flat_map(|i| {
                    if NUMPAD.contains(&i) {
                        green
                    } else {
                        [0, 0, 0]
                    }
                })
                .collect()
        } else {
            colors
        };
        for (i, data) in colors.chunks(54).enumerate() {
            let offset = i * 54;
            let mut p = [0; 64];
            p[0] = 4;
            p[3] = 11;
            p[4] = data.len() as u8;
            p[5] = offset as u8;
            p[6] = (offset >> 8) as u8;
            p[8..8 + data.len()].copy_from_slice(data);
            result.push(checked(p));
        }
    }
    let data = match state {
        "idle" => [0, 21, 4, 4, 0, 1, 0, 0, 0],
        "running_subagents" => [0, 8, 4, 2, 0, 0, 0, 32, 127],
        "subagents_only" => [0, 8, 4, 2, 0, 0, 0, 0, 0],
        "waiting" => [0, 3, 4, 2, 0, 0, 127, 55, 0],
        "done" => [0, 3, 4, 2, 0, 0, 0, 127, 16],
        _ => [0, 3, 4, 2, 0, 0, 0, 32, 127],
    };
    let mut p = [0; 64];
    p[0] = 4;
    p[3..8].copy_from_slice(&[6, 9, 0, 0, 85]);
    p[8..17].copy_from_slice(&data);
    result.push(checked(p));
    result
}
// 分区需要独立颜色，复用已验证的 Custom 报文，不依赖未确认的固件分区动画。
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    pub state: String,
    pub main_spectrum: bool,
    pub numpad_spectrum: bool,
}
impl Effect {
    pub fn new(state: String, counts: (usize, usize)) -> Self {
        let running = matches!(
            state.as_str(),
            "running" | "running_subagents" | "subagents_only"
        );
        Self {
            main_spectrum: running && counts.0 >= 2,
            numpad_spectrum: (running || state == "input_waiting_subagents") && counts.1 >= 2,
            state,
        }
    }
    pub fn animated(&self) -> bool {
        self.main_spectrum || self.numpad_spectrum
    }
    pub fn frame(&self, seconds: f64) -> Vec<[u8; 64]> {
        let input_waiting = matches!(
            self.state.as_str(),
            "input_waiting" | "input_waiting_subagents"
        );
        if !self.animated() && !input_waiting {
            return reports(&self.state);
        }
        // Spectrum：4 秒走完整个色环，同一区域同步变色，保持最高亮度档。
        let rainbow = spectrum_color(seconds);
        let split = !matches!(self.state.as_str(), "running" | "input_waiting");
        let mut packets = reports(if self.state == "subagents_only" {
            "subagents_only"
        } else {
            "running_subagents"
        });
        for (chunk, packet) in packets[..7].iter_mut().enumerate() {
            for j in 0..18 {
                let numpad = NUMPAD.contains(&(chunk * 18 + j));
                let (color, spectrum) = if input_waiting && !numpad {
                    ([127, 55, 0], false)
                } else if split && numpad {
                    ([0, 127, 16], self.numpad_spectrum)
                } else if self.state == "subagents_only" {
                    ([0, 0, 0], false)
                } else {
                    ([0, 32, 127], self.main_spectrum)
                };
                for c in 0..3 {
                    packet[8 + j * 3 + c] = if spectrum { rainbow[c] } else { color[c] };
                }
            }
            *packet = checked(*packet);
        }
        packets
    }
}
fn spectrum_color(seconds: f64) -> [u8; 3] {
    let hue = seconds.rem_euclid(4.0) * 1.5;
    let sector = hue.floor() as usize;
    let rising = ((hue - sector as f64) * 127.0).round() as u8;
    let falling = 127 - rising;
    match sector {
        0 => [127, rising, 0],
        1 => [falling, 127, 0],
        2 => [0, 127, rising],
        3 => [0, falling, 127],
        4 => [rising, 0, 127],
        _ => [127, 0, falling],
    }
}
fn discover() -> io::Result<PathBuf> {
    let mut matches = Vec::new();
    for e in fs::read_dir("/sys/class/hidraw")? {
        let e = e?;
        let p = e.path().join("device");
        let meta = fs::read_to_string(p.join("uevent")).unwrap_or_default();
        if meta.contains("HID_ID=0003:0000046A:00000079\n")
            && fs::read(p.join("report_descriptor"))
                .unwrap_or_default()
                .windows(3)
                .any(|v| v == [6, 28, 255])
        {
            matches.push(PathBuf::from("/dev").join(e.file_name()));
        }
    }
    if matches.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "需要恰好一个 CHERRY 灯光接口",
        ));
    }
    Ok(matches.remove(0))
}
pub struct Device {
    file: Option<File>,
    pub path: Option<PathBuf>,
    dry: bool,
    previous: Vec<[u8; 64]>,
}
impl Device {
    pub fn new(dry: bool) -> Self {
        Self {
            file: None,
            path: None,
            dry,
            previous: Vec::new(),
        }
    }
    pub fn invalidate(&mut self) {
        self.previous.clear();
        self.file = None;
        self.path = None;
    }
    pub fn set_frame(&mut self, packets: Vec<[u8; 64]>) -> io::Result<usize> {
        if self.dry {
            self.previous = packets;
            return Ok(0);
        }
        if self.file.is_none() {
            let path = discover()?;
            self.file = Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)?,
            );
            self.path = Some(path);
        }
        let file = self.file.as_mut().unwrap();
        let mut written = 0;
        for (index, packet) in packets.iter().enumerate() {
            if self.previous.len() == packets.len() && self.previous.get(index) == Some(packet) {
                continue;
            }
            let n = file.write(packet)?;
            if n != 64 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "HID 短写"));
            }
            let mut poll = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(700);
            loop {
                let timeout = deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis() as i32;
                let rc = unsafe { libc::poll(&mut poll, 1, timeout) };
                if rc <= 0 {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "HID 匹配响应超时"));
                }
                let mut reply = [0; 64];
                let n = file.read(&mut reply)?;
                if n == 64 && reply == *packet {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "HID 未收到匹配响应",
                    ));
                }
            }
            written += 64;
        }
        self.previous = packets;
        Ok(written)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn rgb(p: &[[u8; 64]], key: usize) -> [u8; 3] {
        let offset = key * 3;
        p[offset / 54][8 + offset % 54..11 + offset % 54]
            .try_into()
            .unwrap()
    }
    #[test]
    fn spectrum_cycle_keeps_peak_and_never_blacks_out() {
        let colors = [
            [127, 0, 0],
            [127, 127, 0],
            [0, 127, 0],
            [0, 127, 127],
            [0, 0, 127],
            [127, 0, 127],
        ];
        for (i, color) in colors.iter().enumerate() {
            assert_eq!(spectrum_color(i as f64 * 4.0 / 6.0), *color);
        }
        for step in 0..40 {
            let t = step as f64 / 10.0;
            let color = spectrum_color(t);
            assert_eq!(*color.iter().max().unwrap(), 127);
            assert_eq!(color, spectrum_color(t + 4.0));
        }
    }
    #[test]
    fn question_orange_main_preserves_numpad() {
        for (state, counts, expected) in [
            ("input_waiting", (2, 0), [0, 32, 127]),
            ("input_waiting_subagents", (2, 1), [0, 127, 16]),
            ("input_waiting_subagents", (2, 2), [0, 127, 127]),
        ] {
            let e = Effect::new(state.into(), counts);
            assert!(!e.main_spectrum);
            assert_eq!(rgb(&e.frame(2.0), 0), [127, 55, 0]);
            assert_eq!(rgb(&e.frame(2.0), 103), expected);
        }
    }
    #[test]
    fn independent_spectrum_and_static_recovery() {
        for (state, counts, main, pad) in [
            ("running_subagents", (2, 1), [0, 127, 127], [0, 127, 16]),
            ("running_subagents", (1, 2), [0, 32, 127], [0, 127, 127]),
            ("running_subagents", (2, 2), [0, 127, 127], [0, 127, 127]),
            ("running", (2, 0), [0, 127, 127], [0, 127, 127]),
            ("subagents_only", (0, 2), [0, 0, 0], [0, 127, 127]),
        ] {
            let e = Effect::new(state.into(), counts);
            let frame = e.frame(2.0);
            for key in 0..126 {
                assert_eq!(
                    rgb(&frame, key),
                    if NUMPAD.contains(&key) { pad } else { main }
                );
            }
            assert_ne!(frame, e.frame(0.0));
            for packet in frame {
                assert_eq!(packet, checked(packet));
            }
        }
        for name in ["waiting", "done", "idle"] {
            let e = Effect::new(name.into(), (2, 2));
            assert!(!e.animated());
            assert_eq!(e.frame(2.0), reports(name));
        }
        let e = Effect::new("running_subagents".into(), (1, 1));
        assert!(!e.animated());
        assert_eq!(e.frame(2.0), reports("running_subagents"));
    }
    #[test]
    fn numpad_and_idle() {
        let p = reports("running_subagents");
        assert_eq!(p.len(), 8);
        let colors: Vec<u8> = p[..7].iter().flat_map(|p| p[8..62].to_vec()).collect();
        for i in 0..126 {
            assert_eq!(
                &colors[i * 3..i * 3 + 3],
                if NUMPAD.contains(&i) {
                    &[0, 127, 16]
                } else {
                    &[0, 32, 127]
                }
            );
        }
        assert_eq!(&reports("idle")[0][9..14], &[21, 4, 4, 0, 1]);
    }

    #[test]
    fn subagents_only_has_black_main_zone_and_green_numpad() {
        let packets = reports("subagents_only");
        let colors: Vec<u8> = packets[..7]
            .iter()
            .flat_map(|p| p[8..62].to_vec())
            .collect();
        for i in 0..126 {
            assert_eq!(
                &colors[i * 3..i * 3 + 3],
                if NUMPAD.contains(&i) {
                    &[0, 127, 16]
                } else {
                    &[0, 0, 0]
                }
            );
        }
    }
}
