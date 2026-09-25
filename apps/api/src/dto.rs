//! Response shapes shared by the API routes.

use omnion_identity::User;
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

/// Public representation of an account.
///
/// Explicit on purpose: internal columns (password hash, session bookkeeping) never leave the
/// identity crate through this type, and any future column has to be added here deliberately.
#[derive(Debug, Clone, Serialize)]
pub struct UserBody {
    /// Account id.
    pub id: Uuid,
    /// Primary organization (`null` for platform-level accounts).
    pub organization_id: Option<Uuid>,
    /// Email address.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// Account status (`active`, `invited`, `disabled`).
    pub status: String,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&User> for UserBody {
    fn from(user: &User) -> Self {
        Self {
            id: user.id,
            organization_id: user.organization_id,
            email: user.email.clone(),
            display_name: user.display_name.clone(),
            status: user.status.clone(),
            created_at: user.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_user() -> User {
        User {
            id: Uuid::nil(),
            organization_id: None,
            email: "ada@example.com".to_owned(),
            display_name: "Ada".to_owned(),
            status: "active".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn user_body_serializes_the_public_fields_only() {
        let body = UserBody::from(&sample_user());
        let json = serde_json::to_value(&body).expect("body must serialize");
        assert_eq!(json["email"], "ada@example.com");
        assert_eq!(json["display_name"], "Ada");
        assert_eq!(json["status"], "active");
        assert_eq!(json["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["organization_id"], serde_json::Value::Null);
        assert!(json.get("password_hash").is_none());
        assert!(json.get("token_hash").is_none());
    }
}
