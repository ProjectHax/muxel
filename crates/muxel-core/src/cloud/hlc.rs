//! Hybrid logical clock stamps for last-writer-wins merging.
//!
//! Wire form (protocol §3.3): `"<wall_ms:013>-<counter:04x>-<device_id>"`, which
//! orders correctly as a plain string comparison.

use std::cmp::Ordering;
use std::fmt;

use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Hlc {
    pub wall_ms: u64,
    pub counter: u16,
    pub device: Uuid,
}

impl Hlc {
    /// The zero stamp: older than anything a device produces.
    pub const ZERO: Hlc = Hlc {
        wall_ms: 0,
        counter: 0,
        device: Uuid::nil(),
    };

    /// The next stamp on `device` at wall time `now_ms`, strictly after `last`
    /// (the newest stamp this device has produced or observed).
    pub fn tick(last: Hlc, now_ms: u64, device: Uuid) -> Hlc {
        if now_ms > last.wall_ms {
            Hlc {
                wall_ms: now_ms,
                counter: 0,
                device,
            }
        } else if last.counter < u16::MAX {
            Hlc {
                wall_ms: last.wall_ms,
                counter: last.counter + 1,
                device,
            }
        } else {
            Hlc {
                wall_ms: last.wall_ms + 1,
                counter: 0,
                device,
            }
        }
    }

    pub fn parse(s: &str) -> Option<Hlc> {
        let mut parts = s.splitn(3, '-');
        let wall = parts.next()?;
        let counter = parts.next()?;
        let device = parts.next()?;
        if wall.len() != 13 || counter.len() != 4 {
            return None;
        }
        Some(Hlc {
            wall_ms: wall.parse().ok()?,
            counter: u16::from_str_radix(counter, 16).ok()?,
            device: Uuid::parse_str(device).ok()?,
        })
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:013}-{:04x}-{}",
            self.wall_ms,
            self.counter,
            self.device.hyphenated()
        )
    }
}

impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.wall_ms, self.counter, self.device).cmp(&(other.wall_ms, other.counter, other.device))
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl serde::Serialize for Hlc {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Hlc {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hlc::parse(&s).ok_or_else(|| serde::de::Error::custom("invalid HLC stamp"))
    }
}

#[cfg(test)]
mod tests {
    use super::Hlc;
    use uuid::Uuid;

    #[test]
    fn roundtrip_and_string_order_matches_ord() {
        let d = Uuid::from_u128(7);
        let a = Hlc {
            wall_ms: 1_700_000_000_000,
            counter: 3,
            device: d,
        };
        let b = Hlc::tick(a, 1_700_000_000_000, d);
        let c = Hlc::tick(b, 1_700_000_000_500, d);
        assert_eq!(Hlc::parse(&a.to_string()), Some(a));
        assert!(a < b && b < c);
        assert!(a.to_string() < b.to_string() && b.to_string() < c.to_string());
        assert_eq!(b.counter, 4);
        assert_eq!(c.counter, 0);
    }

    #[test]
    fn tick_is_monotonic_when_the_clock_goes_backwards() {
        let d = Uuid::from_u128(1);
        let last = Hlc {
            wall_ms: 5_000,
            counter: 0,
            device: d,
        };
        let next = Hlc::tick(last, 1_000, d);
        assert!(next > last);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Hlc::parse("nope"), None);
        assert_eq!(
            Hlc::parse("1-0000-00000000-0000-0000-0000-000000000000"),
            None
        );
    }
}
