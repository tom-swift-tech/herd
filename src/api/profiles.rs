use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde::Serialize;

#[derive(Serialize)]
pub struct ProfilesResponse {
    pub enabled: bool,
    pub default_profile: Option<String>,
    pub profiles: std::collections::HashMap<String, ProfileSummary>,
}

#[derive(Serialize)]
pub struct ProfileSummary {
    pub strategy: String,
    pub tags: Vec<String>,
    pub backends: Vec<String>,
    pub preferred_model: Option<String>,
    pub description: Option<String>,
}

/// GET /api/profiles — list all configured routing profiles
pub async fn list_profiles(
    State(state): State<AppState>,
) -> Json<ProfilesResponse> {
    let config = state.config.read().await;
    let pc = &config.routing_profiles;

    let profiles = pc
        .profiles
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                ProfileSummary {
                    strategy: p.strategy.to_string(),
                    tags: p.tags.clone(),
                    backends: p.backends.clone(),
                    preferred_model: p.preferred_model.clone(),
                    description: p.description.clone(),
                },
            )
        })
        .collect();

    Json(ProfilesResponse {
        enabled: pc.enabled,
        default_profile: pc.default_profile.clone(),
        profiles,
    })
}

#[derive(serde::Deserialize)]
pub struct SetDefaultRequest {
    pub profile: String,
}

/// PUT /api/profiles/default — switch the default profile at runtime (admin auth required)
pub async fn set_default_profile(
    State(state): State<AppState>,
    Json(body): Json<SetDefaultRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let mut config = state.config.write().await;

    // Validate that the profile exists (if profiles are enabled)
    if config.routing_profiles.enabled && !config.routing_profiles.profiles.contains_key(&body.profile) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("Profile '{}' not found", body.profile)
            })),
        ));
    }

    config.routing_profiles.default_profile = Some(body.profile.clone());

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "default_profile": body.profile
        })),
    ))
}
