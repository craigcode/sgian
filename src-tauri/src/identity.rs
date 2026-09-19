use super::*;

// ---------------------------------------------------------------------------
// (M6) Per-client identity: docs/design/client-identity.md.
// ---------------------------------------------------------------------------

/// What a credential may do. `read`: snapshot, subscribe, search, dossier,
/// hook and status-line reports. `write`: input, leases, pane lifecycle,
/// projects. `admin`: identities, config, shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ClientScope {
    Read,
    Write,
    Admin,
}

impl ClientScope {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityPolicy {
    Open,
    Required,
}

impl IdentityPolicy {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "open" => Some(Self::Open),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Required => "required",
        }
    }
}

/// One issued credential. The token itself is shown once at issue time and
/// only its hash is kept.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ClientRecord {
    pub(crate) id: String,
    pub(crate) holder: String,
    pub(crate) scopes: Vec<ClientScope>,
    pub(crate) token_hash: String,
    pub(crate) created_at_ms: u64,
    #[serde(default)]
    pub(crate) last_seen_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revoked_at_ms: Option<u64>,
}

impl ClientRecord {
    /// The listing shape: everything but the hash.
    pub(crate) fn public(&self) -> Value {
        json!({
            "id": self.id,
            "holder": self.holder,
            "scopes": self.scopes,
            "created_at_ms": self.created_at_ms,
            "last_seen_ms": self.last_seen_ms,
            "revoked_at_ms": self.revoked_at_ms,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ClientsFile {
    #[serde(default)]
    pub(crate) clients: Vec<ClientRecord>,
}

pub(crate) const CLIENT_TOKEN_PREFIX: &str = "sgc_";
pub(crate) const CLIENT_TOKEN_HASH_PREFIX: &str = "sgian.client.v1\n";
pub(crate) const MAX_CLIENT_RECORDS: usize = 256;

pub(crate) fn client_token_hash(token: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(CLIENT_TOKEN_HASH_PREFIX.as_bytes());
    hasher.update(token.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Who a connection is (docs/design/client-identity.md). `credential` and
/// `holder` are `None` for the workspace token (the root credential): its
/// holder stays self-declared, as before M6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientIdentity {
    pub(crate) credential: Option<String>,
    pub(crate) holder: Option<String>,
    pub(crate) scopes: Vec<ClientScope>,
}

impl ClientIdentity {
    /// The workspace token: everything under `open`; read and admin (no
    /// writes) under `required`, so every keystroke needs a credential.
    pub(crate) fn root(policy: IdentityPolicy) -> Self {
        let scopes = match policy {
            IdentityPolicy::Open => vec![ClientScope::Read, ClientScope::Write, ClientScope::Admin],
            IdentityPolicy::Required => vec![ClientScope::Read, ClientScope::Admin],
        };
        Self {
            credential: None,
            holder: None,
            scopes,
        }
    }

    pub(crate) fn from_record(record: &ClientRecord) -> Self {
        let mut scopes = record.scopes.clone();
        if !scopes.contains(&ClientScope::Read) {
            scopes.push(ClientScope::Read);
        }
        Self {
            credential: Some(record.id.clone()),
            holder: Some(record.holder.clone()),
            scopes,
        }
    }

    pub(crate) fn has(&self, scope: ClientScope) -> bool {
        self.scopes.contains(&scope)
    }

    pub(crate) fn describe(&self, policy: IdentityPolicy) -> Value {
        json!({
            "credential": self.credential,
            "holder": self.holder,
            "scopes": self.scopes,
            "root": self.credential.is_none(),
            "identity_policy": policy.name(),
        })
    }
}

/// The scope a request needs. Reads include the hook and status-line
/// reports (observations, not keystrokes). Anything not listed is a write.
pub(crate) fn request_scope(request: &DaemonRequest) -> ClientScope {
    match request {
        DaemonRequest::Ping
        | DaemonRequest::BootstrapWorkspace
        | DaemonRequest::ListPanes
        | DaemonRequest::PaneStatus { .. }
        | DaemonRequest::LeaseStatus { .. }
        | DaemonRequest::KranzBindings
        | DaemonRequest::ProjectList
        | DaemonRequest::ProjectShow { .. }
        | DaemonRequest::ProjectLedger { .. }
        | DaemonRequest::ProjectDossier { .. }
        | DaemonRequest::AgentStatus { .. }
        | DaemonRequest::AgentSignal { .. }
        | DaemonRequest::GetScrollback { .. }
        | DaemonRequest::SearchScrollback { .. }
        | DaemonRequest::ScrollbackLines { .. }
        | DaemonRequest::GetConfig
        | DaemonRequest::StatusVerbose
        | DaemonRequest::Wait { .. }
        | DaemonRequest::Snapshot { .. }
        | DaemonRequest::Find { .. }
        | DaemonRequest::Subscribe
        | DaemonRequest::Whoami => ClientScope::Read,
        DaemonRequest::WriteConfig { .. }
        | DaemonRequest::Shutdown
        | DaemonRequest::IdentityIssue { .. }
        | DaemonRequest::IdentityList
        | DaemonRequest::IdentityRevoke { .. } => ClientScope::Admin,
        _ => ClientScope::Write,
    }
}

pub(crate) fn request_name(request: &DaemonRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value["command"].as_str().map(str::to_string))
        .unwrap_or_else(|| "request".to_string())
}

/// Bind a credentialed connection's writes to its holder: unattributed input
/// becomes attributed input, a declared holder must match, and broadcast (no
/// holder) is refused. The root credential passes through unchanged.
pub(crate) fn bind_holder(
    request: DaemonRequest,
    identity: &ClientIdentity,
) -> Result<DaemonRequest, String> {
    let Some(own) = identity.holder.as_deref() else {
        return Ok(request);
    };
    let mismatch = |declared: &str| {
        format!("holder '{declared}' does not match this credential's holder '{own}'")
    };
    Ok(match request {
        DaemonRequest::SendInput { pane_id, input }
        | DaemonRequest::WriteToPane {
            pane_id,
            data: input,
        } => DaemonRequest::SendInputAs {
            pane_id,
            input,
            holder: own.to_string(),
            generation: None,
        },
        DaemonRequest::SendInputAs { ref holder, .. }
        | DaemonRequest::TakeLease { ref holder, .. }
        | DaemonRequest::ReleaseLease { ref holder, .. }
            if holder != own =>
        {
            return Err(mismatch(holder));
        }
        DaemonRequest::Broadcast { .. } => {
            return Err(
                "broadcast has no holder; a credentialed client sends per pane".to_string(),
            );
        }
        other => other,
    })
}

/// The per-client token this process presents, if any: `SGIAN_CLIENT_TOKEN`,
/// else the first line of the file named by `SGIAN_CLIENT_TOKEN_FILE`.
pub(crate) fn client_token_from_env() -> Option<String> {
    if let Ok(token) = std::env::var("SGIAN_CLIENT_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    let path = std::env::var_os("SGIAN_CLIENT_TOKEN_FILE")?;
    read_token(Path::new(&path)).ok().flatten()
}
