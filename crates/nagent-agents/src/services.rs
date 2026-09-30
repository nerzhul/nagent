//! Service catalogue exposed to the chat UI.
//!
//! `ServiceDef` is the static description of one integration (set of
//! fields it needs, label, icon, docs link). The runtime
//! [`ServiceRegistry`] holds a `&'static [ServiceDef]` and exposes
//! them to:
//!
//! - the `GET /api/integrations` HTTP handler;
//! - the LLM system prompt (configured-only slice, see
//!   `llm::prompt::configured_integrations_block`).
//!
//! v1 ships with an empty registry (`&[]`). Future verticals
//! (productivity, dev-tools, home automation) append their
//! `ServiceDef`s here.

use serde::Serialize;

/// Visual / semantic kind of a single field in a service form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    /// Plain text input (`<input type="text">`). Used for hosts,
    /// usernames, account IDs.
    Text,
    /// Password input (`<input type="password">`). The UI masks the
    /// value and the server still stores the plaintext encrypted.
    Password,
    /// URL input (`<input type="url">`). Browser validates the format
    /// before submitting.
    Url,
}

/// Static description of a single configurable field.
#[derive(Debug, Clone)]
pub struct FieldDef {
    /// Stable key used as the JSON property name in
    /// `PUT /api/integrations/:id/credentials` and in
    /// `ctx.secret("svc", field_key)`.
    pub key: &'static str,
    /// Human-readable label rendered above the input.
    pub label: &'static str,
    /// `Text` / `Password` / `Url` — drives the HTML input type.
    pub kind: FieldKind,
    /// Whether the field must be set for the service to count as
    /// "configured" in the UI. Server-side validation enforces this
    /// on PUT.
    pub required: bool,
    /// Optional help text rendered under the input.
    pub help: Option<&'static str>,
    /// Optional placeholder shown when the input is empty.
    pub placeholder: Option<&'static str>,
}

/// Static description of one integration.
#[derive(Debug, Clone)]
pub struct ServiceDef {
    /// Stable identifier (`"email_imap"`, `"github"`, …). Used as the
    /// URL segment under `/api/integrations/:id` and as the
    /// `service` argument of `ctx.secret(...)`.
    pub id: &'static str,
    /// Display name shown in the UI (`"Email (IMAP)"`).
    pub display_name: &'static str,
    /// Icon — unicode emoji or short token. Rendered verbatim in
    /// the UI; the server does not interpret it.
    pub icon: &'static str,
    /// Ordered list of fields. Order is preserved in the UI form.
    pub fields: &'static [FieldDef],
    /// Optional docs URL surfaced in the UI.
    pub docs_url: Option<&'static str>,
}

/// Cheap, `Clone`-friendly catalogue. The internal slice is
/// `'static` so the registry can be built once at boot and shared via
/// `Arc` without lifetime gymnastics.
#[derive(Debug, Clone, Copy)]
pub struct ServiceRegistry {
    services: &'static [ServiceDef],
}

impl ServiceRegistry {
    /// Build a registry from a static slice.
    pub const fn new(services: &'static [ServiceDef]) -> Self {
        Self { services }
    }

    /// Empty registry — the v1 default. The UI renders
    /// "no integrations available yet" and the LLM does not get any
    /// integration list block.
    pub const fn empty() -> Self {
        Self { services: &[] }
    }

    /// Wrap in an `Arc` for cheap cloning into per-request contexts.
    pub fn into_arc(self) -> std::sync::Arc<Self> {
        std::sync::Arc::new(self)
    }

    /// Iterate every registered service.
    pub fn list(&self) -> &'static [ServiceDef] {
        self.services
    }

    /// Look up a service by id.
    pub fn get(&self, id: &str) -> Option<&'static ServiceDef> {
        self.services.iter().find(|s| s.id == id)
    }

    /// Whether the registry has no services registered.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Number of registered services.
    pub fn len(&self) -> usize {
        self.services.len()
    }
}

/// JSON shape returned by `GET /api/integrations` for each service.
/// Built per-request so the `configured` boolean reflects the
/// caller's `user_credentials` rows.
#[derive(Debug, Serialize)]
pub struct ServiceSummary {
    pub id: &'static str,
    pub display_name: &'static str,
    pub icon: &'static str,
    pub fields: Vec<FieldSummary>,
    pub configured: bool,
    pub docs_url: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct FieldSummary {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    pub help: Option<&'static str>,
    pub placeholder: Option<&'static str>,
    /// True iff the user has saved a value for this field. Never
    /// carries the value itself — only the configured-bit.
    pub filled: bool,
}

impl ServiceDef {
    /// Project to the JSON shape returned to the browser. `filled`
    /// is computed by the caller from
    /// `AuthStore::list_configured_field_keys(user_id, id)`.
    pub fn to_summary(&self, filled_keys: &[String]) -> ServiceSummary {
        let fields = self
            .fields
            .iter()
            .map(|f| FieldSummary {
                key: f.key,
                label: f.label,
                kind: f.kind,
                required: f.required,
                help: f.help,
                placeholder: f.placeholder,
                filled: filled_keys.iter().any(|k| k == f.key),
            })
            .collect();
        let configured = self
            .fields
            .iter()
            .filter(|f| f.required)
            .all(|f| filled_keys.iter().any(|k| k == f.key));
        ServiceSummary {
            id: self.id,
            display_name: self.display_name,
            icon: self.icon,
            fields,
            configured,
            docs_url: self.docs_url,
        }
    }

    /// One-line human-readable description used in the configured
    /// integrations system-prompt block. Mirrors `Agent::description()`
    /// in tone so the LLM sees the same summary as for any agent
    /// tool.
    pub fn description_line(&self) -> String {
        let required: Vec<&'static str> = self
            .fields
            .iter()
            .filter(|f| f.required)
            .map(|f| f.key)
            .collect();
        if required.is_empty() {
            "per-user integration; call the matching tool when the user references this service."
                .to_string()
        } else {
            format!(
                "per-user integration (required fields: {}); call the matching tool when the user references this service.",
                required.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_SERVICE: ServiceDef = ServiceDef {
        id: "fake",
        display_name: "Fake",
        icon: "?",
        fields: &[
            FieldDef {
                key: "host",
                label: "Host",
                kind: FieldKind::Text,
                required: true,
                help: None,
                placeholder: Some("imap.example.com"),
            },
            FieldDef {
                key: "password",
                label: "Password",
                kind: FieldKind::Password,
                required: true,
                help: None,
                placeholder: None,
            },
        ],
        docs_url: None,
    };

    #[test]
    fn empty_registry_has_no_services() {
        let reg = ServiceRegistry::empty();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.list().is_empty());
    }

    #[test]
    fn registry_get_returns_known_service() {
        let reg = ServiceRegistry::new(&[FAKE_SERVICE]);
        assert!(reg.get("fake").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn summary_configured_only_when_required_fields_filled() {
        let s = FAKE_SERVICE.to_summary(&[]);
        assert!(!s.configured, "no fields filled → not configured");
        let s = FAKE_SERVICE.to_summary(&["host".to_string()]);
        assert!(!s.configured, "missing required password → not configured");
        let s = FAKE_SERVICE.to_summary(&["host".to_string(), "password".to_string()]);
        assert!(s.configured, "both required fields filled");
        assert_eq!(s.fields.len(), 2);
        assert!(s.fields[0].filled);
    }
}
