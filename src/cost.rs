//! Cost computation from admin-supplied prices (FR-6.3).
//!
//! No vendor prices are bundled. Models with no price configured produce
//! `None` (unknown), never `0.0`.

use crate::types::{Prices, TokenUsage};

/// Compute USD cost for a request. Returns `None` when prices are not configured
/// or canonical totals are incomplete.
/// Format a USD amount for human-facing messages, keeping precision for tiny
/// amounts so a budget of $0.0001 does not read as "0.00".
pub fn format_usd(v: f64) -> String {
    if v != 0.0 && v.abs() < 0.01 {
        format!("{v:.6}")
    } else {
        format!("{v:.2}")
    }
}

pub fn compute_cost(prices: &Prices, usage: &TokenUsage) -> Option<f64> {
    if !prices.is_configured() {
        return None;
    }
    let (Some(input), Some(output)) = (usage.input, usage.output) else {
        return None;
    };
    let input = input as f64;
    let output = output as f64;

    // Canonical input/output are inclusive totals. Partition reported
    // breakdowns within those totals so malformed overlapping detail counters
    // can never charge more tokens than the provider-reported total.
    let cached = (usage.cached.unwrap_or(0) as f64).min(input);
    let cache_write = (usage.cache_write.unwrap_or(0) as f64).min(input - cached);
    let regular_input = input - cached - cache_write;
    let thinking = (usage.thinking.unwrap_or(0) as f64).min(output);
    let regular_output = output - thinking;

    let input_price = match prices.input_per_1m {
        Some(price) => price,
        None if input == 0.0 => 0.0,
        None => return None,
    };
    let cached_price = prices.cached_per_1m.unwrap_or(input_price);
    let cache_write_price = prices.cache_write_per_1m.unwrap_or(input_price);
    let output_price = match prices.output_per_1m {
        Some(price) => price,
        None if output == 0.0 => 0.0,
        None => return None,
    };
    let thinking_price = prices.thinking_per_1m.unwrap_or(output_price);

    let cost = (regular_input * input_price
        + cached * cached_price
        + cache_write * cache_write_price
        + regular_output * output_price
        + thinking * thinking_price)
        / 1_000_000.0;

    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_when_unpriced() {
        let p = Prices::default();
        let u = TokenUsage {
            input: Some(100),
            output: Some(50),
            ..Default::default()
        };
        assert!(compute_cost(&p, &u).is_none());
    }

    #[test]
    fn incomplete_canonical_totals_are_unknown() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            ..Default::default()
        };
        let input_only = TokenUsage {
            input: Some(100),
            output: None,
            ..Default::default()
        };
        let output_only = TokenUsage {
            input: None,
            output: Some(50),
            ..Default::default()
        };

        assert!(compute_cost(&p, &input_only).is_none());
        assert!(compute_cost(&p, &output_only).is_none());
    }

    #[test]
    fn input_known_output_unknown_is_unknown_when_output_is_consumed() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: None,
            ..Default::default()
        };
        let u = TokenUsage {
            input: Some(100),
            output: Some(50),
            ..Default::default()
        };
        assert!(compute_cost(&p, &u).is_none());
    }

    #[test]
    fn output_known_input_unknown_is_unknown_when_input_is_consumed() {
        let p = Prices {
            input_per_1m: None,
            output_per_1m: Some(2.0),
            ..Default::default()
        };
        let u = TokenUsage {
            input: Some(100),
            output: Some(50),
            ..Default::default()
        };
        assert!(compute_cost(&p, &u).is_none());
    }

    #[test]
    fn prices_inclusive_totals_without_double_charging_breakdowns() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            cached_per_1m: Some(0.1),
            cache_write_per_1m: Some(1.25),
            thinking_per_1m: Some(3.0),
        };
        let u = TokenUsage {
            input: Some(1_000_000),
            output: Some(1_000_000),
            cached: Some(200_000),
            cache_write: Some(100_000),
            thinking: Some(250_000),
        };
        // 700k*1 + 200k*0.1 + 100k*1.25 + 750k*2 + 250k*3 = 3.095
        let cost = compute_cost(&p, &u).unwrap();
        assert!((cost - 3.095).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn overlapping_breakdowns_never_exceed_inclusive_totals() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            cached_per_1m: Some(0.5),
            cache_write_per_1m: Some(3.0),
            thinking_per_1m: Some(4.0),
        };
        let u = TokenUsage {
            input: Some(100),
            output: Some(50),
            cached: Some(80),
            cache_write: Some(80),
            thinking: Some(100),
        };
        // 80 cache-read + 20 cache-write = exactly 100 input tokens.
        // Thinking is clamped to the 50-token inclusive output total.
        let expected = (80.0 * 0.5 + 20.0 * 3.0 + 50.0 * 4.0) / 1_000_000.0;
        assert!((compute_cost(&p, &u).unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn openai_reasoning_breakdown_is_not_added_to_completion_total() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            cached_per_1m: None,
            cache_write_per_1m: None,
            thinking_per_1m: None,
        };
        let u = TokenUsage {
            input: Some(0),
            output: Some(1_000_000),
            thinking: Some(250_000),
            ..Default::default()
        };
        assert!((compute_cost(&p, &u).unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn formats_tiny_amounts_with_precision() {
        assert_eq!(format_usd(0.0001), "0.000100");
        assert_eq!(format_usd(2.5), "2.50");
        assert_eq!(format_usd(0.0), "0.00");
    }
}
