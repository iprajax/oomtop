//! Helpers for [`Measured<T>`] and [`Quality`] (SPEC §5: every number carries source + quality).

use crate::model::{Measured, Quality};

impl Quality {
    /// Severity order used when combining values: exact < estimate < unavailable.
    pub fn rank(&self) -> u8 {
        match self {
            Quality::Exact => 0,
            Quality::Estimate => 1,
            Quality::Unavailable(_) => 2,
        }
    }

    pub fn is_available(&self) -> bool {
        !matches!(self, Quality::Unavailable(_))
    }

    /// The weaker of two qualities (estimate beats exact; unavailable beats both).
    pub fn worst(self, other: Quality) -> Quality {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    /// Short label for UI/exports: "exact", "est.", "n/a".
    pub fn short_label(&self) -> &'static str {
        match self {
            Quality::Exact => "exact",
            Quality::Estimate => "est.",
            Quality::Unavailable(_) => "n/a",
        }
    }
}

impl<T> Measured<T> {
    pub fn exact(value: T, source: impl Into<String>) -> Self {
        Measured {
            value: Some(value),
            source: source.into(),
            quality: Quality::Exact,
        }
    }

    pub fn estimate(value: T, source: impl Into<String>) -> Self {
        Measured {
            value: Some(value),
            source: source.into(),
            quality: Quality::Estimate,
        }
    }

    pub fn unavailable(source: impl Into<String>, reason: impl Into<String>) -> Self {
        Measured {
            value: None,
            source: source.into(),
            quality: Quality::Unavailable(reason.into()),
        }
    }

    /// Value, if available.
    pub fn get(&self) -> Option<&T> {
        self.value.as_ref()
    }

    pub fn is_available(&self) -> bool {
        self.value.is_some() && self.quality.is_available()
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Measured<U> {
        Measured {
            value: self.value.map(f),
            source: self.source,
            quality: self.quality,
        }
    }

    /// Downgrades quality to `Estimate` (keeps `Unavailable`).
    pub fn into_estimate(mut self) -> Self {
        if self.quality == Quality::Exact {
            self.quality = Quality::Estimate;
        }
        self
    }

    /// Reason text when unavailable.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.quality {
            Quality::Unavailable(r) => Some(r.as_str()),
            _ => None,
        }
    }
}

impl<T: Copy> Measured<T> {
    pub fn value_or(&self, default: T) -> T {
        self.value.unwrap_or(default)
    }

    /// The value only when the quality is not `Unavailable` (guards against inconsistent inputs such as
    /// `value: Some(_)` with `quality: unavailable`, which must never be used as a number).
    pub fn usable(&self) -> Option<T> {
        self.value.filter(|_| self.quality.is_available())
    }
}

/// Sums byte values. The result is available if at least one input is; quality is the worst of the
/// available inputs, and becomes `Estimate` if some inputs were unavailable (partial sum).
pub fn sum_bytes<'a>(items: impl IntoIterator<Item = &'a Measured<u64>>, source: &str) -> Measured<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    let mut missing = false;
    let mut quality = Quality::Exact;
    let mut reason = String::from("no inputs");
    for m in items {
        match (&m.value, &m.quality) {
            (Some(v), q) if q.is_available() => {
                total = total.saturating_add(*v);
                any = true;
                quality = quality.worst(q.clone());
            }
            (_, Quality::Unavailable(r)) => {
                missing = true;
                reason = r.clone();
            }
            _ => missing = true,
        }
    }
    if !any {
        return Measured::unavailable(source, reason);
    }
    if missing {
        quality = quality.worst(Quality::Estimate);
    }
    Measured {
        value: Some(total),
        source: source.to_string(),
        quality,
    }
}

/// Sums f64 values with the same rules as [`sum_bytes`].
pub fn sum_f64<'a>(items: impl IntoIterator<Item = &'a Measured<f64>>, source: &str) -> Measured<f64> {
    let mut total = 0.0;
    let mut any = false;
    let mut missing = false;
    let mut quality = Quality::Exact;
    for m in items {
        match m.value {
            Some(v) if m.quality.is_available() => {
                total += v;
                any = true;
                quality = quality.worst(m.quality.clone());
            }
            _ => missing = true,
        }
    }
    if !any {
        return Measured::unavailable(source, "no inputs");
    }
    if missing {
        quality = quality.worst(Quality::Estimate);
    }
    Measured {
        value: Some(total),
        source: source.to_string(),
        quality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_quality_rules() {
        let a = Measured::exact(10u64, "a");
        let b = Measured::estimate(5u64, "b");
        let c: Measured<u64> = Measured::unavailable("c", "needs root");
        let s = sum_bytes([&a, &b], "sum");
        assert_eq!(s.value, Some(15));
        assert_eq!(s.quality, Quality::Estimate);
        let s = sum_bytes([&a, &c], "sum");
        assert_eq!(s.value, Some(10));
        assert_eq!(s.quality, Quality::Estimate);
        let s = sum_bytes([&c], "sum");
        assert_eq!(s.value, None);
        assert_eq!(s.unavailable_reason(), Some("needs root"));
        let s = sum_bytes([&a, &a], "sum");
        assert_eq!(s.quality, Quality::Exact);
    }

    #[test]
    fn usable_requires_available_quality() {
        assert_eq!(Measured::exact(3u64, "a").usable(), Some(3));
        let inconsistent = Measured {
            value: Some(3u64),
            source: "x".into(),
            quality: Quality::Unavailable("stale".into()),
        };
        assert_eq!(inconsistent.usable(), None);
        assert!(!inconsistent.is_available());
        assert_eq!(Measured::estimate(1.5f64, "b").map(|v| v * 2.0).value, Some(3.0));
        assert_eq!(
            Measured::exact(1u8, "c").into_estimate().quality,
            Quality::Estimate
        );
        let u: Measured<u8> = Measured::unavailable("d", "needs root");
        assert_eq!(u.into_estimate().unavailable_reason(), Some("needs root"));
    }

    #[test]
    fn worst_quality() {
        assert_eq!(Quality::Exact.worst(Quality::Estimate), Quality::Estimate);
        assert_eq!(Quality::Estimate.worst(Quality::Exact), Quality::Estimate);
    }
}
