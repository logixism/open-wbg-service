use std::{
    collections::{BTreeSet, HashSet},
    ffi::CString,
    fs::{self, File},
    io::{self, Read},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

const SYSTEM: &str = "system_info_source";
const VOLUME: &str = "system_volume";
const BATTERY: &str = "system_battery_source";
const MAX_PROC_BYTES: u64 = 1024 * 1024;
const MAX_MOUNTS: usize = 128;
const MAX_SENSORS: usize = 128;
const PACTL_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, Serialize)]
pub struct DataPoint {
    pub path: String,
    pub name: String,
    pub source: String,
    pub value: DataValue,
    pub min: f32,
    pub max: f32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DataValue {
    Float(f32),
    Bool(bool),
    Integer(i64),
}

#[derive(Default)]
pub struct Sampler {
    last_cpu: Option<(u64, u64)>,
    failures: BTreeSet<&'static str>,
}

impl Sampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self, enabled: &BTreeSet<String>) -> Vec<DataPoint> {
        let mut points = Vec::new();
        if enabled.contains(SYSTEM) {
            self.system(&mut points);
        }
        if enabled.contains(VOLUME) {
            self.volume(&mut points);
        }
        if enabled.contains(BATTERY) {
            self.battery(&mut points);
        }
        points
    }

    fn observe<T>(&mut self, key: &'static str, result: io::Result<T>) -> Option<T> {
        match result {
            Ok(value) => {
                if self.failures.remove(key) {
                    tracing::info!(source = key, "metric provider recovered");
                }
                Some(value)
            }
            Err(error) => {
                if self.failures.insert(key) {
                    tracing::warn!(source = key, %error, "metric provider unavailable");
                }
                None
            }
        }
    }

    fn system(&mut self, points: &mut Vec<DataPoint>) {
        if let Some(cpu) = self.observe("cpu", cpu_snapshot()) {
            if let Some((total, idle)) = self.last_cpu {
                let elapsed = cpu.0.checked_sub(total);
                let idle_elapsed = cpu.1.checked_sub(idle);
                if let (Some(elapsed), Some(idle_elapsed)) = (elapsed, idle_elapsed)
                    && elapsed > 0
                    && idle_elapsed <= elapsed
                {
                    percent(
                        points,
                        "sys/cpu/usage",
                        "CPU usage",
                        SYSTEM,
                        100.0 * (elapsed - idle_elapsed) as f32 / elapsed as f32,
                    );
                }
            }
            self.last_cpu = Some(cpu);
        } else {
            self.last_cpu = None;
        }
        if let Some((ram, swap)) = self.observe("memory", memory()) {
            if let Some(value) = ram {
                percent(points, "sys/ram/usage", "RAM usage", SYSTEM, value);
            }
            if let Some(value) = swap {
                percent(points, "sys/swap/usage", "Swap usage", SYSTEM, value);
            }
        }
        self.observe("disk", disk_usage(points));
        if let Some(net) = self.observe("network", network()) {
            for (path, name, value) in [
                ("sys/net/received/packets", "Received packets", net[0]),
                ("sys/net/received/err", "Receive errors", net[1]),
                ("sys/net/sent/packets", "Sent packets", net[2]),
                ("sys/net/sent/err", "Send errors", net[3]),
            ] {
                points.push(point(
                    path,
                    name,
                    SYSTEM,
                    DataValue::Integer(value.min(i64::MAX as u64) as i64),
                    0.0,
                    f32::MAX,
                ));
            }
        }
        if let Some(temperatures) = self.observe("temperature", temperatures()) {
            for (sensor, value) in temperatures {
                points.push(point(
                    &format!("comp/{sensor}/temp"),
                    &sensor,
                    SYSTEM,
                    DataValue::Float(value),
                    -40.0,
                    120.0,
                ));
            }
        }
        // The proprietary os/kind integer enum has not been mapped. Never publish a guessed value.
    }

    fn volume(&mut self, points: &mut Vec<DataPoint>) {
        if let Some(value) = self.observe("volume", pactl("get-sink-volume")) {
            if let Some(level) = parse_volume(&value) {
                points.push(point(
                    "audio/speaker/level",
                    "Speaker volume",
                    VOLUME,
                    DataValue::Float(level),
                    0.0,
                    level.max(100.0),
                ));
            } else {
                self.observe::<()>(
                    "volume",
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid pactl sink volume",
                    )),
                );
            }
        }
        if let Some(value) = self.observe("mute", pactl("get-sink-mute")) {
            let mute = match value.trim() {
                "Mute: yes" => Some(true),
                "Mute: no" => Some(false),
                _ => None,
            };
            if let Some(mute) = mute {
                points.push(point(
                    "audio/speaker/mute",
                    "Speaker muted",
                    VOLUME,
                    DataValue::Bool(mute),
                    0.0,
                    1.0,
                ));
            } else {
                self.observe::<()>(
                    "mute",
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid pactl mute state",
                    )),
                );
            }
        }
    }

    fn battery(&mut self, points: &mut Vec<DataPoint>) {
        // A desktop with no battery is not a failed provider.
        if let Some(Some((level, charging))) = self.observe("battery", batteries()) {
            percent(points, "battery/level", "Battery level", BATTERY, level);
            if let Some(charging) = charging {
                points.push(point(
                    "battery/charging",
                    "Battery charging",
                    BATTERY,
                    DataValue::Bool(charging),
                    0.0,
                    1.0,
                ));
            }
        }
    }
}

fn point(path: &str, name: &str, source: &str, value: DataValue, min: f32, max: f32) -> DataPoint {
    DataPoint {
        path: path.into(),
        name: name.into(),
        source: source.into(),
        value,
        min,
        max,
    }
}

fn percent(points: &mut Vec<DataPoint>, path: &str, name: &str, source: &str, value: f32) {
    if value.is_finite() {
        points.push(point(
            path,
            name,
            source,
            DataValue::Float(value.clamp(0.0, 100.0)),
            0.0,
            100.0,
        ));
    }
}

fn bounded_file(path: &Path, max: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "system file too large",
        ));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn invalid(field: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, field)
}

fn cpu_snapshot() -> io::Result<(u64, u64)> {
    let stat = bounded_file(Path::new("/proc/stat"), MAX_PROC_BYTES)?;
    let fields: Vec<u64> = stat
        .lines()
        .next()
        .ok_or_else(|| invalid("missing cpu counters"))?
        .strip_prefix("cpu ")
        .ok_or_else(|| invalid("missing aggregate cpu"))?
        .split_whitespace()
        .take(10)
        .map(|n| n.parse().map_err(|_| invalid("invalid cpu counter")))
        .collect::<io::Result<_>>()?;
    if fields.len() < 4 {
        return Err(invalid("incomplete cpu counters"));
    }
    // Guest and guest_nice are already included in user and nice; exclude them from total.
    let total = fields
        .iter()
        .take(8)
        .try_fold(0u64, |a, n| a.checked_add(*n))
        .ok_or_else(|| invalid("cpu overflow"))?;
    let idle = fields[3]
        .checked_add(*fields.get(4).unwrap_or(&0))
        .ok_or_else(|| invalid("cpu idle overflow"))?;
    Ok((total, idle))
}

fn memory() -> io::Result<(Option<f32>, Option<f32>)> {
    let text = bounded_file(Path::new("/proc/meminfo"), MAX_PROC_BYTES)?;
    let mut total = None;
    let mut available = None;
    let mut swap_total = None;
    let mut swap_free = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(key, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree") {
            continue;
        }
        let n: u64 = value
            .split_whitespace()
            .next()
            .ok_or_else(|| invalid("missing memory value"))?
            .parse()
            .map_err(|_| invalid("invalid memory value"))?;
        match key {
            "MemTotal" => total = Some(n),
            "MemAvailable" => available = Some(n),
            "SwapTotal" => swap_total = Some(n),
            "SwapFree" => swap_free = Some(n),
            _ => unreachable!(),
        }
    }
    let ram = match (total, available) {
        (Some(t), Some(a)) if t > 0 && a <= t => Some(100.0 * (t - a) as f32 / t as f32),
        _ => None,
    };
    // Zero swap capacity means unavailable, not 0% used.
    let swap = match (swap_total, swap_free) {
        (Some(t), Some(f)) if t > 0 && f <= t => Some(100.0 * (t - f) as f32 / t as f32),
        _ => None,
    };
    if ram.is_none() {
        return Err(invalid("missing RAM usage"));
    }
    Ok((ram, swap))
}

fn network() -> io::Result<[u64; 4]> {
    let text = bounded_file(Path::new("/proc/net/dev"), MAX_PROC_BYTES)?;
    let mut totals = [0u64; 4];
    let mut found = false;
    for line in text.lines().skip(2) {
        let Some((interface, counters)) = line.split_once(':') else {
            continue;
        };
        if interface.trim() == "lo" {
            continue;
        }
        let values: Vec<u64> = counters
            .split_whitespace()
            .take(12)
            .map(|n| n.parse().map_err(|_| invalid("invalid network counter")))
            .collect::<io::Result<_>>()?;
        if values.len() < 11 {
            return Err(invalid("incomplete network counters"));
        }
        for (sum, value) in totals
            .iter_mut()
            .zip([values[1], values[2], values[9], values[10]])
        {
            *sum = sum
                .checked_add(value)
                .ok_or_else(|| invalid("network counter overflow"))?;
        }
        found = true;
    }
    if !found {
        return Err(invalid("no network interfaces"));
    }
    Ok(totals)
}

fn disk_usage(points: &mut Vec<DataPoint>) -> io::Result<()> {
    let text = bounded_file(Path::new("/proc/self/mountinfo"), MAX_PROC_BYTES)?;
    let mut devices = HashSet::new();
    let mut sources = HashSet::new();
    let mut total = 0u128;
    let mut free = 0u128;
    let mut attempted = 0usize;
    for line in text.lines() {
        let Some((mount, filesystem)) = line.split_once(" - ") else {
            continue;
        };
        let mut fields = filesystem.split_whitespace();
        let Some(fs_type) = fields.next() else {
            continue;
        };
        let Some(source) = fields.next() else {
            continue;
        };
        // Only local filesystems: statvfs of a network/FUSE mount can wait forever.
        if !matches!(
            fs_type,
            "ext2"
                | "ext3"
                | "ext4"
                | "btrfs"
                | "xfs"
                | "f2fs"
                | "zfs"
                | "vfat"
                | "exfat"
                | "ntfs"
                | "ntfs3"
                | "overlay"
        ) {
            continue;
        }
        let Some(encoded) = mount.split_whitespace().nth(4) else {
            continue;
        };
        let Some(path) = decode_mount(encoded) else {
            continue;
        };
        attempted += 1;
        if attempted > MAX_MOUNTS {
            break;
        }
        let path = Path::new(&path);
        let Ok(dev) = fs::metadata(path).map(|meta| meta.dev()) else {
            continue;
        };
        // Btrfs subvolumes can have distinct st_dev values for the same disk.
        if devices.contains(&dev) || sources.contains(source) {
            continue;
        }
        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            continue;
        };
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: statvfs writes its initialized structure to this valid pointer.
        if unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) } != 0 {
            continue;
        }
        // SAFETY: success above initialized stat in full.
        let stat = unsafe { stat.assume_init() };
        let blocks = u128::from(stat.f_blocks);
        let free_blocks = u128::from(stat.f_bfree);
        if blocks == 0 || free_blocks > blocks {
            continue;
        }
        let block_size = u128::from(stat.f_frsize);
        total = total.saturating_add(blocks * block_size);
        free = free.saturating_add(free_blocks * block_size);
        devices.insert(dev);
        sources.insert(source);
        if let Some(source) =
            decode_mount(source).and_then(|path| path.into_os_string().into_string().ok())
        {
            percent(
                points,
                &format!("disk/{source}/used"),
                &format!("Disk usage ({source})"),
                SYSTEM,
                100.0 * (blocks - free_blocks) as f32 / blocks as f32,
            );
        }
    }
    if total == 0 {
        return Err(invalid("no readable local filesystems"));
    }
    percent(
        points,
        "disk/all/used",
        "Disk usage",
        SYSTEM,
        100.0 * (total - free.min(total)) as f32 / total as f32,
    );
    Ok(())
}

fn decode_mount(encoded: &str) -> Option<PathBuf> {
    let mut out = Vec::with_capacity(encoded.len());
    let mut iter = encoded.as_bytes().iter().copied();
    while let Some(ch) = iter.next() {
        if ch == b'\\' {
            let digits = [iter.next()?, iter.next()?, iter.next()?];
            if !digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return None;
            }
            out.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + digits[2] - b'0');
        } else {
            out.push(ch);
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

fn temperatures() -> io::Result<Vec<(String, f32)>> {
    let mut readings = Vec::new();
    let entries = fs::read_dir("/sys/class/hwmon")?;
    for hwmon in entries.take(MAX_SENSORS).flatten() {
        let root = hwmon.path();
        let chip = fs::read_to_string(root.join("name")).unwrap_or_default();
        let chip = sensor_key(chip.trim());
        let device = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let mut hash = 0x811c9dc5u32;
        for byte in device.as_os_str().as_bytes() {
            hash = (hash ^ u32::from(*byte)).wrapping_mul(0x01000193);
        }
        let Ok(files) = fs::read_dir(&root) else {
            continue;
        };
        for entry in files.take(MAX_SENSORS).flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if !file.starts_with("temp") || !file.ends_with("_input") {
                continue;
            }
            let index = &file[4..file.len() - 6];
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(raw) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(raw) = raw.trim().parse::<i64>() else {
                continue;
            };
            if !(-100_000..=300_000).contains(&raw) {
                continue;
            }
            let label =
                fs::read_to_string(root.join(format!("temp{index}_label"))).unwrap_or_default();
            let label = if label.trim().is_empty() {
                index.to_string()
            } else {
                sensor_key(label.trim())
            };
            let name = format!("{chip}-{label}-{hash:08x}");
            readings.push((name, raw as f32 / 1000.0));
        }
    }
    if readings.is_empty() {
        return Err(invalid("no readable temperature sensors"));
    }
    Ok(readings)
}

fn sensor_key(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars().take(128) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    if out.is_empty() { "sensor".into() } else { out }
}

fn batteries() -> io::Result<Option<(f32, Option<bool>)>> {
    let mut levels = Vec::new();
    let mut charging = None;
    let mut statuses_known = true;
    let mut found = false;
    for entry in fs::read_dir("/sys/class/power_supply")?.take(64).flatten() {
        let root = entry.path();
        if fs::read_to_string(root.join("type"))?.trim() != "Battery" {
            continue;
        }
        found = true;
        if fs::read_to_string(root.join("present"))
            .ok()
            .is_some_and(|s| s.trim() == "0")
        {
            continue;
        }
        let weighted = [("energy_now", "energy_full"), ("charge_now", "charge_full")]
            .into_iter()
            .enumerate()
            .find_map(|(unit, (now, full))| {
                let now = read_u64(&root.join(now))?;
                let full = read_u64(&root.join(full))?;
                (full > 0 && now <= full).then_some((now, full, unit))
            });
        let level = if let Some((now, full, unit)) = weighted {
            Some((now as f64 / full as f64 * 100.0, Some((full, unit))))
        } else {
            read_u64(&root.join("capacity"))
                .filter(|n| *n <= 100)
                .map(|n| (n as f64, None))
        };
        if let Some(level) = level {
            levels.push(level);
        } else {
            return Err(invalid("battery capacity unavailable"));
        }
        match fs::read_to_string(root.join("status"))
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("Charging") => charging = Some(true),
            Some("Discharging" | "Full" | "Not charging") if charging != Some(true) => {
                charging = Some(false)
            }
            Some("Discharging" | "Full" | "Not charging") => {}
            _ => statuses_known = false,
        }
    }
    if levels.is_empty() {
        return if found {
            Err(invalid("battery capacity unavailable"))
        } else {
            Ok(None)
        };
    }
    // For mixed battery types (mAh and mWh), neither their capacities nor their weights
    // can be combined physically. Average their individual percentages in that case.
    let unit = levels[0].1.map(|(_, unit)| unit);
    let same_unit = unit.is_some()
        && levels
            .iter()
            .all(|(_, weight)| weight.map(|(_, u)| u) == unit);
    let level = if same_unit {
        let total: u128 = levels
            .iter()
            .map(|(_, weight)| u128::from(weight.unwrap().0))
            .sum();
        if total == 0 {
            return Err(invalid("zero battery capacity"));
        }
        levels
            .iter()
            .map(|(pct, weight)| pct * weight.unwrap().0 as f64)
            .sum::<f64>()
            / total as f64
    } else {
        levels.iter().map(|(pct, _)| pct).sum::<f64>() / levels.len() as f64
    };
    Ok(Some((
        level as f32,
        if charging == Some(true) || statuses_known {
            charging
        } else {
            None
        },
    )))
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn pactl(operation: &str) -> io::Result<String> {
    let mut child = Command::new("pactl")
        .env("LC_ALL", "C")
        .args([operation, "@DEFAULT_SINK@"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(invalid("pactl query failed"));
            }
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .ok_or_else(|| invalid("pactl stdout unavailable"))?
                .take(4097)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                return Err(invalid("pactl output too large"));
            }
            return String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        if start.elapsed() >= PACTL_TIMEOUT {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "pactl query timed out",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn parse_volume(text: &str) -> Option<f32> {
    // PulseAudio/PipeWire print one percentage per channel. Use their arithmetic mean,
    // preserving software amplification above 100%. Mute does not alter this level.
    let mut count = 0u32;
    let mut sum = 0.0;
    for channel in text.trim().strip_prefix("Volume:")?.split(',') {
        let fraction = channel.split('/').nth(1)?.trim();
        let percentage = fraction.strip_suffix('%')?.trim().parse::<f32>().ok()?;
        if !percentage.is_finite() || !(0.0..=1000.0).contains(&percentage) {
            return None;
        }
        sum += percentage;
        count += 1;
    }
    (count > 0).then_some(sum / count as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_volume_keeps_amplification_and_does_not_imply_mute() {
        assert_eq!(
            parse_volume(
                "Volume: front-left: 65536 / 100% / 0 dB, front-right: 98304 / 150% / 1 dB\n        balance 0.00"
            ),
            Some(125.0)
        );
        assert_eq!(parse_volume("Volume: mono: 0 / 0% / -inf dB"), Some(0.0));
        assert_eq!(parse_volume("Volume: mono: invalid / -inf dB"), None);
    }
}
