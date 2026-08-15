use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteBillingKind {
    Metered,
    Subscription,
    IncludedQuota,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteCostSource {
    PublicApiPricing,
    PublicPlanPricing,
    RuntimePlan,
    OpenRouterEndpoint,
    OpenRouterCatalog,
    /// Live models.dev pricing catalog (https://models.dev/api.json).
    ModelsDevCatalog,
    /// Live Copilot `/models` catalog (api.githubcopilot.com/models).
    CopilotCatalog,
    Heuristic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteCostConfidence {
    Exact,
    High,
    Medium,
    Low,
    Unknown,
}
