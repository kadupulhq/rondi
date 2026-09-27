//! Snapshot serialization for Rondi's own versioned format.
//! This does not parse, emit, or rename RRDtool `.rrd` files.

use crate::format::{DatabaseFile, FORMAT_VERSION};
use crate::storage::StoreError;

pub(crate) fn encode_snapshot(db: &DatabaseFile) -> Result<Vec<u8>, StoreError> {
    Ok(serde_json::to_vec(db)?)
}

pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<DatabaseFile, StoreError> {
    let db: DatabaseFile = serde_json::from_slice(bytes)?;
    if db.version != FORMAT_VERSION {
        return Err(StoreError::FormatVersion(db.version));
    }
    if db.config.step == 0 || db.config.heartbeat == 0 || db.config.rows == 0 {
        return Err(StoreError::InvalidConfig(
            "snapshot step, heartbeat, and rows must be positive".into(),
        ));
    }
    let step = i64::try_from(db.config.step)
        .map_err(|_| StoreError::InvalidConfig("snapshot step exceeds timestamp range".into()))?;
    if db.points.len() > db.config.rows
        || db.last_update < db.config.start
        || db.bucket_start > db.last_update
        || db.last_update
            >= db
                .bucket_start
                .checked_add(step)
                .ok_or(StoreError::InvalidValue)?
        || db.known_seconds > db.config.step
        || !db.weighted_sum.is_finite()
    {
        return Err(StoreError::InvalidConfig(
            "snapshot state violates Rondi format invariants".into(),
        ));
    }
    let mut previous = None;
    for point in &db.points {
        if point.value.is_some_and(|value| !value.is_finite())
            || point.timestamp.rem_euclid(step) != 0
            || point.timestamp > db.last_update
            || previous.is_some_and(|timestamp| timestamp >= point.timestamp)
        {
            return Err(StoreError::InvalidConfig(
                "snapshot archive points are invalid or out of order".into(),
            ));
        }
        previous = Some(point.timestamp);
    }
    Ok(db)
}
