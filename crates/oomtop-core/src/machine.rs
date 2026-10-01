//! Human machine names (UX §2 "MacBook Air M5 · 24 GB unified"): the collector reports the raw hardware
//! identifier (`hw.model`, e.g. `Mac17,3`), which is not what people call their machine.
//!
//! Apple Silicon identifiers (`MacNN,M`) carry no family name, so a small table maps the known ones to a
//! family; the chip comes from `machdep.cpu.brand_string` ("Apple M5" → "M5"). Unknown identifiers keep the
//! identifier visible next to the chip ("Apple M5 (Mac99,1)") rather than guessing a family.

use crate::model::HostInfo;

/// Family of a Mac hardware identifier, if known.
pub fn mac_family(identifier: &str) -> Option<&'static str> {
    // Pre-2022 identifiers name the family themselves.
    for (prefix, family) in [
        ("MacBookAir", "MacBook Air"),
        ("MacBookPro", "MacBook Pro"),
        ("Macmini", "Mac mini"),
        ("iMacPro", "iMac Pro"),
        ("iMac", "iMac"),
        ("MacPro", "Mac Pro"),
    ] {
        if identifier.starts_with(prefix) {
            return Some(family);
        }
    }
    Some(match identifier {
        "Mac14,2" | "Mac14,15" | "Mac15,12" | "Mac15,13" | "Mac16,12" | "Mac16,13" | "Mac17,3" => {
            "MacBook Air"
        }
        "Mac14,5" | "Mac14,6" | "Mac14,7" | "Mac14,9" | "Mac14,10" | "Mac15,3" | "Mac15,6" | "Mac15,7"
        | "Mac15,8" | "Mac15,9" | "Mac15,10" | "Mac15,11" | "Mac16,1" | "Mac16,5" | "Mac16,6" | "Mac16,7"
        | "Mac16,8" => "MacBook Pro",
        "Mac14,3" | "Mac14,12" | "Mac16,10" | "Mac16,11" => "Mac mini",
        "Mac13,1" | "Mac13,2" | "Mac14,13" | "Mac14,14" | "Mac15,14" | "Mac16,9" => "Mac Studio",
        "Mac15,4" | "Mac15,5" | "Mac16,2" | "Mac16,3" => "iMac",
        "Mac14,8" => "Mac Pro",
        _ => return None,
    })
}

/// Display name of the machine: "MacBook Air M5" for a known Mac, "Apple M5 (Mac99,1)" for an unknown one,
/// else the model string, the CPU brand or the hostname.
pub fn display_name(h: &HostInfo) -> String {
    let brand = h.cpu_brand.as_deref().map(str::trim).filter(|b| !b.is_empty());
    let chip = brand.map(|b| b.strip_prefix("Apple ").unwrap_or(b));
    match (h.model.as_deref().map(str::trim).filter(|m| !m.is_empty()), chip) {
        (Some(m), Some(chip)) if brand.is_some_and(|b| b.starts_with("Apple")) => match mac_family(m) {
            Some(family) => format!("{family} {chip}"),
            None => format!("{} ({m})", brand.unwrap_or(chip)),
        },
        (Some(m), _) => mac_family(m).map(str::to_string).unwrap_or_else(|| m.to_string()),
        (None, _) => brand.map(str::to_string).unwrap_or_else(|| h.hostname.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(model: Option<&str>, brand: Option<&str>) -> HostInfo {
        HostInfo {
            hostname: "air".into(),
            model: model.map(Into::into),
            cpu_brand: brand.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn names_the_m5_air_like_people_do() {
        assert_eq!(
            display_name(&host(Some("Mac17,3"), Some("Apple M5"))),
            "MacBook Air M5"
        );
        assert_eq!(
            display_name(&host(Some("Mac14,2"), Some("Apple M2"))),
            "MacBook Air M2"
        );
        assert_eq!(
            display_name(&host(Some("Mac99,1"), Some("Apple M9"))),
            "Apple M9 (Mac99,1)",
            "unknown identifiers are not guessed"
        );
        assert_eq!(
            display_name(&host(Some("MacBookPro16,1"), Some("Intel(R) Core(TM) i9"))),
            "MacBook Pro"
        );
        // Linux: DMI product name, or the CPU brand, or the hostname.
        assert_eq!(
            display_name(&host(Some("ThinkPad X1"), Some("AMD Ryzen 7"))),
            "ThinkPad X1"
        );
        assert_eq!(display_name(&host(None, Some("AMD Ryzen 7"))), "AMD Ryzen 7");
        assert_eq!(display_name(&host(None, None)), "air");
    }
}
