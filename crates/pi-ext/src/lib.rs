use pi_permissions::Capability;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiVersion {
    pub major: u16,
    pub minor: u16,
}

impl AbiVersion {
    pub const V1: Self = Self { major: 1, minor: 0 };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub abi: AbiVersion,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hostcall {
    Tool { name: String, input: String },
    SessionRead { key: String },
    SessionWrite { key: String, value: String },
    Http { method: String, url: String },
    UiNotify { message: String },
}

impl Hostcall {
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::Tool { .. } => Capability::ExtensionHostcall,
            Self::SessionRead { .. } | Self::SessionWrite { .. } => Capability::Session,
            Self::Http { .. } => Capability::Network,
            Self::UiNotify { .. } => Capability::ExtensionHostcall,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionConformanceCase {
    pub id: String,
    pub hostcall: Hostcall,
    pub expected_capability: Capability,
}

pub fn mvp_conformance_cases() -> Vec<ExtensionConformanceCase> {
    vec![
        ExtensionConformanceCase {
            id: "tool-hostcall-requires-extension-capability".to_string(),
            hostcall: Hostcall::Tool {
                name: "read".to_string(),
                input: "README.md".to_string(),
            },
            expected_capability: Capability::ExtensionHostcall,
        },
        ExtensionConformanceCase {
            id: "http-hostcall-requires-network".to_string(),
            hostcall: Hostcall::Http {
                method: "GET".to_string(),
                url: "https://example.com".to_string(),
            },
            expected_capability: Capability::Network,
        },
    ]
}
