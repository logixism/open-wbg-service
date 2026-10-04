//! Wire-only interoperability definitions observed in Wootility 5.4.2's public
//! client (https://wootility.io/assets/index-BaoALhfv.js). No vendor code is linked.
use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Empty {}
#[derive(Clone, PartialEq, Message)]
pub struct Enabled {
    #[prost(bool, tag = "1")]
    pub enabled: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct Heartbeat {
    #[prost(string, tag = "1")]
    pub current_version: String,
    #[prost(bool, tag = "2")]
    pub auto_start_enabled: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct SetSource {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(bool, tag = "2")]
    pub enabled: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct SourceMetadata {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub icon: String,
    #[prost(string, tag = "4")]
    pub description: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct SourceInfo {
    #[prost(message, optional, tag = "1")]
    pub meta: Option<SourceMetadata>,
    #[prost(bool, tag = "2")]
    pub enabled: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct Sources {
    #[prost(message, repeated, tag = "1")]
    pub data_sources: Vec<SourceInfo>,
}
#[derive(Clone, PartialEq, Message)]
pub struct PointId {
    #[prost(bool, tag = "1")]
    pub is_internal: bool,
    #[prost(uint32, tag = "2")]
    pub id: u32,
}
#[derive(Clone, PartialEq, Message)]
pub struct NumberMetadata {
    #[prost(float, optional, tag = "1")]
    pub minimum: Option<f32>,
    #[prost(float, optional, tag = "2")]
    pub maximum: Option<f32>,
    #[prost(string, optional, tag = "3")]
    pub unit: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct EnumOption {
    #[prost(uint32, tag = "1")]
    pub value: u32,
    #[prost(string, tag = "2")]
    pub name: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct EnumMetadata {
    #[prost(message, repeated, tag = "1")]
    pub options: Vec<EnumOption>,
}
#[derive(Clone, PartialEq, Message)]
pub struct PointMetadata {
    #[prost(string, tag = "2")]
    pub key: String,
    #[prost(string, tag = "3")]
    pub title: String,
    #[prost(string, tag = "4")]
    pub description: String,
    #[prost(string, tag = "5")]
    pub category: String,
    #[prost(bool, tag = "6")]
    pub hidden: bool,
    #[prost(oneof = "point_metadata::ValueType", tags = "7, 8, 9, 10, 11")]
    pub value_type: Option<point_metadata::ValueType>,
}
pub mod point_metadata {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum ValueType {
        #[prost(message, tag = "7")]
        Number(super::NumberMetadata),
        #[prost(message, tag = "8")]
        Boolean(super::Empty),
        #[prost(message, tag = "9")]
        Enum(super::EnumMetadata),
        #[prost(message, tag = "10")]
        Event(super::Empty),
        #[prost(message, tag = "11")]
        Color(super::Empty),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct PointInfo {
    #[prost(message, optional, tag = "1")]
    pub data_point: Option<PointId>,
    #[prost(message, optional, tag = "2")]
    pub meta: Option<PointMetadata>,
    #[prost(float, tag = "3")]
    pub value: f32,
}
#[derive(Clone, PartialEq, Message)]
pub struct Points {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<PointInfo>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Subscription {
    #[prost(uint32, tag = "1")]
    pub index: u32,
    #[prost(uint32, tag = "2")]
    pub hash: u32,
}
#[derive(Clone, PartialEq, Message)]
pub struct Subscriptions {
    #[prost(message, repeated, tag = "1")]
    pub subscriptions: Vec<Subscription>,
}
#[derive(Clone, PartialEq, Message)]
pub struct AppRequest {
    #[prost(bool, tag = "1")]
    pub force_refresh: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct SteamApp {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(uint32, tag = "3")]
    pub app_id: u32,
}
#[derive(Clone, PartialEq, Message)]
pub struct DiskApp {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, optional, tag = "4")]
    pub executable: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Apps {
    #[prost(message, repeated, tag = "1")]
    pub steam_apps: Vec<SteamApp>,
    #[prost(message, repeated, tag = "2")]
    pub disk_apps: Vec<DiskApp>,
}
#[derive(Clone, PartialEq, Message)]
pub struct OpenWindow {
    #[prost(string, tag = "1")]
    pub title: String,
    #[prost(uint64, tag = "2")]
    pub process_id: u64,
    #[prost(string, tag = "3")]
    pub process_path: String,
    #[prost(string, tag = "4")]
    pub app_name: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct OpenWindows {
    #[prost(message, repeated, tag = "1")]
    pub open_windows: Vec<OpenWindow>,
}

/// Firmware and Wootility use FNV-1a over the UTF-8 source path, not CRC32.
pub fn point_hash(path: &str) -> u32 {
    path.bytes().fold(2_166_136_261, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}
