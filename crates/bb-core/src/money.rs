//! Decimal money helpers. All prices, quantities, balances and fees are
//! [`Decimal`]; `f64` is reserved for statistics (returns, volatility, scores).

use rust_decimal::{Decimal, RoundingStrategy};

pub const BPS_PER_UNIT: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// `value * bps / 10_000`.
pub fn apply_bps(value: Decimal, bps: Decimal) -> Decimal {
    value * bps / BPS_PER_UNIT
}

/// `(to - from) / from` in basis points; `None` when `from` is zero.
pub fn change_bps(from: Decimal, to: Decimal) -> Option<Decimal> {
    if from.is_zero() {
        None
    } else {
        Some((to - from) / from * BPS_PER_UNIT)
    }
}

/// Round `value` down to a multiple of `step` (exchange LOT_SIZE / tick rules).
/// A non-positive step returns `value` unchanged.
pub fn floor_to_step(value: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return value;
    }
    let steps = (value / step).round_dp_with_strategy(0, RoundingStrategy::ToZero);
    (steps * step).normalize()
}

/// Round `value` up to a multiple of `step`.
pub fn ceil_to_step(value: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return value;
    }
    let steps = (value / step).round_dp_with_strategy(0, RoundingStrategy::AwayFromZero);
    (steps * step).normalize()
}

/// Lossy conversion for statistics only.
pub fn to_f64(value: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    value.to_f64().unwrap_or(f64::NAN)
}

/// Lossy conversion from statistics back into money space; `None` for NaN/inf.
pub fn from_f64(value: f64) -> Option<Decimal> {
    use rust_decimal::prelude::FromPrimitive;
    Decimal::from_f64(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn step_rounding_never_exceeds_input_on_floor() {
        assert_eq!(floor_to_step(dec!(1.23456), dec!(0.001)), dec!(1.234));
        assert_eq!(floor_to_step(dec!(0.0009), dec!(0.001)), dec!(0));
        assert_eq!(ceil_to_step(dec!(1.2341), dec!(0.001)), dec!(1.235));
        assert_eq!(floor_to_step(dec!(5), dec!(0)), dec!(5));
    }

    #[test]
    fn bps_math() {
        assert_eq!(apply_bps(dec!(200), dec!(25)), dec!(0.5));
        assert_eq!(change_bps(dec!(100), dec!(101)), Some(dec!(100)));
        assert_eq!(change_bps(dec!(0), dec!(1)), None);
    }
}
