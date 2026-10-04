// Profile protocol and protobuf layout adapted from KeyProfileBridge:
// https://github.com/apfelgriebsch/KeyProfileBridge/blob/main/bin/keyprofile-bridge
// Copyright (c) 2026 SOLO ❯ CODES. Licensed under the MIT License:
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::{
    ffi::CString,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;

const VENDOR: u16 = 0x31e3;
const CONTROL_PAGE: u16 = 0xff55;
const CONTROL_USAGE: u16 = 1;
const CONTROL_DESCRIPTOR: [u8; 7] = [0x06, 0x55, 0xff, 0x09, 0x01, 0xa1, 0x01];
const TIMEOUT: Duration = Duration::from_millis(1500);
const CMD_CURRENT: u8 = 0x0b;
const CMD_ACTIVATE: u8 = 0x17;
const CMD_METADATA: u8 = 0x37;
const CMD_COUNT: u8 = 0x3e;
const CMD_SELECT_LINKED: u8 = 0x54;
const CMD_LINKED: u8 = 0x55;
const CMD_APP_COUNT: u8 = 0x56;
const CMD_APP: u8 = 0x57;

#[derive(Debug)]
pub struct FirmwareError {
    pub command: u8,
    pub status: u8,
}
impl std::fmt::Display for FirmwareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "firmware rejected command 0x{:02x} (status 0x{:02x}{})",
            self.command,
            self.status,
            if self.status == 0x66 {
                ", unsupported"
            } else {
                ""
            }
        )
    }
}
impl std::error::Error for FirmwareError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ProfileId {
    pub namespace: u8,
    pub index: u8,
}

impl ProfileId {
    fn parameter(self) -> u32 {
        u32::from(self.namespace) << 8 | u32::from(self.index)
    }

    fn supported(self) -> Result<Self> {
        ensure!(
            self.namespace <= 1,
            "unsupported profile namespace {} (expected onboard=0 or linked=1)",
            self.namespace
        );
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppFilter {
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LinkedApp {
    pub name: String,
    pub steam_id: Option<u32>,
    pub match_type: u32,
    pub filters: Vec<AppFilter>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub apps: Vec<LinkedApp>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeviceInfo {
    pub path: String,
    pub serial: String,
    pub product: String,
    pub product_id: u16,
}

pub struct Device {
    pub info: DeviceInfo,
    handle: HidDevice,
}

// The FF55/usage 1 application collection identifies the modern control
// interface; keyboard/consumer/analog input interfaces must not be opened.
// https://github.com/meeuw/libwootility/blob/main/libwootility/device_linux.py
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let api = HidApi::new().context("enumerating HID devices")?;
    Ok(api
        .device_list()
        .filter(|device| {
            device.vendor_id() == VENDOR
                && device.usage_page() == CONTROL_PAGE
                && device.usage() == CONTROL_USAGE
        })
        .map(|device| DeviceInfo {
            path: device.path().to_string_lossy().into_owned(),
            serial: device.serial_number().unwrap_or_default().to_owned(),
            product: device.product_string().unwrap_or_default().to_owned(),
            product_id: device.product_id(),
        })
        .collect())
}

impl Device {
    pub fn open(info: &DeviceInfo) -> Result<Self> {
        let api = HidApi::new().context("enumerating HID devices for control interface")?;
        let path = CString::new(info.path.as_bytes()).context("invalid HID path")?;
        let listed = api.device_list().any(|candidate| {
            candidate.path() == path.as_c_str()
                && candidate.vendor_id() == VENDOR
                && candidate.product_id() == info.product_id
                && candidate.usage_page() == CONTROL_PAGE
                && candidate.usage() == CONTROL_USAGE
        });
        ensure!(
            listed,
            "{} is not a connected Wooting FF55/usage-1 control interface; reconnect keyboard or check access to /dev/hidraw*",
            info.path
        );
        let handle = api.open_path(&path).with_context(|| {
            format!(
                "opening Wooting control interface {} (check hidraw permissions)",
                info.path
            )
        })?;
        let mut descriptor = [0; 4096];
        let len = handle
            .get_report_descriptor(&mut descriptor)
            .with_context(|| format!("reading FF55 report descriptor at {}", info.path))?;
        ensure!(
            descriptor[..len].starts_with(&CONTROL_DESCRIPTOR),
            "{} does not expose the expected modern FF55 control descriptor; legacy HID protocol is unsupported",
            info.path
        );
        Ok(Self {
            info: info.clone(),
            handle,
        })
    }

    /// Issue a modern FF55 feature query, reading its matching input report.
    /// Only known profile commands and the data-subscription query are allowed;
    /// this interface cannot accidentally send flash-save/reset commands.
    pub fn query(&self, command: u8, param: u32) -> Result<Vec<u8>> {
        ensure!(
            matches!(
                command,
                CMD_CURRENT
                    | CMD_ACTIVATE
                    | CMD_METADATA
                    | CMD_COUNT
                    | CMD_SELECT_LINKED
                    | CMD_LINKED
                    | CMD_APP_COUNT
                    | CMD_APP
                    | 0x41
            ),
            "HID command 0x{command:02x} is not an approved volatile/profile query"
        );
        let deadline = Instant::now() + TIMEOUT;
        let mut stale = [0u8; 4096];
        // Nonblocking drain prevents an earlier query's reply being mistaken for this one.
        loop {
            ensure!(
                Instant::now() < deadline,
                "timed out draining stale HID replies on {}; device control interface may be flooding",
                self.info.path
            );
            if self
                .handle
                .read_timeout(&mut stale, 0)
                .with_context(|| format!("draining stale replies from {}", self.info.path))?
                == 0
            {
                break;
            }
        }
        let mut feature = [0u8; 8]; // report ID 0; d1 da; command; u32 LE parameter
        feature[1..3].copy_from_slice(&[0xd1, 0xda]);
        feature[3] = command;
        feature[4..8].copy_from_slice(&param.to_le_bytes());
        self.handle.send_feature_report(&feature).with_context(|| {
            format!(
                "sending feature command 0x{command:02x} to {}",
                self.info.path
            )
        })?;
        let mut last_invalid = None;
        let mut frame = [0u8; 4096];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let detail = last_invalid.unwrap_or_else(|| "no matching response".to_owned());
                bail!(
                    "HID command 0x{command:02x} timed out after 1.5s on {}: {detail}; check firmware App Linking support and device access",
                    self.info.path
                );
            }
            let ms = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
            let len = self.handle.read_timeout(&mut frame, ms).with_context(|| {
                format!(
                    "reading command 0x{command:02x} response from {}",
                    self.info.path
                )
            })?;
            if len == 0 {
                continue;
            }
            match parse_reply(&frame[..len], command) {
                Ok(reply) if reply.status == 0x88 => return Ok(reply.body.to_vec()),
                Ok(reply) => {
                    return Err(FirmwareError {
                        command,
                        status: reply.status,
                    }
                    .into());
                }
                Err(error) => last_invalid = Some(error.to_string()),
            }
        }
    }

    pub fn current_profile(&self) -> Result<ProfileId> {
        let body = self.query(CMD_CURRENT, 0)?;
        ensure!(
            !body.is_empty(),
            "current-profile reply from {} has no profile index",
            self.info.path
        );
        ProfileId {
            index: body[0],
            namespace: body.get(1).copied().unwrap_or(0),
        }
        .supported()
    }

    pub fn linked_profile(&self) -> Result<Option<ProfileId>> {
        let body = self.query(CMD_LINKED, 0)?;
        match body.as_slice() {
            [] => Ok(None),
            [index, namespace] => Ok(Some(
                ProfileId {
                    namespace: *namespace,
                    index: *index,
                }
                .supported()?,
            )),
            _ => bail!(
                "linked-profile reply from {} has invalid length {} (expected 0 or 2)",
                self.info.path,
                body.len()
            ),
        }
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let mut profiles = Vec::new();
        for namespace in [0u8, 1] {
            let count = self
                .query(CMD_COUNT, u32::from(namespace))
                .with_context(|| format!("reading profile count for namespace {namespace}"))?;
            let count = *count.first().context(
                "profile-count reply has no count byte; firmware may not support App Linking",
            )?;
            for index in 0..count {
                let id = ProfileId { namespace, index };
                let metadata = self
                    .query(CMD_METADATA, id.parameter())
                    .with_context(|| format!("reading metadata for profile {namespace}/{index}"))?;
                let name = metadata_name(&metadata).with_context(|| {
                    format!("decoding metadata for profile {namespace}/{index}")
                })?;
                let mut apps = Vec::new();
                if namespace == 1 {
                    let count = self
                        .query(CMD_APP_COUNT, id.parameter())
                        .with_context(|| format!("reading linked-app count for profile {index}"))?;
                    let count = *count
                        .first()
                        .context("linked-app count reply has no count byte")?;
                    ensure!(
                        count <= 8,
                        "linked profile {index} reports {count} apps (firmware supports at most 8)"
                    );
                    for app_index in 0..count {
                        let body = self
                            .query(CMD_APP, id.parameter() | (u32::from(app_index) << 16))
                            .with_context(|| {
                                format!("reading linked app {app_index} for profile {index}")
                            })?;
                        apps.push(parse_app(&body).with_context(|| {
                            format!("decoding linked app {app_index} for profile {index}")
                        })?);
                    }
                }
                profiles.push(Profile { id, name, apps });
            }
        }
        Ok(profiles)
    }

    pub fn select_linked(&self, profile: Option<ProfileId>) -> Result<()> {
        let param = match profile {
            Some(id) => {
                ensure!(
                    id.namespace == 1,
                    "linked-profile selection requires namespace 1, got {}",
                    id.namespace
                );
                id.parameter()
            }
            None => 0xffff,
        };
        self.query(CMD_SELECT_LINKED, param)?;
        let actual = self.linked_profile()?;
        ensure!(
            actual == profile,
            "linked-profile selection was not applied on {}: requested {profile:?}, got {actual:?}",
            self.info.path
        );
        Ok(())
    }

    pub fn activate(&self, id: ProfileId) -> Result<()> {
        let id = id.supported()?;
        self.query(CMD_ACTIVATE, id.parameter())?;
        let actual = self.current_profile()?;
        ensure!(
            actual == id,
            "profile activation was not applied on {}: requested {id:?}, got {actual:?}",
            self.info.path
        );
        Ok(())
    }

    /// Raw interrupt OUT only; report ID and all framing are supplied by caller.
    /// Unlike feature queries, this does not consume or wait for an input response.
    pub(crate) fn write_report(&self, report: &[u8]) -> Result<usize> {
        ensure!(!report.is_empty(), "HID output report needs a report ID");
        self.handle
            .write(report)
            .with_context(|| format!("writing HID output report to {}", self.info.path))
    }
}

struct Reply<'a> {
    status: u8,
    body: &'a [u8],
}

fn parse_reply(raw: &[u8], expected_command: u8) -> Result<Reply<'_>> {
    // Input report IDs 1..6 belong to this descriptor; hidapi may strip the ID.
    // A marker in arbitrary input data must not be accepted as a reply.
    let offset = if raw.starts_with(&[0xd1, 0xda]) {
        0
    } else {
        ensure!(
            raw.first().is_some_and(|id| (1..=6).contains(id)),
            "not an FF55 input report"
        );
        (1..=4)
            .find(|&offset| {
                raw.get(offset..offset + 2) == Some(&[0xd1, 0xda])
                    && raw[1..offset].iter().all(|byte| *byte == 0)
            })
            .context("missing FF55 magic in input-report prefix")?
    };
    let data = &raw[offset..];
    ensure!(data.len() >= 6, "short FF55 reply header");
    ensure!(
        data[2] == 0 || data[2] == expected_command,
        "stale/unexpected command 0x{:02x}, expected 0x{expected_command:02x}",
        data[2]
    );
    let body_len = usize::from(u16::from_le_bytes([data[4], data[5]]));
    ensure!(
        body_len <= data.len() - 6,
        "truncated FF55 reply body: announced {body_len}, received {}",
        data.len() - 6
    );
    Ok(Reply {
        status: data[3],
        body: &data[6..6 + body_len],
    })
}

// Small bounded protobuf reader: skip unknown varint, fixed-width and
// length-delimited fields without assuming that their field order is stable.
struct Fields<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Fields<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for byte_index in 0..10 {
            let byte = *self
                .data
                .get(self.pos)
                .context("truncated protobuf varint")?;
            self.pos += 1;
            ensure!(byte_index != 9 || byte <= 1, "protobuf varint exceeds u64");
            value |= u64::from(byte & 0x7f) << (byte_index * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("protobuf varint exceeds 10 bytes")
    }

    fn next(&mut self) -> Result<Option<(u64, Field<'a>)>> {
        if self.pos == self.data.len() {
            return Ok(None);
        }
        let tag = self.varint()?;
        let number = tag >> 3;
        ensure!(number != 0, "protobuf field zero is invalid");
        let field = match tag & 7 {
            0 => Field::Integer(self.varint()?),
            1 | 5 => {
                let n = if tag & 7 == 1 { 8 } else { 4 };
                let end = self
                    .pos
                    .checked_add(n)
                    .context("protobuf field offset overflow")?;
                ensure!(
                    end <= self.data.len(),
                    "truncated protobuf fixed-width field"
                );
                self.pos = end;
                Field::Other
            }
            2 => {
                let size = usize::try_from(self.varint()?).context("protobuf length overflow")?;
                let end = self
                    .pos
                    .checked_add(size)
                    .context("protobuf field offset overflow")?;
                ensure!(
                    end <= self.data.len(),
                    "truncated protobuf length-delimited field"
                );
                let bytes = &self.data[self.pos..end];
                self.pos = end;
                Field::Bytes(bytes)
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        };
        Ok(Some((number, field)))
    }
}

enum Field<'a> {
    Integer(u64),
    Bytes(&'a [u8]),
    Other,
}

fn text(bytes: &[u8]) -> Result<String> {
    Ok(std::str::from_utf8(bytes)
        .context("invalid UTF-8 in profile metadata")?
        .to_owned())
}

fn metadata_name(data: &[u8]) -> Result<String> {
    let mut fields = Fields::new(data);
    let mut name = None;
    while let Some((number, field)) = fields.next()? {
        if let (1, Field::Bytes(value)) = (number, field) {
            name = Some(text(value)?);
        }
    }
    name.context("profile metadata has no name field (field 1)")
}

fn parse_filter(data: &[u8]) -> Result<Option<AppFilter>> {
    let mut fields = Fields::new(data);
    let mut filter = None;
    while let Some((number, field)) = fields.next()? {
        let kind = match number {
            1 => "process_name",
            2 => "process_directory",
            3 => "window_title",
            4 => "process_full_path",
            _ => continue,
        };
        if let Field::Bytes(value) = field {
            // A filter is a protobuf oneof; the last recognized arm wins.
            filter = Some(AppFilter {
                kind: kind.to_owned(),
                value: text(value)?,
            });
        }
    }
    Ok(filter)
}

fn parse_custom_app(data: &[u8], app: &mut LinkedApp) -> Result<()> {
    let mut fields = Fields::new(data);
    while let Some((number, field)) = fields.next()? {
        match (number, field) {
            (1, Field::Integer(value)) => {
                app.match_type = u32::try_from(value).context("app match_type exceeds u32")?
            }
            (2, Field::Bytes(value)) => {
                if let Some(filter) = parse_filter(value)? {
                    app.filters.push(filter);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_app(data: &[u8]) -> Result<LinkedApp> {
    let mut app = LinkedApp {
        name: String::new(),
        steam_id: None,
        match_type: 0,
        filters: Vec::new(),
    };
    let mut fields = Fields::new(data);
    while let Some((number, field)) = fields.next()? {
        match (number, field) {
            (1, Field::Bytes(value)) => app.name = text(value)?,
            (2, Field::Bytes(value)) => parse_custom_app(value, &mut app)?,
            (3, Field::Integer(value)) => {
                app.steam_id = Some(u32::try_from(value).context("Steam app ID exceeds u32")?)
            }
            _ => {}
        }
    }
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_validate_report_header_command_and_length() {
        let good = [1, 0xd1, 0xda, 0x37, 0x88, 2, 0, 0x0a, 0];
        assert_eq!(parse_reply(&good, 0x37).unwrap().body, &[0x0a, 0]);
        assert!(parse_reply(&good, 0x3e).is_err());
        assert!(parse_reply(&[9, 0xd1, 0xda, 0x37, 0x88, 0, 0], 0x37).is_err());
        assert!(parse_reply(&[1, 0, 9, 0xd1, 0xda, 0x37, 0x88, 0, 0], 0x37).is_err());
        assert!(parse_reply(&good[..8], 0x37).is_err());
    }

    #[test]
    fn nested_apps_skip_unknown_fields_and_reject_truncation() {
        // Outer custom_app (field 2) contains match_type=1 and one title filter.
        let app = [
            0x0a, 4, b'T', b'e', b's', b't', 0x12, 14, 8, 1, 0x12, 10, 0x1a, 8, b'K', b'e', b'y',
            b'b', b'o', b'a', b'r', b'd', 0x18, 0xac, 2, 0x20, 3,
        ];
        let parsed = parse_app(&app).unwrap();
        assert_eq!(parsed.name, "Test");
        assert_eq!(parsed.steam_id, Some(300));
        assert_eq!(parsed.match_type, 1);
        assert_eq!(
            parsed.filters,
            vec![AppFilter {
                kind: "window_title".into(),
                value: "Keyboard".into()
            }]
        );
        let mut broken = app;
        broken[7] = 0x7f;
        assert!(parse_app(&broken).is_err());
        assert!(parse_app(&[0x12, 3, 8, 1]).is_err());
        assert!(metadata_name(&[0x0a, 0x80]).is_err());
        assert!(
            parse_app(&[
                0x08, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02
            ])
            .is_err()
        );
    }
}
