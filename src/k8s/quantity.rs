//! Kubernetes resource quantities: parsing them, and printing them back.
//!
//! Every number the API server reports about capacity — CPU, memory, ephemeral
//! storage, extended resources — arrives as a string in one small grammar:
//!
//! ```text
//! <quantity>        ::= <signedNumber><suffix>
//! <suffix>          ::= <binarySI> | <decimalExponent> | <decimalSI>
//! <binarySI>        ::= Ki | Mi | Gi | Ti | Pi | Ei
//! <decimalSI>       ::= n | u | m | "" | k | M | G | T | P | E
//! <decimalExponent> ::= ("e" | "E") <signedNumber>
//! ```
//!
//! `k8s-openapi` models it as a newtype over `String` and stops there, so the
//! arithmetic is ours to do. It is worth doing carefully and exactly once: this
//! is the foundation the capacity columns, utilisation percentages, and
//! eventually metrics-server all stand on, and a parser that quietly reads
//! `2Gi` as 2 is a wrong answer rather than an error message.
//!
//! Everything here is a pure function over a string, which is why the awkward
//! cases below — `1e3`, a capital `K` that the grammar does not actually
//! allow, an empty string, a value too large for any machine — are tests.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;

/// A quantity we could not make sense of.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The string does not match the grammar at all.
    #[error(
        "{0:?} is not a Kubernetes quantity.\n\
         Expected a number with an optional unit, like \"100m\", \"1.5\", \"2Gi\", or \"1e3\"."
    )]
    Malformed(String),

    /// The string is well-formed but names a number no machine will ever hold.
    #[error("{0:?} is larger than this tool can represent; is the value correct?")]
    TooLarge(String),
}

/// A parsed resource quantity.
///
/// Held internally as thousandths of a unit — millicores for CPU, thousandths
/// of a byte for memory — as an `i128`. Integer thousandths rather than a float
/// because a millicore is the smallest unit anyone schedules against and a
/// quantity that survives a round trip unchanged is much easier to reason
/// about; `i128` because thousandths of an exbibyte overflow an `i64`.
///
/// Values finer than one thousandth of a unit (a `1n` extended resource, say)
/// are rounded to the nearest thousandth. Nothing this tool displays is
/// measured that finely, and carrying arbitrary precision to avoid a rounding
/// nobody can see is not worth the machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Quantity {
    thousandths: i128,
}

impl Quantity {
    /// Parse a quantity as the API server writes it.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let (digits, fraction_digits, suffix) =
            split(text).ok_or_else(|| Error::Malformed(text.to_owned()))?;
        let scale = Scale::of(suffix).ok_or_else(|| Error::Malformed(text.to_owned()))?;
        // Every character in `digits` is a digit or a leading sign, so the only
        // way this fails is a number with more digits than an i128 holds.
        let mantissa: i128 = digits
            .parse()
            .map_err(|_| Error::TooLarge(text.to_owned()))?;

        // Saturating rather than checked: `1e9223372036854775807` parses as a
        // perfectly good i64 exponent and then overflows the shift to
        // thousandths, which in a debug build is a panic. Saturating leaves the
        // exponent far outside `pow10`'s range, which is exactly right — it is
        // either too large or rounds to nothing.
        let shift = |exponent: i64| {
            exponent
                .saturating_add(3)
                .saturating_sub(i64::from(fraction_digits))
        };

        let thousandths = match scale {
            Scale::Decimal(exponent) => pow10(mantissa, shift(exponent), text)?,
            // 2^bits is exact and bits is at most 60, so the shift cannot lose
            // anything; only the multiply can overflow.
            Scale::Binary(bits) => {
                let shifted = mantissa
                    .checked_mul(1_i128 << bits)
                    .ok_or_else(|| Error::TooLarge(text.to_owned()))?;
                pow10(shifted, shift(0), text)?
            }
        };

        Ok(Self { thousandths })
    }

    /// Parse the `k8s-openapi` newtype, which is a `String` in a coat.
    pub fn from_api(quantity: &ApiQuantity) -> Result<Self, Error> {
        Self::parse(&quantity.0)
    }

    /// Look one resource up in a `capacity` or `allocatable` map.
    ///
    /// `None` covers both "the node did not report this resource" and "it
    /// reported something we cannot parse". A capacity column is not the place
    /// to fail a whole listing over one odd extended resource, so the raw value
    /// goes to `tracing::debug` and the cell reads as unknown.
    #[must_use]
    pub fn lookup(map: Option<&BTreeMap<String, ApiQuantity>>, resource: &str) -> Option<Self> {
        let raw = map?.get(resource)?;
        match Self::from_api(raw) {
            Ok(quantity) => Some(quantity),
            Err(error) => {
                tracing::debug!(%error, resource, "ignoring unparseable quantity");
                None
            }
        }
    }

    /// Thousandths of a unit: millicores for CPU, thousandths of a byte for
    /// memory.
    #[must_use]
    pub const fn thousandths(self) -> i128 {
        self.thousandths
    }

    /// Whole units, rounded towards zero. Bytes, for memory.
    #[must_use]
    pub const fn units(self) -> i128 {
        self.thousandths / 1000
    }

    /// The value as a float, for ratios and bar widths.
    ///
    /// Lossy above 2^53 thousandths — around nine petabytes — which is far past
    /// anything a single node reports, and only ever feeds a percentage.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(self) -> f64 {
        self.thousandths as f64 / 1000.0
    }

    /// `self / total`, or `None` when `total` is zero or negative.
    ///
    /// Returning `None` rather than an infinity keeps every caller from having
    /// to remember that a node reporting zero capacity is a real thing that
    /// happens while a node registers.
    #[must_use]
    pub fn ratio_of(self, total: Self) -> Option<f64> {
        if total.thousandths <= 0 {
            return None;
        }
        Some(self.as_f64() / total.as_f64())
    }
}

impl std::ops::Add for Quantity {
    type Output = Self;

    /// Saturating rather than checked or wrapping.
    ///
    /// Summing the pod requests on one node cannot get near an `i128` unless
    /// something upstream is already nonsense, and a total that pins at the
    /// maximum is a visibly absurd number the user can act on. Wrapping would
    /// turn it into a small, plausible lie.
    fn add(self, other: Self) -> Self {
        Self {
            thousandths: self.thousandths.saturating_add(other.thousandths),
        }
    }
}

impl std::iter::Sum for Quantity {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), std::ops::Add::add)
    }
}

/// Split a quantity into its mantissa digits (with the decimal point removed),
/// the number of digits that followed that point, and the suffix. Returns
/// `None` for anything that is not a number followed by a suffix.
fn split(text: &str) -> Option<(String, u32, &str)> {
    let bytes = text.as_bytes();
    let mut end = 0;

    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end += 1;
    }

    let integer_start = end;
    while matches!(bytes.get(end), Some(byte) if byte.is_ascii_digit()) {
        end += 1;
    }
    let integer_digits = end - integer_start;

    let mut fraction_digits = 0;
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        let fraction_start = end;
        while matches!(bytes.get(end), Some(byte) if byte.is_ascii_digit()) {
            end += 1;
        }
        fraction_digits = end - fraction_start;
    }

    if integer_digits == 0 && fraction_digits == 0 {
        return None;
    }

    // Deleting the decimal point turns "1.5" into the integer 15, which is the
    // mantissa; `fraction_digits` records how far to shift it back.
    let mut digits = String::with_capacity(end);
    // "2." and ".5" are both legal; with the point gone they are just "2" and
    // "5", and the guard above guarantees at least one digit survives.
    digits.extend(text[..end].chars().filter(|c| *c != '.'));

    Some((digits, u32::try_from(fraction_digits).ok()?, &text[end..]))
}

/// The multiplier a suffix stands for.
#[derive(Debug, Clone, Copy)]
enum Scale {
    /// A power of ten.
    Decimal(i64),
    /// A power of two, as a bit count.
    Binary(u32),
}

impl Scale {
    fn of(suffix: &str) -> Option<Self> {
        // Binary first: "Ei" and "E" differ only by the trailing `i`, and "E"
        // alone is exa rather than the start of an exponent.
        let binary = match suffix {
            "Ki" => Some(10),
            "Mi" => Some(20),
            "Gi" => Some(30),
            "Ti" => Some(40),
            "Pi" => Some(50),
            "Ei" => Some(60),
            _ => None,
        };
        if let Some(bits) = binary {
            return Some(Self::Binary(bits));
        }

        // Note the absence of a capital "K": the grammar really does only
        // accept the lowercase one, and so does `kubectl`.
        let decimal = match suffix {
            "n" => Some(-9),
            "u" => Some(-6),
            "m" => Some(-3),
            "" => Some(0),
            "k" => Some(3),
            "M" => Some(6),
            "G" => Some(9),
            "T" => Some(12),
            "P" => Some(15),
            "E" => Some(18),
            _ => None,
        };
        if let Some(exponent) = decimal {
            return Some(Self::Decimal(exponent));
        }

        let rest = suffix.strip_prefix(['e', 'E'])?;
        rest.parse::<i64>().ok().map(Self::Decimal)
    }
}

/// `value * 10^exponent`, rounding to the nearest integer when the exponent is
/// negative.
fn pow10(value: i128, exponent: i64, raw: &str) -> Result<i128, Error> {
    // 10^38 is the largest power of ten an i128 holds, so anything beyond it
    // either overflows or — for a negative exponent — rounds away to nothing.
    const LIMIT: u32 = 38;

    let too_large = || Error::TooLarge(raw.to_owned());

    if exponent >= 0 {
        let exponent = u32::try_from(exponent).map_err(|_| too_large())?;
        if value == 0 {
            return Ok(0);
        }
        if exponent > LIMIT {
            return Err(too_large());
        }
        10_i128
            .checked_pow(exponent)
            .and_then(|factor| value.checked_mul(factor))
            .ok_or_else(too_large)
    } else {
        let exponent = exponent.unsigned_abs();
        let Ok(exponent) = u32::try_from(exponent) else {
            return Ok(0);
        };
        if exponent > LIMIT {
            return Ok(0);
        }
        Ok(round_div(value, 10_i128.pow(exponent)))
    }
}

/// Divide, rounding halves away from zero.
fn round_div(value: i128, divisor: i128) -> i128 {
    let half = divisor / 2;
    if value >= 0 {
        value.saturating_add(half) / divisor
    } else {
        value.saturating_sub(half) / divisor
    }
}

/// CPU, the way `kubectl` writes it: whole cores when it divides evenly,
/// millicores otherwise.
#[must_use]
pub fn cpu(quantity: Quantity) -> String {
    let millis = quantity.thousandths();
    if millis % 1000 == 0 {
        format!("{}", millis / 1000)
    } else {
        format!("{millis}m")
    }
}

/// A count of whole things: GPUs, dongles, licences.
///
/// Extended resources are integers by definition — a device plugin cannot
/// advertise half a GPU and the scheduler will not hand one out — so this is
/// [`cpu`]'s spelling under a name that says what is being counted. The
/// fractional fallback is deliberate rather than rounded away: a cluster that
/// somehow advertises `500m` of a device has a bug worth seeing, and a column
/// reading `0` would hide it.
#[must_use]
pub fn count(quantity: Quantity) -> String {
    cpu(quantity)
}

/// Memory, in the largest binary unit that leaves a number a person can read.
///
/// One decimal place, and the `.0` trimmed: `15.6Gi`, `512Mi`, `4Gi`. This is
/// deliberately *not* `kubectl`'s output — it reports allocatable memory as
/// `7134420Ki`, which is precise and completely unreadable at a glance, and the
/// whole point of a capacity column is the glance.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn memory(quantity: Quantity) -> String {
    const UNITS: [(&str, u32); 6] = [
        ("Ei", 60),
        ("Pi", 50),
        ("Ti", 40),
        ("Gi", 30),
        ("Mi", 20),
        ("Ki", 10),
    ];

    let bytes = quantity.units();
    let sign = if bytes < 0 { "-" } else { "" };
    let magnitude = bytes.unsigned_abs();

    for (suffix, bits) in UNITS {
        let unit = 1_u128 << bits;
        if magnitude >= unit {
            let scaled = magnitude as f64 / unit as f64;
            let text = format!("{scaled:.1}");
            return format!("{sign}{}{suffix}", text.trim_end_matches(".0"));
        }
    }

    format!("{bytes}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Thousandths, which is what `Quantity` stores.
    fn parse(text: &str) -> i128 {
        Quantity::parse(text).unwrap().thousandths()
    }

    #[test]
    fn a_count_of_devices_reads_as_a_whole_number() {
        assert_eq!(count(Quantity::parse("4").unwrap()), "4");
        assert_eq!(count(Quantity::parse("0").unwrap()), "0");
        // A cluster advertising a fraction of a device has a bug worth seeing;
        // a column reading `0` would hide it.
        assert_eq!(count(Quantity::parse("500m").unwrap()), "500m");
    }

    #[test]
    fn a_bare_number_is_that_many_units() {
        assert_eq!(parse("0"), 0);
        assert_eq!(parse("1"), 1_000);
        assert_eq!(parse("64"), 64_000);
    }

    #[test]
    fn decimals_and_signs_are_carried_exactly() {
        assert_eq!(parse("1.5"), 1_500);
        assert_eq!(parse("0.25"), 250);
        assert_eq!(parse(".5"), 500);
        assert_eq!(parse("2."), 2_000);
        assert_eq!(parse("+3"), 3_000);
        // Negative quantities are not something a node reports, but the grammar
        // allows them and silently flipping a sign would be worse than showing
        // one.
        assert_eq!(parse("-1.5"), -1_500);
    }

    #[test]
    fn every_decimal_si_suffix_scales_by_a_power_of_ten() {
        assert_eq!(parse("1n"), 0); // a billionth of a unit rounds away
        assert_eq!(parse("1u"), 0);
        assert_eq!(parse("1000u"), 1);
        assert_eq!(parse("1500u"), 2); // 1.5 thousandths, rounded to nearest
        assert_eq!(parse("100m"), 100);
        assert_eq!(parse("1k"), 1_000_000);
        assert_eq!(parse("1M"), 1_000_000_000);
        assert_eq!(parse("1G"), 1_000_000_000_000);
        assert_eq!(parse("1T"), 1_000_000_000_000_000);
        assert_eq!(parse("1P"), 1_000_000_000_000_000_000);
        assert_eq!(parse("1E"), 1_000_000_000_000_000_000_000);
    }

    #[test]
    fn every_binary_si_suffix_scales_by_a_power_of_two() {
        for (text, bytes) in [
            ("1Ki", 1_u128 << 10),
            ("1Mi", 1 << 20),
            ("1Gi", 1 << 30),
            ("1Ti", 1 << 40),
            ("1Pi", 1 << 50),
            ("1Ei", 1 << 60),
        ] {
            assert_eq!(
                Quantity::parse(text).unwrap().units(),
                i128::try_from(bytes).unwrap(),
                "{text}"
            );
        }

        assert_eq!(Quantity::parse("2Gi").unwrap().units(), 2_147_483_648);
        // A fraction of a binary unit still lands on a whole byte.
        assert_eq!(Quantity::parse("1.5Gi").unwrap().units(), 1_610_612_736);
    }

    #[test]
    fn an_exponent_suffix_is_accepted_in_both_cases() {
        assert_eq!(parse("1e3"), 1_000_000);
        assert_eq!(parse("1E3"), 1_000_000);
        assert_eq!(parse("1.5e3"), 1_500_000);
        assert_eq!(parse("2e-3"), 2);
        assert_eq!(parse("5e+2"), 500_000);
        // A bare `E` is exa, not the start of an exponent — the one place the
        // two halves of the grammar overlap.
        assert_ne!(parse("1E"), parse("1e0"));
    }

    #[test]
    fn garbage_is_rejected_with_an_example_of_what_was_wanted() {
        for text in [
            "", " ", "abc", "1.2.3", "-", "+", ".", "1 Gi", "1Gi ", "1Kb", "1Gib", "1e", "1e1.5",
            "Gi", "1x", "0x10", "1,5", "NaN", "inf",
        ] {
            let error = Quantity::parse(text).unwrap_err();
            assert!(
                matches!(error, Error::Malformed(_)),
                "{text:?} should be malformed, got {error:?}"
            );
        }

        // A capital K is the classic near-miss; Kubernetes does not accept it
        // either, so neither do we.
        let error = Quantity::parse("1K").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("\"1K\""), "{message}");
        assert!(message.contains("2Gi"), "{message}");
    }

    #[test]
    fn a_number_too_large_to_hold_says_so_rather_than_wrapping() {
        // Fits in the mantissa, overflows once the suffix is applied.
        let error = Quantity::parse("1000000000000000000000Ei").unwrap_err();
        assert!(matches!(error, Error::TooLarge(_)), "{error:?}");

        // More digits than an i128 holds at all: still too large, not garbage.
        let error = Quantity::parse("999999999999999999999999999999999999999").unwrap_err();
        assert!(matches!(error, Error::TooLarge(_)), "{error:?}");

        let error = Quantity::parse("1e300").unwrap_err();
        assert!(matches!(error, Error::TooLarge(_)), "{error:?}");

        // An exponent at the edge of what an i64 holds must not overflow the
        // shift into thousandths on its way to being rejected.
        let error = Quantity::parse("1e9223372036854775807").unwrap_err();
        assert!(matches!(error, Error::TooLarge(_)), "{error:?}");
        assert_eq!(parse("1e-9223372036854775808"), 0);

        // Zero is representable at any scale, however silly the exponent.
        assert_eq!(parse("0e300"), 0);
        assert_eq!(parse("1e-300"), 0);
    }

    #[test]
    fn ratios_refuse_to_divide_by_a_node_reporting_nothing() {
        let used = Quantity::parse("1").unwrap();
        let total = Quantity::parse("4").unwrap();

        assert_eq!(used.ratio_of(total), Some(0.25));
        assert_eq!(used.ratio_of(Quantity::default()), None);
        assert_eq!(used.ratio_of(Quantity::parse("-1").unwrap()), None);
    }

    #[test]
    fn quantities_add_and_sum_without_ever_wrapping() {
        let hundred = Quantity::parse("100m").unwrap();
        assert_eq!(hundred + hundred, Quantity::parse("200m").unwrap());
        assert_eq!(
            [hundred, hundred, hundred].into_iter().sum::<Quantity>(),
            Quantity::parse("300m").unwrap()
        );
        assert_eq!(
            std::iter::empty::<Quantity>().sum::<Quantity>(),
            Quantity::default()
        );

        // Absurd, but it must saturate rather than wrap into a small number: a
        // total that reads as impossibly large is a bug someone can see, and a
        // wrapped one is a quiet lie. This is within a factor of two of the
        // largest quantity that can be represented at all.
        let huge = Quantity::parse("100000000000000000E").unwrap();
        assert_eq!((huge + huge).thousandths(), i128::MAX);
    }

    #[test]
    fn looking_up_a_resource_tolerates_a_missing_or_broken_entry() {
        let mut map = BTreeMap::new();
        map.insert("cpu".to_owned(), ApiQuantity("4".to_owned()));
        map.insert("memory".to_owned(), ApiQuantity("not-a-number".to_owned()));

        assert_eq!(
            Quantity::lookup(Some(&map), "cpu"),
            Some(Quantity::parse("4").unwrap())
        );
        // Present but nonsense, absent, and no map at all all read the same:
        // one odd resource must not fail a whole listing.
        assert_eq!(Quantity::lookup(Some(&map), "memory"), None);
        assert_eq!(Quantity::lookup(Some(&map), "nvidia.com/gpu"), None);
        assert_eq!(Quantity::lookup(None, "cpu"), None);
    }

    #[test]
    fn cpu_reads_as_cores_when_it_divides_evenly_and_millicores_otherwise() {
        let show = |text: &str| cpu(Quantity::parse(text).unwrap());

        assert_eq!(show("4"), "4");
        assert_eq!(show("4000m"), "4");
        assert_eq!(show("3920m"), "3920m");
        assert_eq!(show("0.5"), "500m");
        assert_eq!(show("0"), "0");
    }

    #[test]
    fn memory_reads_in_the_largest_binary_unit_that_stays_legible() {
        let show = |text: &str| memory(Quantity::parse(text).unwrap());

        assert_eq!(show("0"), "0");
        assert_eq!(show("512"), "512");
        assert_eq!(show("512Mi"), "512Mi");
        assert_eq!(show("4Gi"), "4Gi");
        // What an m5.large actually reports as allocatable.
        assert_eq!(show("7134420Ki"), "6.8Gi");
        assert_eq!(show("1Ti"), "1Ti");
        assert_eq!(show("-1Gi"), "-1Gi");
    }

    #[test]
    fn memory_in_decimal_units_still_prints_in_binary_ones() {
        // Nothing stops a device plugin reporting `1G`; a column mixing binary
        // and decimal units would be unreadable, so everything is shown one way.
        assert_eq!(memory(Quantity::parse("1G").unwrap()), "953.7Mi");
    }
}
