//! Derives per-token pricing from the live Copilot `/models` catalog.
//!
//! The Copilot API returns `billing.token_prices` in AIC (Artificial
//! Intelligence Credits) per `batch_size` tokens. OpenCode converts these
//! to USD per million tokens with:
//!
//!   usd_per_million = 10_000 / batch_size
//!   cost_per_mtok   = aic_price * usd_per_million
//!
//! We store costs as microdollars (µ$) per million tokens, consistent with
//! the rest of jcode's pricing infrastructure.

use crate::{RouteCheapnessEstimate, RouteCostConfidence, RouteCostSource};

/// Raw token prices from a single tier in the Copilot `/models` catalog.
///
/// All `*_price` fields are AIC (integer). `batch_size` is the number of
/// tokens the AIC prices apply to (typically 1 000 000).
#[derive(Debug, Clone, Default)]
pub struct CopilotTokenPricesTier {
    pub input_price: u64,
    pub output_price: u64,
    pub cache_price: u64,
}

/// Billing data extracted from a single model's catalog entry.
#[derive(Debug, Clone)]
pub struct CopilotCatalogBilling {
    pub batch_size: u64,
    pub default: CopilotTokenPricesTier,
}

/// Convert AIC-per-batch prices into a `RouteCheapnessEstimate` with
/// microdollars-per-million-token fields.
///
/// Returns `None` when `batch_size` is zero (avoids division by zero) or
/// when billing data is absent (caller passes `None`).
pub fn estimate_from_catalog(
    billing: Option<&CopilotCatalogBilling>,
) -> Option<RouteCheapnessEstimate> {
    let billing = billing?;
    if billing.batch_size == 0 {
        return None;
    }

    // OpenCode formula: usd_per_million = 10_000 / batch_size
    // Then: cost_usd_per_mtok = aic_price * usd_per_million
    // We store µ$ per Mtok, so multiply by 1_000_000.
    //
    // Combined: micros_per_mtok = aic_price * 10_000 * 1_000_000 / batch_size
    //                           = aic_price * 10_000_000_000 / batch_size
    let to_micros = |aic: u64| -> u64 {
        // Use u128 to avoid overflow with large AIC values.
        ((aic as u128) * 10_000_000_000u128 / (billing.batch_size as u128)) as u64
    };

    let input = to_micros(billing.default.input_price);
    let output = to_micros(billing.default.output_price);
    let cache_read = to_micros(billing.default.cache_price);

    Some(RouteCheapnessEstimate::metered(
        RouteCostSource::CopilotCatalog,
        RouteCostConfidence::High,
        input,
        output,
        Some(cache_read),
        Some("Copilot live catalog per-token pricing".to_string()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RouteBillingKind;

    #[test]
    fn claude_sonnet_4_6_matches_opencode_conversion() {
        // Real catalog data for claude-sonnet-4.6:
        //   batch_size=1_000_000, input=300 AIC, output=1500 AIC, cache=30 AIC
        // OpenCode: usd_per_million = 10_000 / 1_000_000 = 0.01
        //   input  = 300 * 0.01  = $3/Mtok   = 3_000_000 µ$/Mtok
        //   output = 1500 * 0.01 = $15/Mtok  = 15_000_000 µ$/Mtok
        //   cache  = 30 * 0.01   = $0.30/Mtok = 300_000 µ$/Mtok
        let billing = CopilotCatalogBilling {
            batch_size: 1_000_000,
            default: CopilotTokenPricesTier {
                input_price: 300,
                output_price: 1500,
                cache_price: 30,
            },
        };
        let est = estimate_from_catalog(Some(&billing)).expect("should produce estimate");
        assert_eq!(est.billing_kind, RouteBillingKind::Metered);
        assert_eq!(est.input_price_per_mtok_micros, Some(3_000_000));
        assert_eq!(est.output_price_per_mtok_micros, Some(15_000_000));
        assert_eq!(est.cache_read_price_per_mtok_micros, Some(300_000));
        assert_eq!(est.source, RouteCostSource::CopilotCatalog);
    }

    #[test]
    fn claude_opus_4_6_from_real_capture() {
        // Real catalog: batch_size=1_000_000, input=500, output=2500, cache=50
        // Expected: $5/Mtok input, $25/Mtok output, $0.50/Mtok cache
        let billing = CopilotCatalogBilling {
            batch_size: 1_000_000,
            default: CopilotTokenPricesTier {
                input_price: 500,
                output_price: 2500,
                cache_price: 50,
            },
        };
        let est = estimate_from_catalog(Some(&billing)).expect("should produce estimate");
        assert_eq!(est.input_price_per_mtok_micros, Some(5_000_000));
        assert_eq!(est.output_price_per_mtok_micros, Some(25_000_000));
        assert_eq!(est.cache_read_price_per_mtok_micros, Some(500_000));
    }

    #[test]
    fn absent_billing_returns_none() {
        // Models without billing data should not produce a pricing estimate,
        // letting the caller fall back to the subscription heuristic.
        assert!(estimate_from_catalog(None).is_none());
    }

    #[test]
    fn zero_batch_size_returns_none() {
        // Guard against division-by-zero from malformed catalog data.
        let billing = CopilotCatalogBilling {
            batch_size: 0,
            default: CopilotTokenPricesTier {
                input_price: 300,
                output_price: 1500,
                cache_price: 30,
            },
        };
        assert!(estimate_from_catalog(Some(&billing)).is_none());
    }

    #[test]
    fn zero_prices_produce_zero_estimate() {
        // Free-tier models should produce a valid metered estimate with zero costs.
        let billing = CopilotCatalogBilling {
            batch_size: 1_000_000,
            default: CopilotTokenPricesTier {
                input_price: 0,
                output_price: 0,
                cache_price: 0,
            },
        };
        let est = estimate_from_catalog(Some(&billing)).expect("should produce estimate");
        assert_eq!(est.input_price_per_mtok_micros, Some(0));
        assert_eq!(est.output_price_per_mtok_micros, Some(0));
        assert_eq!(est.cache_read_price_per_mtok_micros, Some(0));
    }

    #[test]
    fn non_million_batch_size_scales_correctly() {
        // If the API changes batch_size (e.g. to 1000), prices should scale.
        // batch_size=1000: usd_per_million = 10_000/1000 = 10
        // input = 300 * 10 = $3000/Mtok = 3_000_000_000 µ$/Mtok
        let billing = CopilotCatalogBilling {
            batch_size: 1000,
            default: CopilotTokenPricesTier {
                input_price: 300,
                output_price: 1500,
                cache_price: 30,
            },
        };
        let est = estimate_from_catalog(Some(&billing)).expect("should produce estimate");
        assert_eq!(est.input_price_per_mtok_micros, Some(3_000_000_000));
    }
}
