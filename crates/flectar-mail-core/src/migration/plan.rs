//! Decides what Flectar can do with each source account.
//!
//! The planner is pure: it never touches storage, so the review screen and
//! tests see exactly what [`crate::Core::import_accounts`] would persist.
//! Differences between the source configuration and the Flectar account are
//! reported as [`Finding`]s instead of being applied silently.

use super::model::{
    AccountCandidate, IdentityCandidate, ImportInventory, ServerCandidate, ServerProtocol,
    SignatureCandidate, SourceAuth, SourceSecurity,
};
use crate::accounts::providers;
use crate::models::{
    AccountSettings, AuthKind, ConnectionSecurity, MailConnectionSettings, MailProtocol,
    PortableAccountConfig, Provider,
};
use crate::sender_identities::MAX_DISPLAY_NAME_CHARS;
use crate::signatures::{MAX_SIGNATURE_BYTES, MAX_SIGNATURES_PER_ACCOUNT};
use std::collections::HashSet;

/// Matches the limit Core applies to stored account user names.
const MAX_USERNAME_BYTES: usize = 512;

/// What the review screen offers for one source account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPlan {
    pub source_id: String,
    pub label: Option<String>,
    /// The address the Flectar account would use; empty when none was found.
    pub email: String,
    pub status: PlanStatus,
    pub sign_in: SignIn,
    pub findings: Vec<Finding>,
    /// What import would persist. Present only for [`PlanStatus::Ready`].
    pub draft: Option<AccountDraft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Ready,
    /// Flectar already has an account with this address. Existing accounts
    /// are never modified by an import.
    AlreadyAdded,
    /// At least one [`Severity::Blocking`] finding prevents import.
    Unsupported,
}

/// How the user authenticates after import. Imported accounts never carry
/// credentials, so every one of them needs this step before it syncs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    Password,
    Google,
    Microsoft,
}

impl SignIn {
    pub fn for_provider(provider: Provider) -> Self {
        match provider {
            Provider::Imap => Self::Password,
            Provider::Gmail => Self::Google,
            Provider::Microsoft => Self::Microsoft,
        }
    }
}

/// Persistable result of planning one account. Core validates it again
/// before writing, so a draft cannot bypass the rules for new accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDraft {
    pub config: PortableAccountConfig,
    pub aliases: Vec<AliasDraft>,
    /// Signatures in source identity order.
    pub signatures: Vec<SignatureDraft>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasDraft {
    pub email: String,
    pub display_name: Option<String>,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureDraft {
    pub name: String,
    /// Unsanitized HTML; Core sanitizes it with the signature editor's rules.
    pub html: String,
    /// The default identity's signature, used for new messages and replies.
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: FindingCode,
    /// Value the message refers to, such as a host name or a count.
    pub detail: String,
}

impl Finding {
    fn new(code: FindingCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub fn severity(&self) -> Severity {
        self.code.severity()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Notice,
    Blocking,
}

/// Stable reasons a source account differs from what Flectar will create.
/// Hosts map [`FindingCode::as_str`] to localized text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCode {
    /// No identity has a usable email address.
    MissingEmail,
    MissingIncomingServer,
    /// POP3 accounts cannot be imported; detail is the host.
    Pop3Unsupported,
    /// Exchange Web Services, WebDAV, or another non-IMAP protocol.
    IncomingProtocolUnsupported,
    MissingOutgoingServer,
    OutgoingProtocolUnsupported,
    /// Flectar only connects with TLS or required STARTTLS; detail is the host.
    UnencryptedIncoming,
    UnencryptedOutgoing,
    /// Client certificates, Kerberos, NTLM, or no authentication.
    IncomingSignInUnsupported,
    OutgoingSignInUnsupported,
    /// The outgoing server signs in with a different user name; detail is it.
    SeparateOutgoingLogin,
    MissingUsername,
    /// A host name or port is unusable; detail is the offending value.
    InvalidServer,
    /// The same address appears earlier in the source.
    DuplicateInSource,
    /// CRAM-MD5-style mechanisms become PLAIN/LOGIN inside TLS.
    PasswordMechanismChanged,
    /// Microsoft no longer accepts passwords; the account uses Microsoft
    /// sign-in instead.
    MicrosoftSignInReplacesPassword,
    /// Gmail accounts use Google sign-in instead of an (app) password.
    GoogleSignInReplacesPassword,
    /// Google or Microsoft sign-in needs an address as the login, but the
    /// source signs in as something else (such as a shared mailbox); detail
    /// is the login.
    OAuthLoginUnsupported,
    /// OAuth for this host is unavailable; sign in with an app password.
    /// Detail is the host.
    OAuthReplacedByPassword,
    /// The provider's standard outgoing server replaces the configured one;
    /// detail is the source host.
    OutgoingServerReplaced,
    /// Gmail controls send-as addresses; detail is the number of addresses.
    AliasesManagedByProvider,
    /// Addresses that are invalid, send through another outgoing server, or
    /// exceed the identity limit; detail is the number skipped.
    AliasesSkipped,
    /// A signature stored in a file outside the selected source.
    SignatureFileNotImported,
    /// A signature larger than the signature editor accepts.
    SignatureTooLarge,
}

impl FindingCode {
    pub const fn severity(self) -> Severity {
        match self {
            Self::MissingEmail
            | Self::MissingIncomingServer
            | Self::Pop3Unsupported
            | Self::IncomingProtocolUnsupported
            | Self::MissingOutgoingServer
            | Self::OutgoingProtocolUnsupported
            | Self::UnencryptedIncoming
            | Self::UnencryptedOutgoing
            | Self::IncomingSignInUnsupported
            | Self::OutgoingSignInUnsupported
            | Self::SeparateOutgoingLogin
            | Self::MissingUsername
            | Self::InvalidServer
            | Self::DuplicateInSource
            | Self::OAuthLoginUnsupported => Severity::Blocking,
            Self::PasswordMechanismChanged
            | Self::MicrosoftSignInReplacesPassword
            | Self::GoogleSignInReplacesPassword
            | Self::OAuthReplacedByPassword
            | Self::OutgoingServerReplaced
            | Self::AliasesManagedByProvider
            | Self::AliasesSkipped
            | Self::SignatureFileNotImported
            | Self::SignatureTooLarge => Severity::Notice,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingEmail => "missing-email",
            Self::MissingIncomingServer => "missing-incoming-server",
            Self::Pop3Unsupported => "pop3-unsupported",
            Self::IncomingProtocolUnsupported => "incoming-protocol-unsupported",
            Self::MissingOutgoingServer => "missing-outgoing-server",
            Self::OutgoingProtocolUnsupported => "outgoing-protocol-unsupported",
            Self::UnencryptedIncoming => "unencrypted-incoming",
            Self::UnencryptedOutgoing => "unencrypted-outgoing",
            Self::IncomingSignInUnsupported => "incoming-sign-in-unsupported",
            Self::OutgoingSignInUnsupported => "outgoing-sign-in-unsupported",
            Self::SeparateOutgoingLogin => "separate-outgoing-login",
            Self::MissingUsername => "missing-username",
            Self::InvalidServer => "invalid-server",
            Self::DuplicateInSource => "duplicate-in-source",
            Self::PasswordMechanismChanged => "password-mechanism-changed",
            Self::MicrosoftSignInReplacesPassword => "microsoft-sign-in-replaces-password",
            Self::GoogleSignInReplacesPassword => "google-sign-in-replaces-password",
            Self::OAuthLoginUnsupported => "oauth-login-unsupported",
            Self::OAuthReplacedByPassword => "oauth-replaced-by-password",
            Self::OutgoingServerReplaced => "outgoing-server-replaced",
            Self::AliasesManagedByProvider => "aliases-managed-by-provider",
            Self::AliasesSkipped => "aliases-skipped",
            Self::SignatureFileNotImported => "signature-file-not-imported",
            Self::SignatureTooLarge => "signature-too-large",
        }
    }
}

/// Plans every account in `inventory`. `existing_emails` are the addresses
/// Flectar already has; matching accounts are reported, never changed.
pub fn plan(inventory: &ImportInventory, existing_emails: &[String]) -> Vec<AccountPlan> {
    let existing = existing_emails
        .iter()
        .map(|email| email.trim().to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    inventory
        .accounts
        .iter()
        .map(|candidate| plan_account(candidate, &existing, &mut seen))
        .collect()
}

fn plan_account(
    candidate: &AccountCandidate,
    existing: &HashSet<String>,
    seen: &mut HashSet<String>,
) -> AccountPlan {
    let mut findings = Vec::new();
    let primary = candidate
        .identities
        .iter()
        .find_map(|identity| Some((identity, sender_address(&identity.email)?)));
    let primary_email = primary.as_ref().map(|(_, email)| email.as_str());
    let mapping = map_servers(candidate, primary_email, &mut findings);

    let email = match &mapping {
        Some(mapping) => mapping.email.clone(),
        None => primary_email.unwrap_or_default().to_owned(),
    };
    if email.is_empty() {
        findings.push(Finding::new(FindingCode::MissingEmail, ""));
    }
    // Only an importable account claims its address, so an unsupported one
    // (an old POP3 setup) never hides a later account for the same mailbox.
    let blocked = blocking_count(&findings) > 0;
    if !blocked && !email.is_empty() && !seen.insert(email.to_ascii_lowercase()) {
        findings.push(Finding::new(FindingCode::DuplicateInSource, email.clone()));
    }

    let blocked = blocking_count(&findings) > 0;
    let already_added = !email.is_empty() && existing.contains(&email.to_ascii_lowercase());
    let sign_in = mapping.as_ref().map_or(SignIn::Password, |mapping| {
        SignIn::for_provider(mapping.provider)
    });
    let status = if blocked {
        PlanStatus::Unsupported
    } else if already_added {
        PlanStatus::AlreadyAdded
    } else {
        PlanStatus::Ready
    };
    let draft = match (status, mapping) {
        (PlanStatus::Ready, Some(mapping)) => Some(draft(
            candidate,
            primary.map(|(identity, _)| identity),
            mapping,
            &mut findings,
        )),
        _ => None,
    };
    findings.sort_by_key(|finding| std::cmp::Reverse(finding.severity()));
    AccountPlan {
        source_id: candidate.source_id.clone(),
        label: clean(candidate.label.as_deref()),
        email,
        status,
        sign_in,
        findings,
        draft,
    }
}

/// The Flectar provider and servers chosen for one account.
struct ServerMapping {
    provider: Provider,
    email: String,
    username: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    connection: MailConnectionSettings,
}

fn map_servers(
    candidate: &AccountCandidate,
    primary_email: Option<&str>,
    findings: &mut Vec<Finding>,
) -> Option<ServerMapping> {
    let Some(incoming) = &candidate.incoming else {
        findings.push(Finding::new(FindingCode::MissingIncomingServer, ""));
        return None;
    };
    let host = incoming.host.trim().to_ascii_lowercase();
    match incoming.protocol {
        ServerProtocol::Imap => {}
        ServerProtocol::Ews if is_microsoft_host(&host) => {}
        ServerProtocol::Pop3 => {
            findings.push(Finding::new(FindingCode::Pop3Unsupported, &incoming.host));
            return None;
        }
        ServerProtocol::Ews | ServerProtocol::Smtp | ServerProtocol::Other => {
            findings.push(Finding::new(
                FindingCode::IncomingProtocolUnsupported,
                &incoming.host,
            ));
            return None;
        }
    }

    // Google and Microsoft accounts use their own sign-in, as Thunderbird
    // migrated them: both providers are retiring IMAP passwords.
    let oauth = incoming.auth == SourceAuth::OAuth2;
    for (provider, matches, replaced) in [
        (
            Provider::Gmail,
            is_google_host(&host),
            FindingCode::GoogleSignInReplacesPassword,
        ),
        (
            Provider::Microsoft,
            is_microsoft_host(&host),
            FindingCode::MicrosoftSignInReplacesPassword,
        ),
    ] {
        if matches {
            if !oauth {
                findings.push(Finding::new(replaced, &incoming.host));
            }
            return provider_mapping(provider, candidate, incoming, primary_email, findings);
        }
    }
    if incoming.protocol != ServerProtocol::Imap {
        findings.push(Finding::new(
            FindingCode::IncomingProtocolUnsupported,
            &incoming.host,
        ));
        return None;
    }
    password_mapping(candidate, incoming, primary_email?, findings)
}

/// Gmail and Microsoft accounts use Flectar's provider configuration, which
/// owns the server names and OAuth scopes.
fn provider_mapping(
    provider: Provider,
    candidate: &AccountCandidate,
    incoming: &ServerCandidate,
    primary_email: Option<&str>,
    findings: &mut Vec<Finding>,
) -> Option<ServerMapping> {
    let servers = match provider {
        Provider::Gmail => &providers::GMAIL,
        _ => &providers::MICROSOFT,
    };
    // Sign-in must return this exact mailbox, so the source login wins when
    // it is an address (a Workspace user may send as another identity). A
    // login that is not an address, such as Microsoft's shared mailbox form
    // `me@example.com\shared@example.com`, can never match.
    let login = incoming.username.trim();
    let email = match sender_address(login) {
        Some(email) => email,
        None if login.is_empty() => primary_email?.to_owned(),
        None => {
            findings.push(Finding::new(FindingCode::OAuthLoginUnsupported, login));
            return None;
        }
    };
    if let Some(outgoing) = &candidate.outgoing {
        let host = outgoing.host.trim().to_ascii_lowercase();
        let same_provider = match provider {
            Provider::Gmail => is_google_host(&host),
            _ => is_microsoft_host(&host),
        };
        if !host.is_empty() && !same_provider {
            findings.push(Finding::new(
                FindingCode::OutgoingServerReplaced,
                &outgoing.host,
            ));
        }
    }
    Some(ServerMapping {
        provider,
        username: email.clone(),
        email,
        imap_host: servers.imap_host.to_owned(),
        imap_port: servers.imap_port,
        smtp_host: servers.smtp_host.to_owned(),
        smtp_port: servers.smtp_port,
        connection: MailConnectionSettings::default(),
    })
}

fn password_mapping(
    candidate: &AccountCandidate,
    incoming: &ServerCandidate,
    primary_email: &str,
    findings: &mut Vec<Finding>,
) -> Option<ServerMapping> {
    let blocked_before = blocking_count(findings);
    let imap_security = check_security(incoming, FindingCode::UnencryptedIncoming, findings);
    check_password_auth(
        incoming,
        FindingCode::IncomingSignInUnsupported,
        SourceAuth::OAuth2,
        findings,
    );
    let imap_host = check_host(incoming, findings);
    let username = incoming.username.trim();
    if username.is_empty() {
        findings.push(Finding::new(FindingCode::MissingUsername, &incoming.host));
    } else if username.len() > MAX_USERNAME_BYTES {
        findings.push(Finding::new(FindingCode::InvalidServer, &incoming.host));
    }

    let Some(outgoing) = &candidate.outgoing else {
        findings.push(Finding::new(FindingCode::MissingOutgoingServer, ""));
        return None;
    };
    if outgoing.protocol != ServerProtocol::Smtp {
        findings.push(Finding::new(
            FindingCode::OutgoingProtocolUnsupported,
            &outgoing.host,
        ));
        return None;
    }
    let smtp_security = check_security(outgoing, FindingCode::UnencryptedOutgoing, findings);
    // An OAuth SMTP server paired with OAuth IMAP falls back to the same app
    // password; paired with a password login it cannot be reproduced.
    check_password_auth(
        outgoing,
        FindingCode::OutgoingSignInUnsupported,
        incoming.auth,
        findings,
    );
    let smtp_host = check_host(outgoing, findings);
    let outgoing_username = outgoing.username.trim();
    if !outgoing_username.is_empty()
        && !outgoing_username.eq_ignore_ascii_case(incoming.username.trim())
    {
        findings.push(Finding::new(
            FindingCode::SeparateOutgoingLogin,
            outgoing_username,
        ));
    }
    if incoming.auth == SourceAuth::OAuth2 {
        findings.push(Finding::new(
            FindingCode::OAuthReplacedByPassword,
            &incoming.host,
        ));
    }
    if [incoming.auth, outgoing.auth].contains(&SourceAuth::EncryptedPassword) {
        findings.push(Finding::new(FindingCode::PasswordMechanismChanged, ""));
    }
    if blocking_count(findings) > blocked_before {
        return None;
    }
    Some(ServerMapping {
        provider: Provider::Imap,
        email: primary_email.to_owned(),
        // Login names are opaque to the server; keep them byte for byte apart
        // from surrounding whitespace.
        username: incoming.username.trim().to_owned(),
        imap_host: imap_host?,
        imap_port: incoming.effective_port(),
        smtp_host: smtp_host?,
        smtp_port: outgoing.effective_port(),
        connection: MailConnectionSettings {
            imap_security: imap_security?,
            smtp_security: smtp_security?,
            trusted_certificate_pem: String::new(),
            ..Default::default()
        },
    })
}

fn blocking_count(findings: &[Finding]) -> usize {
    findings
        .iter()
        .filter(|finding| finding.severity() == Severity::Blocking)
        .count()
}

fn check_security(
    server: &ServerCandidate,
    unencrypted: FindingCode,
    findings: &mut Vec<Finding>,
) -> Option<ConnectionSecurity> {
    match server.security {
        SourceSecurity::Tls => Some(ConnectionSecurity::Tls),
        SourceSecurity::Starttls => Some(ConnectionSecurity::Starttls),
        SourceSecurity::None => {
            findings.push(Finding::new(unencrypted, &server.host));
            None
        }
        SourceSecurity::Unknown => {
            findings.push(Finding::new(FindingCode::InvalidServer, &server.host));
            None
        }
    }
}

/// `oauth_allowed_with` is the incoming method under which an OAuth server can
/// fall back to an app password.
fn check_password_auth(
    server: &ServerCandidate,
    unsupported: FindingCode,
    oauth_allowed_with: SourceAuth,
    findings: &mut Vec<Finding>,
) {
    let supported = match server.auth {
        SourceAuth::Password | SourceAuth::EncryptedPassword | SourceAuth::Automatic => true,
        SourceAuth::OAuth2 => oauth_allowed_with == SourceAuth::OAuth2,
        SourceAuth::ClientCertificate
        | SourceAuth::Kerberos
        | SourceAuth::Ntlm
        | SourceAuth::None => false,
        SourceAuth::Unknown => {
            findings.push(Finding::new(FindingCode::InvalidServer, &server.host));
            return;
        }
    };
    if !supported {
        findings.push(Finding::new(unsupported, auth_name(server.auth)));
    }
}

fn auth_name(auth: SourceAuth) -> &'static str {
    match auth {
        SourceAuth::Password | SourceAuth::Automatic => "password",
        SourceAuth::EncryptedPassword => "CRAM-MD5",
        SourceAuth::OAuth2 => "OAuth2",
        SourceAuth::ClientCertificate => "client certificate",
        SourceAuth::Kerberos => "Kerberos",
        SourceAuth::Ntlm => "NTLM",
        SourceAuth::None => "none",
        SourceAuth::Unknown => "unknown",
    }
}

fn check_host(server: &ServerCandidate, findings: &mut Vec<Finding>) -> Option<String> {
    let host = server.host.trim();
    let valid = !host.is_empty()
        && host.len() <= 253
        && !host.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '/'
        })
        && server.port != Some(0);
    if valid {
        Some(host.to_owned())
    } else {
        findings.push(Finding::new(FindingCode::InvalidServer, host));
        None
    }
}

fn draft(
    candidate: &AccountCandidate,
    primary: Option<&IdentityCandidate>,
    mapping: ServerMapping,
    findings: &mut Vec<Finding>,
) -> AccountDraft {
    let display_name =
        clean(primary.and_then(|identity| identity.display_name.as_deref())).map(|name| {
            name.chars()
                .take(MAX_DISPLAY_NAME_CHARS)
                .collect::<String>()
        });

    // Every other source identity becomes a sender identity. The account
    // address itself is the provider-owned primary identity.
    let mut aliases = Vec::new();
    let mut skipped = 0usize;
    let mut seen = HashSet::from([mapping.email.to_ascii_lowercase()]);
    for identity in &candidate.identities {
        let Some(email) = sender_address(&identity.email) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(email.to_ascii_lowercase()) {
            continue;
        }
        // Flectar sends every identity through the account's one outgoing
        // server, which may refuse an address it does not host.
        if identity.separate_outgoing_server
            || aliases.len() >= crate::sender_identities::MAX_IDENTITIES - 1
        {
            skipped += 1;
            continue;
        }
        aliases.push(AliasDraft {
            email,
            display_name: clean(identity.display_name.as_deref())
                .filter(|name| name.chars().count() <= MAX_DISPLAY_NAME_CHARS),
            reply_to: identity.reply_to.as_deref().and_then(sender_address),
        });
    }
    // When sign-in uses another address than the source's default identity,
    // that identity stays the default sender. Gmail decides that itself.
    let primary_email = primary.and_then(|identity| sender_address(&identity.email));
    let default_sender_email = primary_email.filter(|email| {
        mapping.provider != Provider::Gmail
            && aliases
                .iter()
                .any(|alias: &AliasDraft| alias.email.eq_ignore_ascii_case(email))
    });
    if mapping.provider == Provider::Gmail && !aliases.is_empty() {
        findings.push(Finding::new(
            FindingCode::AliasesManagedByProvider,
            aliases.len().to_string(),
        ));
        aliases.clear();
    }
    if skipped > 0 {
        findings.push(Finding::new(
            FindingCode::AliasesSkipped,
            skipped.to_string(),
        ));
    }

    let mut signatures = Vec::new();
    let mut external_signature = false;
    let mut oversized_signature = false;
    for (index, identity) in candidate.identities.iter().enumerate() {
        let html = match &identity.signature {
            Some(SignatureCandidate::Text(text)) if !text.trim().is_empty() => {
                crate::signatures::text_html(text.trim())
            }
            Some(SignatureCandidate::Html(html)) if !html.trim().is_empty() => {
                html.trim().to_owned()
            }
            Some(SignatureCandidate::ExternalFile) => {
                external_signature = true;
                continue;
            }
            _ => continue,
        };
        if html.len() > MAX_SIGNATURE_BYTES {
            oversized_signature = true;
            continue;
        }
        // The identity that gave the account its address supplies the
        // default signature, as the source's default identity did.
        let default = primary.map_or(index == 0, |primary| std::ptr::eq(primary, identity));
        if let Some(existing) = signatures
            .iter_mut()
            .find(|existing: &&mut SignatureDraft| existing.html == html)
        {
            existing.default |= default;
            continue;
        }
        signatures.push(SignatureDraft {
            name: signature_name(identity),
            html,
            default,
        });
    }
    // The editor's per-account limit; the default signature always fits.
    signatures.sort_by_key(|signature| !signature.default);
    signatures.truncate(MAX_SIGNATURES_PER_ACCOUNT);
    if external_signature {
        findings.push(Finding::new(FindingCode::SignatureFileNotImported, ""));
    }
    if oversized_signature {
        findings.push(Finding::new(FindingCode::SignatureTooLarge, ""));
    }

    AccountDraft {
        config: PortableAccountConfig {
            email: mapping.email,
            display_name,
            provider: mapping.provider,
            auth_kind: match mapping.provider {
                Provider::Imap => AuthKind::Password,
                Provider::Gmail | Provider::Microsoft => AuthKind::Oauth2,
            },
            mail_protocol: MailProtocol::Imap,
            username: mapping.username,
            jmap_url: String::new(),
            imap_host: mapping.imap_host,
            imap_port: mapping.imap_port,
            smtp_host: mapping.smtp_host,
            smtp_port: mapping.smtp_port,
            settings: AccountSettings {
                connection: mapping.connection,
                default_sender_email,
                ..AccountSettings::default()
            },
        },
        aliases,
        signatures,
        color: candidate.color.clone(),
    }
}

/// An address Flectar can send from, checked as Settings checks sender
/// identities, so an importable plan never fails when it is imported.
fn sender_address(value: &str) -> Option<String> {
    let email = crate::normalize_account_email(value)?;
    crate::sender_identities::manual_identity_fields(email, None, None)
        .ok()
        .map(|(email, _, _)| email)
}

/// Signature names are limited to 100 bytes. Naming each one after its
/// identity keeps several imported signatures distinguishable.
fn signature_name(identity: &IdentityCandidate) -> String {
    let email = identity.email.trim();
    if email.is_empty() || email.len() > 100 {
        "Signature".to_owned()
    } else {
        email.to_owned()
    }
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .chars()
                .filter(|character| !character.is_control())
                .collect()
        })
}

fn is_google_host(host: &str) -> bool {
    matches!(
        host,
        "imap.gmail.com" | "imap.googlemail.com" | "smtp.gmail.com" | "smtp.googlemail.com"
    )
}

fn is_microsoft_host(host: &str) -> bool {
    matches!(
        host,
        "outlook.office365.com"
            | "outlook.office.com"
            | "imap-mail.outlook.com"
            | "imap.outlook.com"
            | "smtp.office365.com"
            | "smtp-mail.outlook.com"
            | "smtp.outlook.com"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::model::SourceKind;

    fn server(protocol: ServerProtocol, host: &str, port: u16) -> ServerCandidate {
        ServerCandidate {
            protocol,
            host: host.into(),
            port: Some(port),
            security: SourceSecurity::Tls,
            auth: SourceAuth::Password,
            username: "me".into(),
        }
    }

    fn candidate(email: &str) -> AccountCandidate {
        AccountCandidate {
            source_id: email.into(),
            label: None,
            identities: vec![IdentityCandidate {
                email: email.into(),
                display_name: Some("Me".into()),
                reply_to: None,
                signature: None,
                separate_outgoing_server: false,
            }],
            incoming: Some(server(ServerProtocol::Imap, "imap.example.com", 993)),
            outgoing: Some(server(ServerProtocol::Smtp, "smtp.example.com", 465)),
            color: None,
        }
    }

    fn plan_one(candidate: AccountCandidate) -> AccountPlan {
        plan_all(vec![candidate], &[]).remove(0)
    }

    fn plan_all(accounts: Vec<AccountCandidate>, existing: &[String]) -> Vec<AccountPlan> {
        plan(
            &ImportInventory {
                source: SourceKind::K9Settings,
                source_version: None,
                accounts,
                other_entries: Vec::new(),
            },
            existing,
        )
    }

    fn codes(plan: &AccountPlan) -> Vec<FindingCode> {
        plan.findings.iter().map(|finding| finding.code).collect()
    }

    #[test]
    fn unencrypted_connections_are_never_upgraded_silently() {
        let mut account = candidate("me@example.com");
        account.incoming.as_mut().unwrap().security = SourceSecurity::None;
        account.outgoing.as_mut().unwrap().security = SourceSecurity::None;
        let plan = plan_one(account);
        assert_eq!(plan.status, PlanStatus::Unsupported);
        assert_eq!(
            codes(&plan),
            [
                FindingCode::UnencryptedIncoming,
                FindingCode::UnencryptedOutgoing
            ]
        );
        assert!(plan.draft.is_none());
    }

    #[test]
    fn a_separate_outgoing_login_blocks_import() {
        let mut account = candidate("me@example.com");
        account.outgoing.as_mut().unwrap().username = "relay-user".into();
        let plan = plan_one(account);
        assert_eq!(plan.status, PlanStatus::Unsupported);
        assert_eq!(plan.findings[0].code, FindingCode::SeparateOutgoingLogin);
        assert_eq!(plan.findings[0].detail, "relay-user");

        // Login names that differ only in ASCII case name the same mailbox.
        let mut account = candidate("me@example.com");
        account.outgoing.as_mut().unwrap().username = "ME".into();
        assert_eq!(plan_one(account).status, PlanStatus::Ready);
    }

    #[test]
    fn unsupported_sign_in_methods_are_reported_per_server() {
        for (auth, expected) in [
            (SourceAuth::ClientCertificate, "client certificate"),
            (SourceAuth::Kerberos, "Kerberos"),
            (SourceAuth::Ntlm, "NTLM"),
        ] {
            let mut account = candidate("me@example.com");
            account.incoming.as_mut().unwrap().auth = auth;
            let plan = plan_one(account);
            assert_eq!(
                plan.findings[0].code,
                FindingCode::IncomingSignInUnsupported
            );
            assert_eq!(plan.findings[0].detail, expected);
        }
        let mut account = candidate("me@example.com");
        account.outgoing.as_mut().unwrap().auth = SourceAuth::None;
        assert_eq!(
            codes(&plan_one(account)),
            [FindingCode::OutgoingSignInUnsupported]
        );
        let mut account = candidate("me@example.com");
        account.incoming.as_mut().unwrap().security = SourceSecurity::Unknown;
        assert_eq!(codes(&plan_one(account)), [FindingCode::InvalidServer]);
    }

    #[test]
    fn oauth_without_flectar_support_falls_back_to_an_app_password() {
        let mut account = candidate("me@fastmail.example");
        account.incoming.as_mut().unwrap().auth = SourceAuth::OAuth2;
        account.outgoing.as_mut().unwrap().auth = SourceAuth::OAuth2;
        let plan = plan_one(account);
        assert_eq!(plan.status, PlanStatus::Ready);
        assert_eq!(plan.sign_in, SignIn::Password);
        assert_eq!(codes(&plan), [FindingCode::OAuthReplacedByPassword]);

        // An OAuth-only outgoing server cannot share a password login.
        let mut account = candidate("me@example.com");
        account.outgoing.as_mut().unwrap().auth = SourceAuth::OAuth2;
        assert_eq!(
            codes(&plan_one(account)),
            [FindingCode::OutgoingSignInUnsupported]
        );
    }

    #[test]
    fn microsoft_accounts_use_microsoft_sign_in() {
        let mut account = candidate("me@contoso.example");
        let incoming = account.incoming.as_mut().unwrap();
        incoming.host = "Outlook.Office365.com".into();
        incoming.username = "me@contoso.example".into();
        account.outgoing.as_mut().unwrap().host = "smtp.contoso.example".into();
        let plan = plan_one(account);
        assert_eq!(plan.status, PlanStatus::Ready);
        assert_eq!(plan.sign_in, SignIn::Microsoft);
        assert_eq!(
            codes(&plan),
            [
                FindingCode::MicrosoftSignInReplacesPassword,
                FindingCode::OutgoingServerReplaced
            ]
        );
        let config = &plan.draft.unwrap().config;
        assert_eq!(config.provider, Provider::Microsoft);
        assert_eq!(config.auth_kind, AuthKind::Oauth2);
        assert_eq!(config.smtp_host, providers::MICROSOFT.smtp_host);
    }

    #[test]
    fn google_sign_in_uses_the_google_login_and_keeps_aliases_with_gmail() {
        let mut account = candidate("me@workspace.example");
        let incoming = account.incoming.as_mut().unwrap();
        incoming.host = "imap.gmail.com".into();
        incoming.auth = SourceAuth::OAuth2;
        incoming.username = "Login@Workspace.Example".into();
        account.outgoing.as_mut().unwrap().host = "smtp.gmail.com".into();
        let plan = plan_one(account);
        assert_eq!(plan.email, "Login@workspace.example");
        assert_eq!(codes(&plan), [FindingCode::AliasesManagedByProvider]);
        assert!(plan.draft.unwrap().aliases.is_empty());

        // Google retired IMAP passwords; password setups move to sign-in.
        let mut account = candidate("me@gmail.com");
        account.incoming.as_mut().unwrap().host = "imap.gmail.com".into();
        account.incoming.as_mut().unwrap().username = "me@gmail.com".into();
        account.outgoing.as_mut().unwrap().host = "smtp.gmail.com".into();
        let plan = plan_one(account);
        assert_eq!(plan.sign_in, SignIn::Google);
        assert_eq!(codes(&plan), [FindingCode::GoogleSignInReplacesPassword]);
    }

    #[test]
    fn duplicates_and_existing_accounts_are_never_overwritten() {
        let plans = plan_all(
            vec![
                candidate("me@example.com"),
                candidate("ME@example.com"),
                candidate("other@example.com"),
            ],
            &["Other@Example.com".into()],
        );
        assert_eq!(plans[0].status, PlanStatus::Ready);
        assert_eq!(plans[1].status, PlanStatus::Unsupported);
        assert_eq!(codes(&plans[1]), [FindingCode::DuplicateInSource]);
        assert_eq!(plans[2].status, PlanStatus::AlreadyAdded);
        assert!(plans[2].draft.is_none());
    }

    #[test]
    fn missing_pieces_are_named() {
        let mut account = candidate("not-an-address");
        account.incoming = None;
        assert_eq!(
            codes(&plan_one(account)),
            [
                FindingCode::MissingIncomingServer,
                FindingCode::MissingEmail
            ]
        );
        let mut account = candidate("me@example.com");
        account.outgoing = None;
        assert_eq!(
            codes(&plan_one(account)),
            [FindingCode::MissingOutgoingServer]
        );
        let mut account = candidate("me@example.com");
        account.incoming.as_mut().unwrap().username = " ".into();
        account.outgoing.as_mut().unwrap().username = String::new();
        assert_eq!(codes(&plan_one(account)), [FindingCode::MissingUsername]);
        let mut account = candidate("me@example.com");
        account.incoming.as_mut().unwrap().host = "imap example.com".into();
        assert_eq!(codes(&plan_one(account)), [FindingCode::InvalidServer]);
    }

    #[test]
    fn identities_become_aliases_with_their_own_signatures() {
        let mut account = candidate("me@example.com");
        account.identities[0].signature = Some(SignatureCandidate::Html("<p>Me</p>".into()));
        account.identities.extend([
            IdentityCandidate {
                email: "Alias@Example.com".into(),
                display_name: Some("Alias".into()),
                reply_to: Some("not an address".into()),
                signature: Some(SignatureCandidate::Text("A & B".into())),
                separate_outgoing_server: false,
            },
            IdentityCandidate {
                email: "invalid".into(),
                display_name: None,
                reply_to: None,
                signature: Some(SignatureCandidate::ExternalFile),
                separate_outgoing_server: false,
            },
            IdentityCandidate {
                email: "ME@example.com".into(),
                display_name: None,
                reply_to: None,
                signature: Some(SignatureCandidate::Html("<p>Me</p>".into())),
                separate_outgoing_server: false,
            },
        ]);
        let plan = plan_one(account);
        assert_eq!(
            codes(&plan),
            [
                FindingCode::AliasesSkipped,
                FindingCode::SignatureFileNotImported
            ]
        );
        let draft = plan.draft.unwrap();
        assert_eq!(draft.aliases.len(), 1);
        assert_eq!(draft.aliases[0].email, "Alias@example.com");
        assert_eq!(draft.aliases[0].reply_to, None);
        assert_eq!(
            draft
                .signatures
                .iter()
                .map(|signature| (signature.name.as_str(), signature.html.as_str()))
                .collect::<Vec<_>>(),
            [
                ("me@example.com", "<p>Me</p>"),
                ("Alias@Example.com", "<div>A &amp; B</div>")
            ]
        );
        assert!(draft.signatures[0].default && !draft.signatures[1].default);
    }

    #[test]
    fn an_unsupported_account_does_not_hide_a_later_one_for_the_same_address() {
        let mut pop = candidate("me@example.com");
        pop.incoming.as_mut().unwrap().protocol = ServerProtocol::Pop3;
        let plans = plan_all(vec![pop, candidate("me@example.com")], &[]);
        assert_eq!(plans[0].status, PlanStatus::Unsupported);
        assert_eq!(plans[1].status, PlanStatus::Ready);
    }

    #[test]
    fn microsoft_sign_in_keeps_the_source_default_sender() {
        let mut account = candidate("jane.doe@contoso.example");
        let incoming = account.incoming.as_mut().unwrap();
        incoming.host = "outlook.office365.com".into();
        incoming.auth = SourceAuth::OAuth2;
        incoming.username = "jdoe@contoso.onmicrosoft.example".into();
        account.outgoing.as_mut().unwrap().host = "smtp.office365.com".into();
        let plan = plan_one(account);
        assert_eq!(plan.email, "jdoe@contoso.onmicrosoft.example");
        let draft = plan.draft.unwrap();
        assert_eq!(draft.aliases[0].email, "jane.doe@contoso.example");
        assert_eq!(
            draft.config.settings.default_sender_email.as_deref(),
            Some("jane.doe@contoso.example")
        );

        // A shared mailbox login can never be the signed-in address.
        let mut account = candidate("shared@contoso.example");
        let incoming = account.incoming.as_mut().unwrap();
        incoming.host = "outlook.office365.com".into();
        incoming.auth = SourceAuth::OAuth2;
        incoming.username = "me@contoso.example\\shared@contoso.example".into();
        let plan = plan_one(account);
        assert_eq!(plan.status, PlanStatus::Unsupported);
        assert_eq!(codes(&plan), [FindingCode::OAuthLoginUnsupported]);

        // Without any identity, the login address still identifies the account.
        let mut account = candidate("me@outlook.com");
        account.identities.clear();
        let incoming = account.incoming.as_mut().unwrap();
        incoming.host = "outlook.office365.com".into();
        incoming.auth = SourceAuth::OAuth2;
        incoming.username = "me@outlook.com".into();
        account.outgoing = None;
        assert_eq!(plan_one(account).status, PlanStatus::Ready);
    }

    #[test]
    fn oversized_signatures_are_reported_and_never_become_the_default() {
        let mut account = candidate("me@example.com");
        account.identities[0].signature = Some(SignatureCandidate::Html(
            "x".repeat(MAX_SIGNATURE_BYTES + 1),
        ));
        account.identities.push(IdentityCandidate {
            email: "alias@example.com".into(),
            display_name: None,
            reply_to: None,
            signature: Some(SignatureCandidate::Text("Alias".into())),
            separate_outgoing_server: false,
        });
        let plan = plan_one(account);
        assert_eq!(codes(&plan), [FindingCode::SignatureTooLarge]);
        let draft = plan.draft.unwrap();
        assert_eq!(draft.signatures.len(), 1);
        assert!(!draft.signatures[0].default);

        let mut account = candidate("me@example.com");
        let long = "u".repeat(MAX_USERNAME_BYTES + 1);
        account.incoming.as_mut().unwrap().username = long.clone();
        account.outgoing.as_mut().unwrap().username = long;
        assert_eq!(codes(&plan_one(account)), [FindingCode::InvalidServer]);
    }

    #[test]
    fn a_signature_only_on_an_alias_is_not_the_account_default() {
        let mut account = candidate("me@example.com");
        account.identities.push(IdentityCandidate {
            email: "alias@example.com".into(),
            display_name: None,
            reply_to: None,
            signature: Some(SignatureCandidate::Text("Alias".into())),
            separate_outgoing_server: false,
        });
        let draft = plan_one(account).draft.unwrap();
        assert_eq!(draft.signatures.len(), 1);
        assert!(!draft.signatures[0].default);
    }

    #[test]
    fn addresses_settings_would_refuse_never_reach_an_importable_plan() {
        let identity =
            |email: &str, reply_to: Option<&str>, signature: Option<&str>| IdentityCandidate {
                email: email.into(),
                display_name: None,
                reply_to: reply_to.map(Into::into),
                signature: signature.map(|text| SignatureCandidate::Text(text.into())),
                separate_outgoing_server: false,
            };
        let mut account = candidate("me@example.com");
        account.identities = vec![
            identity("*@example.com", None, Some("Catch-all")),
            identity("me@example.com", None, Some("Me")),
            identity("alias@example.com", Some("*@example.com"), None),
            identity("bell\u{7}@example.com", None, None),
        ];
        let plan = plan_one(account);
        assert_eq!(plan.status, PlanStatus::Ready);
        assert_eq!(plan.email, "me@example.com");
        assert_eq!(codes(&plan), [FindingCode::AliasesSkipped]);
        assert_eq!(plan.findings[0].detail, "2");
        let draft = plan.draft.unwrap();
        assert_eq!(draft.aliases.len(), 1);
        assert_eq!(draft.aliases[0].email, "alias@example.com");
        assert_eq!(draft.aliases[0].reply_to, None);
        // The identity that gave the account its address supplies the
        // default signature.
        assert_eq!(
            draft
                .signatures
                .iter()
                .map(|signature| (signature.html.as_str(), signature.default))
                .collect::<Vec<_>>(),
            [("<div>Me</div>", true), ("<div>Catch-all</div>", false)]
        );
    }

    #[test]
    fn imported_signatures_stay_within_the_editor_limit() {
        let mut account = candidate("me@example.com");
        account.identities = (0..MAX_SIGNATURES_PER_ACCOUNT + 5)
            .map(|index| IdentityCandidate {
                email: format!("me{index}@example.com"),
                display_name: None,
                reply_to: None,
                signature: Some(SignatureCandidate::Text(format!("Signature {index}"))),
                separate_outgoing_server: false,
            })
            .collect();
        let draft = plan_one(account).draft.unwrap();
        assert_eq!(draft.signatures.len(), MAX_SIGNATURES_PER_ACCOUNT);
        assert!(draft.signatures[0].default);
        assert_eq!(draft.signatures[0].html, "<div>Signature 0</div>");
    }
}
