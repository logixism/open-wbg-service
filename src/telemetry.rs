use anyhow::{Result, ensure};
use prost::Message;

use crate::{
    hid::{Device, FirmwareError},
    metrics::{DataPoint, DataValue},
    proto::{self, Subscriptions},
};

pub fn subscriptions(device: &Device) -> Result<Option<Subscriptions>> {
    match device.query(0x41, 0) {
        Ok(bytes) => Ok(Some(Subscriptions::decode(bytes.as_slice())?)),
        Err(error)
            if error
                .downcast_ref::<FirmwareError>()
                // Wootility's FF55 response enum: UNSUPPORTED = 0xaa.
                .is_some_and(|e| e.status == 0xaa) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn value(point: &DataPoint) -> f32 {
    match point.value {
        DataValue::Float(value) => value,
        DataValue::Bool(value) => u8::from(value) as f32,
        DataValue::Integer(value) => value as f32,
    }
}

/// Captured 80HE FF55 interrupt-OUT DataSubscriptionsUpdate, one source per
/// report. Source: github.com/funnyferrell/Wooting-80HE-Lightbar-Undocumented-USB-Control
/// WOOTING_LIGHTBAR.md. The protobuf schema is independently present in Wootility.
/// Use the observed 33-byte report, not speculative large-report/chunk framing.
fn report(index: u32, value: f32) -> Result<[u8; 33]> {
    ensure!(
        index < 128,
        "unsupported telemetry subscription slot {index}: exceeds verified one-byte slot encoding"
    );
    ensure!(value.is_finite(), "non-finite telemetry value");
    let mut report = [0; 33];
    report[..11].copy_from_slice(&[1, 0xd1, 0xda, 0x1f, 9, 0, 0x0a, 7, 8, index as u8, 0x15]);
    report[11..15].copy_from_slice(&value.to_le_bytes());
    Ok(report)
}

pub fn update(device: &Device, subscriptions: &Subscriptions, points: &[DataPoint]) -> Result<()> {
    for subscription in &subscriptions.subscriptions {
        let Some(point) = points
            .iter()
            .find(|point| proto::point_hash(&point.path) == subscription.hash)
        else {
            continue;
        };
        let report = report(subscription.index, value(point))?;
        ensure!(
            device.write_report(&report)? == report.len(),
            "short keyboard telemetry write"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn volume_update_matches_captured_80he_report() {
        let report = report(0, 75.0).unwrap();
        assert_eq!(
            &report[..15],
            &[1, 0xd1, 0xda, 0x1f, 9, 0, 10, 7, 8, 0, 21, 0, 0, 150, 66]
        );
        assert!(report[15..].iter().all(|b| *b == 0));
        assert!(self::report(128, 75.0).is_err());
        assert!(self::report(0, f32::NAN).is_err());
    }
    #[test]
    fn subscriptions_map_by_source_hash_not_position() {
        let subscriptions =
            Subscriptions::decode(&[10, 8, 8, 4, 16, 242, 223, 195, 196, 8][..]).unwrap();
        assert_eq!(subscriptions.subscriptions[0].index, 4);
        assert_eq!(
            subscriptions.subscriptions[0].hash,
            proto::point_hash("audio/speaker/level")
        );
    }
}
