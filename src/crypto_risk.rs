//! Heuristic suspicious-transaction scorer for on-chain transfers.
//!
//! This is pattern matching, not attribution: a high score means "worth a
//! human look", never "this is criminal". Every reason is labeled and the
//! thresholds are documented below. False positives are expected —
//! exchanges batching withdrawals, for example, trip several heuristics.

/// Whale thresholds per ticker (main units): transfers above this get flagged.
fn whale_threshold(ticker: &str) -> f64 {
    match ticker {
        "BTC" => 50.0,
        "ETH" => 1_000.0,
        "LTC" => 5_000.0,
        "DOGE" => 1_000_000.0,
        _ => f64::INFINITY,
    }
}

/// Dust thresholds per ticker (main units): outputs below this suggest
/// dusting (deanonymization probes).
fn dust_threshold(ticker: &str) -> f64 {
    match ticker {
        "BTC" => 0.00000546, // 546 sats
        "ETH" => 0.000001,
        "LTC" => 0.00000546,
        "DOGE" => 0.01,
        _ => 0.0,
    }
}

/// Round numbers that often appear in ransom/OTC/extortion payments.
const ROUND_STEPS: &[f64] = &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 500.0, 1000.0];

fn is_round_amount(value: f64) -> bool {
    if value <= 0.0 {
        return false;
    }
    ROUND_STEPS.iter().any(|step| {
        let nearest = (value / step).round() * step;
        nearest > 0.0 && (value - nearest).abs() / value < 0.005
    })
}

#[derive(Debug, Clone)]
pub struct CryptoRisk {
    pub score: u8,
    pub level: &'static str,
    pub reasons: Vec<String>,
}

/// Score a transaction. All inputs in the currency's main unit.
pub fn score_tx(
    ticker: &str,
    value: f64,
    fee: f64,
    vin_sz: usize,
    vout_sz: usize,
    min_output: f64,
) -> CryptoRisk {
    let mut score: u32 = 0;
    let mut reasons: Vec<String> = Vec::new();

    // 1. Whale-sized transfer.
    if value >= whale_threshold(ticker) {
        score += 30;
        reasons.push(format!(
            "large transfer ({value:.2} {ticker} ≥ whale threshold)"
        ));
    }

    // 2. Round amount — common in ransom and OTC payments.
    if is_round_amount(value) {
        score += 15;
        reasons.push(format!("round amount ({value:.4} {ticker})"));
    }

    // 3. Dust outputs — deanonymization probe pattern.
    let dust = dust_threshold(ticker);
    if dust > 0.0 && min_output > 0.0 && min_output < dust {
        score += 20;
        reasons.push(format!("dust output ({min_output:.8} {ticker})"));
    }

    // 4. Excessive fee relative to value — urgency or fee games.
    if value > 0.0 && fee / value > 0.02 {
        score += 20;
        reasons.push(format!("high fee ({:.2}% of value)", fee / value * 100.0));
    }

    // 5. Mixing-like structure: many inputs or many outputs.
    if vin_sz >= 10 || vout_sz >= 10 {
        score += 15;
        reasons.push(format!(
            "many inputs/outputs ({vin_sz} in / {vout_sz} out — mixing-like)"
        ));
    } else if vout_sz >= 5 {
        // 6. Moderate fan-out: peel chains, distributions.
        score += 10;
        reasons.push(format!("high output count ({vout_sz} outputs)"));
    }

    let score = score.min(100) as u8;
    let level = match score {
        0..=24 => "low",
        25..=49 => "moderate",
        50..=74 => "high",
        _ => "severe",
    };
    CryptoRisk {
        score,
        level,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whale_btc_scores_high() {
        let r = score_tx("BTC", 100.0, 0.0001, 1, 2, 0.5);
        assert!(r.score >= 30);
        assert!(r.reasons.iter().any(|x| x.contains("large transfer")));
    }

    #[test]
    fn round_amount_flagged() {
        let r = score_tx("ETH", 10.0, 0.001, 1, 2, 5.0);
        assert!(r.reasons.iter().any(|x| x.contains("round amount")));
    }

    #[test]
    fn dust_flagged() {
        let r = score_tx("BTC", 1.5, 0.0001, 1, 3, 0.000001);
        assert!(r.reasons.iter().any(|x| x.contains("dust")));
    }

    #[test]
    fn normal_tx_scores_low() {
        let r = score_tx("BTC", 0.1234, 0.00001, 1, 2, 0.05);
        assert!(r.score < 25);
        assert_eq!(r.level, "low");
    }

    #[test]
    fn mixing_pattern_flagged() {
        let r = score_tx("BTC", 2.5, 0.001, 25, 30, 0.08);
        assert!(r.reasons.iter().any(|x| x.contains("mixing-like")));
    }
}
