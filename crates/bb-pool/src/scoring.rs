//! Paper-only candidate scorers for modular pool evaluation (f64 statistics).

use crate::error::{PoolError, Result};

/// Benchmark rate history for one asset, in benchmark order: `(benchmark, oldest..newest)`.
pub type History = [(String, Vec<f64>)];

/// `(benchmark, weight)` pairs in configured order.
pub type Weights = [(String, f64)];

/// One benchmark-basket-relative candidate score.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetScore {
    pub asset: String,
    pub score: f64,
}

/// Configured paper scorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scorer {
    /// Weighted benchmark momentum.
    SimpleMomentum,
    /// Momentum minus realized benchmark-relative volatility (population stdev).
    RiskAdjustedMomentum,
}

impl Scorer {
    /// Build one configured scorer by name.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "simple_momentum" => Ok(Self::SimpleMomentum),
            "risk_adjusted_momentum" => Ok(Self::RiskAdjustedMomentum),
            other => Err(PoolError::Config(format!(
                "unsupported paper scorer: {other}"
            ))),
        }
    }

    /// Scorer name as persisted in the ledger.
    pub fn name(self) -> &'static str {
        match self {
            Self::SimpleMomentum => "simple_momentum",
            Self::RiskAdjustedMomentum => "risk_adjusted_momentum",
        }
    }

    /// Score `asset`; `None` when any weighted benchmark lacks a valid history.
    pub fn score(self, asset: &str, history: &History, weights: &Weights) -> Option<AssetScore> {
        let returns = benchmark_returns(history, weights)?;
        let momentum: f64 = returns
            .iter()
            .zip(weights)
            .map(|(ret, (_, weight))| ret * weight)
            .sum();
        let score = match self {
            Self::SimpleMomentum => momentum,
            Self::RiskAdjustedMomentum => {
                let mut period_returns = Vec::new();
                for (benchmark, values) in history {
                    if !weights.iter().any(|(name, _)| name == benchmark) {
                        continue;
                    }
                    for pair in values.windows(2) {
                        if pair[0] > 0.0 {
                            period_returns.push(pair[1] / pair[0] - 1.0);
                        }
                    }
                }
                let volatility = if period_returns.len() > 1 {
                    pstdev(&period_returns)
                } else {
                    0.0
                };
                momentum - volatility
            }
        };
        Some(AssetScore {
            asset: asset.to_owned(),
            score,
        })
    }
}

/// Weighted return of one portfolio's benchmark totals against its initial totals.
pub fn basket_performance(current: &Weights, initial: &Weights, weights: &Weights) -> Option<f64> {
    let lookup =
        |values: &Weights, name: &str| values.iter().find(|(key, _)| key == name).map(|(_, v)| *v);
    if initial.is_empty()
        || weights
            .iter()
            .any(|(name, _)| lookup(initial, name).unwrap_or(0.0) <= 0.0)
    {
        return None;
    }
    let mut total = 0.0;
    for (benchmark, weight) in weights {
        let latest = lookup(current, benchmark).filter(|v| v.is_finite())?;
        total += (latest / lookup(initial, benchmark)? - 1.0) * weight;
    }
    Some(total)
}

fn benchmark_returns(history: &History, weights: &Weights) -> Option<Vec<f64>> {
    weights
        .iter()
        .map(|(benchmark, _)| {
            let values = history
                .iter()
                .find(|(name, _)| name == benchmark)
                .map_or(&[][..], |(_, values)| values.as_slice());
            let (first, last) = (values.first()?, values.last()?);
            if values.len() < 2
                || *first <= 0.0
                || !values.iter().all(|v| v.is_finite() && *v > 0.0)
            {
                return None;
            }
            Some(last / first - 1.0)
        })
        .collect()
}

/// Population standard deviation.
#[allow(clippy::cast_precision_loss)]
fn pstdev(values: &[f64]) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    variance.sqrt()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn pairs(values: &[(&str, f64)]) -> Vec<(String, f64)> {
        values.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn momentum_and_risk_adjusted_scores() {
        let weights = pairs(&[("USDT", 0.5), ("BTC", 0.5)]);
        let history = vec![
            ("USDT".to_owned(), vec![1.0, 1.1, 1.2]),
            ("BTC".to_owned(), vec![2.0, 2.0, 2.0]),
        ];
        let simple = Scorer::SimpleMomentum
            .score("X", &history, &weights)
            .unwrap();
        assert!((simple.score - 0.1).abs() < 1e-12);
        let risk = Scorer::RiskAdjustedMomentum
            .score("X", &history, &weights)
            .unwrap();
        assert!(risk.score < simple.score);
        let short = vec![
            ("USDT".to_owned(), vec![1.0]),
            ("BTC".to_owned(), vec![2.0, 2.0]),
        ];
        assert!(
            Scorer::SimpleMomentum
                .score("X", &short, &weights)
                .is_none()
        );
        assert!(Scorer::from_name("magic").is_err());
    }

    #[test]
    fn basket_performance_requires_positive_initial_totals() {
        let weights = pairs(&[("USDT", 0.5), ("BTC", 0.5)]);
        let initial = pairs(&[("USDT", 100.0), ("BTC", 0.002)]);
        let current = pairs(&[("USDT", 110.0), ("BTC", 0.002)]);
        assert!((basket_performance(&current, &initial, &weights).unwrap() - 0.05).abs() < 1e-12);
        assert!(basket_performance(&current, &[], &weights).is_none());
        assert!(
            basket_performance(&current, &pairs(&[("USDT", 100.0), ("BTC", 0.0)]), &weights)
                .is_none()
        );
    }
}
