use std::collections::HashMap;

pub struct ModelPricing {
    pub input_per_mtok: f32,
    pub output_per_mtok: f32,
}

/// Look up built-in pricing for a frontier model
pub fn get_pricing(model: &str) -> Option<ModelPricing> {
    match model {
        // Anthropic
        "claude-opus-4-20250514" => Some(ModelPricing {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
        }),
        "claude-sonnet-4-20250514" => Some(ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        }),
        // OpenAI
        "gpt-4.1" => Some(ModelPricing {
            input_per_mtok: 2.0,
            output_per_mtok: 8.0,
        }),
        "o4-mini" => Some(ModelPricing {
            input_per_mtok: 1.10,
            output_per_mtok: 4.40,
        }),
        // xAI
        "grok-3" => Some(ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        }),
        "grok-3-mini" => Some(ModelPricing {
            input_per_mtok: 0.30,
            output_per_mtok: 0.50,
        }),
        // MiniMax
        "MiniMax-M1" => Some(ModelPricing {
            input_per_mtok: 0.80,
            output_per_mtok: 3.20,
        }),
        _ => None,
    }
}

/// Look up pricing with user overrides taking precedence
pub fn get_pricing_with_overrides(
    model: &str,
    overrides: &HashMap<String, crate::config::PricingOverride>,
) -> Option<ModelPricing> {
    if let Some(ov) = overrides.get(model) {
        return Some(ModelPricing {
            input_per_mtok: ov.input_per_mtok,
            output_per_mtok: ov.output_per_mtok,
        });
    }
    get_pricing(model)
}

pub fn calculate_cost(pricing: &ModelPricing, tokens_in: u64, tokens_out: u64) -> f32 {
    (tokens_in as f32 / 1_000_000.0) * pricing.input_per_mtok
        + (tokens_out as f32 / 1_000_000.0) * pricing.output_per_mtok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_has_pricing() {
        let pricing = get_pricing("claude-opus-4-20250514").expect("should have pricing");
        assert_eq!(pricing.input_per_mtok, 15.0);
        assert_eq!(pricing.output_per_mtok, 75.0);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(get_pricing("some-unknown-model-xyz").is_none());
    }

    #[test]
    fn cost_calculation() {
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        // 1M input tokens + 500k output tokens
        let cost = calculate_cost(&pricing, 1_000_000, 500_000);
        assert!((cost - 10.5).abs() < 0.0001, "expected 10.5, got {cost}");
    }

    #[test]
    fn override_replaces_builtin() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-4.1".to_string(),
            crate::config::PricingOverride {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
            },
        );
        let pricing = get_pricing_with_overrides("gpt-4.1", &overrides)
            .expect("should have pricing via override");
        assert_eq!(pricing.input_per_mtok, 1.0);
        assert_eq!(pricing.output_per_mtok, 2.0);
    }
}
