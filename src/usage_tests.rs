use super::*;

fn sample_profile() -> serde_json::Value {
    serde_json::json!({
        "account": {
            "uuid": "acc-uuid",
            "email": "dev@example.com",
            "display_name": "Dev",
            "full_name": "Dev Full",
            "created_at": "2026-01-01T00:00:00Z"
        },
        "organization": {
            "uuid": "org-uuid",
            "name": "Dev's Org",
            "organization_type": "claude_max",
            "rate_limit_tier": "default_claude_max_20x",
            "billing_type": "stripe_subscription",
            "has_extra_usage_enabled": false,
            "subscription_created_at": "2026-01-02T00:00:00Z"
        }
    })
}

#[test]
fn oauth_account_from_profile_maps_fields() {
    let o = oauth_account_from_profile(&sample_profile()).unwrap();
    assert_eq!(o["accountUuid"], "acc-uuid");
    assert_eq!(o["emailAddress"], "dev@example.com");
    assert_eq!(o["displayName"], "Dev");
    assert_eq!(o["fullName"], "Dev Full");
    assert_eq!(o["accountCreatedAt"], "2026-01-01T00:00:00Z");
    assert_eq!(o["organizationUuid"], "org-uuid");
    assert_eq!(o["organizationName"], "Dev's Org");
    assert_eq!(o["organizationType"], "claude_max");
    assert_eq!(o["organizationRateLimitTier"], "default_claude_max_20x");
    assert_eq!(o["billingType"], "stripe_subscription");
    assert_eq!(o["hasExtraUsageEnabled"], false);
    assert_eq!(o["subscriptionCreatedAt"], "2026-01-02T00:00:00Z");
}

#[test]
fn oauth_account_from_profile_none_without_account() {
    let p = serde_json::json!({ "organization": { "uuid": "x" } });
    assert!(oauth_account_from_profile(&p).is_none());
}

#[test]
fn oauth_account_from_profile_missing_org_fields_are_null() {
    let p = serde_json::json!({ "account": { "uuid": "a", "email": "e@x.com" } });
    let o = oauth_account_from_profile(&p).unwrap();
    assert_eq!(o["accountUuid"], "a");
    assert!(o["organizationUuid"].is_null());
}

#[test]
fn usage_deserializes_windows() {
    let json = serde_json::json!({
        "five_hour": { "utilization": 9.0, "resets_at": "2026-09-05T08:00:00Z" },
        "seven_day": { "utilization": 61.5, "resets_at": "2026-09-09T04:00:00Z" },
        "seven_day_opus": null
    })
    .to_string();
    let u: Usage = serde_json::from_str(&json).unwrap();
    assert_eq!(u.five_hour.as_ref().unwrap().utilization, Some(9.0));
    assert_eq!(
        u.five_hour.as_ref().unwrap().resets_at.as_deref(),
        Some("2026-09-05T08:00:00Z")
    );
    assert_eq!(u.seven_day.as_ref().unwrap().utilization, Some(61.5));
    assert!(u.seven_day_opus.is_none());
}

#[test]
fn usage_window_defaults_when_fields_absent() {
    let json = serde_json::json!({
        "five_hour": {},
        "seven_day": {},
        "seven_day_opus": null
    })
    .to_string();
    let u: Usage = serde_json::from_str(&json).unwrap();
    assert!(u.five_hour.as_ref().unwrap().utilization.is_none());
    assert!(u.five_hour.as_ref().unwrap().resets_at.is_none());
}
