//! Exit logic shared by paper and live round trips.

use std::time::Duration;

use rust_decimal::Decimal;

use super::settings::{HoldMode, SniperSettings};

/// `(elapsed_seconds, price)` observations, entry first.
pub type Samples = [(f64, Decimal)];

/// Momentum exit: any of
/// 1. negative velocity for `momentum_negative_polls_to_exit` consecutive polls,
/// 2. no new high for `momentum_no_new_high_polls_to_exit` polls,
/// 3. short MA below long MA with at least `momentum_ma_cross_min_pullback_pct` pullback from `peak`.
pub fn momentum_should_exit(
    settings: &SniperSettings,
    samples: &Samples,
    peak: Decimal,
) -> Option<String> {
    let n = samples.len();
    if n < 2 {
        return None;
    }

    let k = settings.momentum_negative_polls_to_exit;
    if n > k {
        // Velocity signs over the last k+1 samples, skipping non-advancing timestamps.
        let negative: Vec<bool> = samples[n - k - 1..]
            .windows(2)
            .filter(|w| w[1].0 - w[0].0 > 0.0)
            .map(|w| w[1].1 < w[0].1)
            .collect();
        if negative.len() >= k && negative[negative.len() - k..].iter().all(|neg| *neg) {
            return Some(format!("momentum_negative_for_{k}_polls"));
        }
    }

    // First occurrence of the maximum, as Python's max() over indices.
    let peak_idx = samples.iter().enumerate().fold(
        0,
        |best, (i, s)| if s.1 > samples[best].1 { i } else { best },
    );
    let polls_since_peak = n - 1 - peak_idx;
    if polls_since_peak >= settings.momentum_no_new_high_polls_to_exit {
        return Some(format!("no_new_high_for_{polls_since_peak}_polls"));
    }

    let (short_n, long_n) = (
        settings.momentum_ma_short_samples,
        settings.momentum_ma_long_samples,
    );
    if n >= long_n && short_n > 0 {
        let mean = |w: &Samples| w.iter().map(|s| s.1).sum::<Decimal>() / Decimal::from(w.len());
        let short_ma = mean(&samples[n - short_n..]);
        let long_ma = mean(&samples[n - long_n..]);
        if short_ma < long_ma && peak > Decimal::ZERO {
            let pullback_pct = (peak - samples[n - 1].1) / peak * Decimal::ONE_HUNDRED;
            if pullback_pct >= settings.momentum_ma_cross_min_pullback_pct {
                return Some(format!(
                    "ma_cross_with_pullback: short_ma={:.8} < long_ma={:.8}, pullback_pct={:.2}",
                    short_ma.round_dp(8),
                    long_ma.round_dp(8),
                    pullback_pct.round_dp(2)
                ));
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub struct HoldOutcome {
    /// Last observed price (entry when nothing was observed).
    pub exit_price: Decimal,
    pub exit_reason: String,
    pub elapsed_seconds: f64,
}

/// Unified hold loop: poll every `trailing_poll_interval_seconds`; the hard
/// stop applies from tick 1, mode-specific exits only after the
/// `hold_seconds` floor, and `max_hold_seconds` forces an exit. Polls where
/// `price` yields `None` are skipped.
pub async fn hold<F, Fut>(
    settings: &SniperSettings,
    entry_price: Decimal,
    mut price: F,
) -> HoldOutcome
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Decimal>>,
{
    let mut samples: Vec<(f64, Decimal)> = vec![(0.0, entry_price)];
    let mut peak = entry_price;
    let mut elapsed: u64 = 0;
    let stop_price = if settings.hard_stop_loss_pct < Decimal::ZERO {
        entry_price * (Decimal::ONE + settings.hard_stop_loss_pct / Decimal::ONE_HUNDRED)
    } else {
        Decimal::ZERO
    };
    let trailing_factor = Decimal::ONE - settings.trailing_stop_pct / Decimal::ONE_HUNDRED;
    let interval = settings.trailing_poll_interval_seconds;
    let done = |price: Decimal, reason: String, elapsed: u64| HoldOutcome {
        exit_price: price,
        exit_reason: reason,
        elapsed_seconds: elapsed as f64,
    };

    while elapsed < settings.max_hold_seconds {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        elapsed += interval;
        let Some(last) = price().await else { continue };
        samples.push((elapsed as f64, last));
        peak = peak.max(last);

        if stop_price > Decimal::ZERO && last <= stop_price {
            let reason = format!(
                "hard_stop_loss_hit: last={:.8} <= stop_price={:.8} ({:.1}% from entry)",
                last.round_dp(8),
                stop_price.round_dp(8),
                settings.hard_stop_loss_pct.round_dp(1)
            );
            return done(last, reason, elapsed);
        }
        if elapsed < settings.hold_seconds {
            continue;
        }
        match settings.hold_mode {
            HoldMode::Flat => return done(last, "flat_hold_elapsed".into(), elapsed),
            HoldMode::Trailing => {
                let stop = peak * trailing_factor;
                if last <= stop {
                    let reason = format!(
                        "trailing_stop_hit: last={:.8} <= peak*{:.4}={:.8}",
                        last.round_dp(8),
                        trailing_factor.round_dp(4),
                        stop.round_dp(8)
                    );
                    return done(last, reason, elapsed);
                }
            }
            HoldMode::Momentum => {
                if let Some(reason) = momentum_should_exit(settings, &samples, peak) {
                    return done(last, reason, elapsed);
                }
            }
        }
    }
    let last = samples.last().map_or(entry_price, |s| s.1);
    done(last, "max_hold_seconds_reached".into(), elapsed)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn mom(
        neg: usize,
        no_high: usize,
        short: usize,
        long: usize,
        pullback: Decimal,
    ) -> SniperSettings {
        SniperSettings {
            hold_mode: HoldMode::Momentum,
            momentum_negative_polls_to_exit: neg,
            momentum_no_new_high_polls_to_exit: no_high,
            momentum_ma_short_samples: short,
            momentum_ma_long_samples: long,
            momentum_ma_cross_min_pullback_pct: pullback,
            ..SniperSettings::default()
        }
    }

    fn samples(points: &[(f64, Decimal)]) -> Vec<(f64, Decimal)> {
        points.to_vec()
    }

    #[test]
    fn too_few_samples_never_exit() {
        assert_eq!(
            momentum_should_exit(
                &mom(2, 5, 3, 6, dec!(5)),
                &samples(&[(0.0, dec!(1))]),
                dec!(1)
            ),
            None
        );
    }

    #[test]
    fn consecutive_negative_velocity_exits() {
        let s = samples(&[
            (0.0, dec!(1.0)),
            (10.0, dec!(1.20)),
            (20.0, dec!(1.10)),
            (30.0, dec!(1.05)),
        ]);
        let reason = momentum_should_exit(&mom(2, 5, 3, 6, dec!(5)), &s, dec!(1.20)).unwrap();
        assert!(reason.contains("momentum_negative_for_2_polls"), "{reason}");
    }

    #[test]
    fn single_dip_then_recovery_holds() {
        let s = samples(&[
            (0.0, dec!(1.0)),
            (10.0, dec!(1.20)),
            (20.0, dec!(1.10)),
            (30.0, dec!(1.30)),
        ]);
        assert_eq!(
            momentum_should_exit(&mom(2, 5, 3, 6, dec!(5)), &s, dec!(1.30)),
            None
        );
    }

    #[test]
    fn no_new_high_for_n_polls_exits() {
        let s = samples(&[
            (0.0, dec!(1.0)),
            (10.0, dec!(2.0)),
            (20.0, dec!(1.5)),
            (30.0, dec!(1.6)),
            (40.0, dec!(1.7)),
        ]);
        let reason = momentum_should_exit(&mom(999, 3, 2, 99, dec!(5)), &s, dec!(2.0)).unwrap();
        assert_eq!(reason, "no_new_high_for_3_polls");
    }

    #[test]
    fn ma_cross_requires_pullback() {
        let s = samples(&[
            (0.0, dec!(1.0)),
            (1.0, dec!(1.05)),
            (2.0, dec!(1.10)),
            (3.0, dec!(1.15)),
            (4.0, dec!(1.18)),
            (5.0, dec!(1.16)),
        ]);
        assert_eq!(
            momentum_should_exit(&mom(999, 999, 3, 6, dec!(10)), &s, dec!(1.18)),
            None
        );
    }

    #[test]
    fn ma_cross_fires_with_sufficient_pullback() {
        let s = samples(&[
            (0.0, dec!(1.0)),
            (1.0, dec!(1.10)),
            (2.0, dec!(1.20)),
            (3.0, dec!(1.15)),
            (4.0, dec!(1.05)),
            (5.0, dec!(1.00)),
        ]);
        let reason = momentum_should_exit(&mom(999, 999, 3, 6, dec!(5)), &s, dec!(1.20)).unwrap();
        assert!(reason.starts_with("ma_cross_with_pullback"), "{reason}");
        assert!(reason.ends_with("pullback_pct=16.67"), "{reason}");
    }

    #[test]
    fn non_advancing_timestamps_are_ignored_for_velocity() {
        let s = samples(&[
            (0.0, dec!(1.0)),
            (10.0, dec!(0.9)),
            (10.0, dec!(0.8)),
            (20.0, dec!(0.7)),
        ]);
        assert_eq!(
            momentum_should_exit(&mom(2, 999, 3, 99, dec!(5)), &s, dec!(1.0)),
            None
        );
    }
}
