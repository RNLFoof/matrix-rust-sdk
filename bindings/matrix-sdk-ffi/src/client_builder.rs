use std::{fs, path::PathBuf, sync::Arc};

use futures_util::StreamExt;
use matrix_sdk::{
    authentication::qrcode::{self, messages::LoginFailureReason, DeviceCodeErrorResponseType},
    crypto::types::qr_login::{LoginQrCodeDecodeError, QrCodeModeData},
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    reqwest::Certificate,
    ruma::{
        api::{error::UnknownVersionError, MatrixVersion},
        ServerName, UserId,
    },
    Client as MatrixClient, ClientBuildError as MatrixClientBuildError,
    ClientBuilder as MatrixClientBuilder, IdParseError,
};
use sanitize_filename_reader_friendly::sanitize;
use url::Url;
use zeroize::Zeroizing;

use super::{client::Client, RUNTIME};
use crate::{
    authentication_service::OidcConfiguration, client::ClientSessionDelegate, error::ClientError,
    helpers::unwrap_or_clone_arc, task_handle::TaskHandle,
};

/// A list of bytes containing a certificate in DER or PEM form.
pub type CertificateBytes = Vec<u8>;

#[derive(Debug, Clone)]
enum HomeserverConfig {
    Url(String),
    ServerName(String),
    ServerNameOrUrl(String),
}

#[derive(Debug, uniffi::Object)]
pub struct QrCodeData {
    inner: qrcode::QrCodeData,
}

#[uniffi::export]
impl QrCodeData {
    #[uniffi::constructor]
    pub fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>, QrCodeDecodeError> {
        Ok(Self { inner: qrcode::QrCodeData::from_bytes(bytes)? }.into())
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum QrCodeDecodeError {
    #[error("Error decoding QR code: {error:?}")]
    Crypto {
        #[from]
        error: LoginQrCodeDecodeError,
    },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum QrLoginError {
    #[error(transparent)]
    ClientBuilder {
        #[from]
        error: ClientBuildError,
    },
    #[error("The homeserver doesn't provide a sliding sync proxy in its configuration.")]
    SlidingSyncNotAvailable,
    #[error("Unable to use OIDC as the supplied client metadata is invalid.")]
    OidcMetadataInvalid,
    #[error("The per-user directory could not have been created: {reason}")]
    StorageSetup { reason: String },
    #[error("The scanned and parsed QR code has an invalid intent, expected the reciprocate intent, got the login intent")]
    InvalidIntent,
    #[error(transparent)]
    LoginError {
        #[from]
        error: qrcode::QRCodeLoginError,
    },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum HumanQrLoginError {
    #[error("Linking with this device is not supported.")]
    LinkingNotSupported,
    #[error("The sign in was cancelled.")]
    Cancelled,
    #[error("The sign in was not completed in the required time.")]
    Expired,
    #[error("A secure connection could not have been established between the two device.")]
    ConnectionInsecure,
    #[error("The sign in was declined.")]
    Declined,
    #[error("An unknown error has happened.")]
    Unknown,
    #[error("The QR code we scanned was not valid.")]
    InvalidQrCode,
}

impl From<std::io::Error> for QrLoginError {
    fn from(value: std::io::Error) -> Self {
        Self::StorageSetup { reason: format!("{value:?}") }
    }
}

impl From<QrLoginError> for HumanQrLoginError {
    fn from(value: QrLoginError) -> Self {
        match value {
            QrLoginError::LoginError { error } => error.into(),
            QrLoginError::InvalidIntent => HumanQrLoginError::InvalidQrCode,
            QrLoginError::ClientBuilder { .. }
            | QrLoginError::SlidingSyncNotAvailable
            | QrLoginError::OidcMetadataInvalid
            | QrLoginError::StorageSetup { .. } => HumanQrLoginError::Unknown,
        }
    }
}

impl From<matrix_sdk::authentication::qrcode::QRCodeLoginError> for HumanQrLoginError {
    fn from(value: qrcode::QRCodeLoginError) -> Self {
        use qrcode::QRCodeLoginError;

        match value {
            QRCodeLoginError::LoginFailure { reason, .. } => match reason {
                LoginFailureReason::UnsupportedProtocol => HumanQrLoginError::LinkingNotSupported,
                LoginFailureReason::AuthorizationExpired => HumanQrLoginError::Expired,
                LoginFailureReason::UserCancelled => HumanQrLoginError::Cancelled,
                _ => HumanQrLoginError::Unknown,
            },
            QRCodeLoginError::Oidc(e) => {
                if let Some(e) = e.as_request_token_error() {
                    match e {
                        DeviceCodeErrorResponseType::AccessDenied => HumanQrLoginError::Declined,
                        DeviceCodeErrorResponseType::ExpiredToken => HumanQrLoginError::Expired,
                        _ => HumanQrLoginError::Unknown,
                    }
                } else {
                    HumanQrLoginError::Unknown
                }
            }
            QRCodeLoginError::SecureChannel(_) => HumanQrLoginError::ConnectionInsecure,
            QRCodeLoginError::UnexpectedMessage { .. }
            | QRCodeLoginError::CrossProcessRefreshLock(_)
            | QRCodeLoginError::DeviceKeyUpload(_)
            | QRCodeLoginError::SessionTokens(_)
            | QRCodeLoginError::UserIdDiscovery(_)
            | QRCodeLoginError::SecretImport(_) => HumanQrLoginError::Unknown,
        }
    }
}

/// Convert the error the [`ClientBuilder::build_with_qr_code()`] method returns
/// into a more user-friendly error type.
#[uniffi::export]
pub fn qr_login_error_to_human_error(error: QrLoginError) -> HumanQrLoginError {
    error.into()
}

/// Enum describing the progress of the QR-code login.
#[derive(Debug, Default, Clone, uniffi::Enum)]
pub enum QrLoginProgress {
    /// The login process is starting.
    #[default]
    Starting,
    /// We established a secure channel with the other device.
    EstablishingSecureChannel {
        /// The check code that the device should display so the other device
        /// can confirm that the channel is secure as well.
        check_code: u8,
        /// The string representation of the check code, will be guaranteed to
        /// be 2 characters long, preserving the leading zero if the
        /// first digit is a zero.
        check_code_string: String,
    },
    /// We are waiting for the login and for the OIDC provider to give us an
    /// access token.
    WaitingForToken { user_code: String },
    /// The login has successfully finished.
    Done,
}

#[uniffi::export(callback_interface)]
pub trait QrLoginProgressListener: Sync + Send {
    fn on_update(&self, state: QrLoginProgress);
}

impl From<qrcode::LoginProgress> for QrLoginProgress {
    fn from(value: qrcode::LoginProgress) -> Self {
        use matrix_sdk::authentication::qrcode::LoginProgress;
        match value {
            LoginProgress::Starting => Self::Starting,
            LoginProgress::EstablishingSecureChannel { check_code } => {
                let check_code = check_code.to_digit();

                Self::EstablishingSecureChannel {
                    check_code,
                    check_code_string: format!("{check_code:02}"),
                }
            }
            LoginProgress::WaitingForToken { user_code } => Self::WaitingForToken { user_code },
            LoginProgress::Done => Self::Done,
        }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum ClientBuildError {
    #[error(transparent)]
    Sdk(#[from] MatrixClientBuildError),
    #[error("Failed to build the client: {message}")]
    Generic { message: String },
}

impl From<IdParseError> for ClientBuildError {
    fn from(e: IdParseError) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

impl From<std::io::Error> for ClientBuildError {
    fn from(e: std::io::Error) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

impl From<url::ParseError> for ClientBuildError {
    fn from(e: url::ParseError) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

impl From<ClientError> for ClientBuildError {
    fn from(e: ClientError) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

#[derive(Clone, uniffi::Object)]
pub struct ClientBuilder {
    base_path: Option<String>,
    username: Option<String>,
    homeserver_cfg: Option<HomeserverConfig>,
    server_versions: Option<Vec<String>>,
    passphrase: Zeroizing<Option<String>>,
    user_agent: Option<String>,
    sliding_sync_proxy: Option<String>,
    proxy: Option<String>,
    disable_ssl_verification: bool,
    disable_automatic_token_refresh: bool,
    inner: MatrixClientBuilder,
    cross_process_refresh_lock_id: Option<String>,
    session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    additional_root_certificates: Vec<Vec<u8>>,
    encryption_settings: EncryptionSettings,
}

#[uniffi::export(async_runtime = "tokio")]
impl ClientBuilder {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base_path: None,
            username: None,
            homeserver_cfg: None,
            server_versions: None,
            passphrase: Zeroizing::new(None),
            user_agent: None,
            sliding_sync_proxy: None,
            proxy: None,
            disable_ssl_verification: false,
            disable_automatic_token_refresh: false,
            inner: MatrixClient::builder(),
            cross_process_refresh_lock_id: None,
            session_delegate: None,
            additional_root_certificates: Default::default(),
            encryption_settings: EncryptionSettings {
                auto_enable_cross_signing: false,
                backup_download_strategy:
                    matrix_sdk::encryption::BackupDownloadStrategy::AfterDecryptionFailure,
                auto_enable_backups: false,
            },
        })
    }

    pub fn enable_cross_process_refresh_lock(
        self: Arc<Self>,
        process_id: String,
        session_delegate: Box<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        self.enable_cross_process_refresh_lock_inner(process_id, session_delegate.into())
    }

    pub fn set_session_delegate(
        self: Arc<Self>,
        session_delegate: Box<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_delegate = Some(session_delegate.into());
        Arc::new(builder)
    }

    pub fn base_path(self: Arc<Self>, path: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.base_path = Some(path);
        Arc::new(builder)
    }

    pub fn username(self: Arc<Self>, username: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.username = Some(username);
        Arc::new(builder)
    }

    pub fn server_versions(self: Arc<Self>, versions: Vec<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.server_versions = Some(versions);
        Arc::new(builder)
    }

    pub fn server_name(self: Arc<Self>, server_name: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_cfg = Some(HomeserverConfig::ServerName(server_name));
        Arc::new(builder)
    }

    pub fn homeserver_url(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_cfg = Some(HomeserverConfig::Url(url));
        Arc::new(builder)
    }

    pub fn server_name_or_homeserver_url(self: Arc<Self>, server_name_or_url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_cfg = Some(HomeserverConfig::ServerNameOrUrl(server_name_or_url));
        Arc::new(builder)
    }

    pub fn passphrase(self: Arc<Self>, passphrase: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.passphrase = Zeroizing::new(passphrase);
        Arc::new(builder)
    }

    pub fn user_agent(self: Arc<Self>, user_agent: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.user_agent = Some(user_agent);
        Arc::new(builder)
    }

    pub fn sliding_sync_proxy(self: Arc<Self>, sliding_sync_proxy: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.sliding_sync_proxy = sliding_sync_proxy;
        Arc::new(builder)
    }

    pub fn proxy(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.proxy = Some(url);
        Arc::new(builder)
    }

    pub fn disable_ssl_verification(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.disable_ssl_verification = true;
        Arc::new(builder)
    }

    pub fn disable_automatic_token_refresh(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.disable_automatic_token_refresh = true;
        Arc::new(builder)
    }

    pub fn add_root_certificates(
        self: Arc<Self>,
        certificates: Vec<CertificateBytes>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.additional_root_certificates = certificates;

        Arc::new(builder)
    }

    pub fn auto_enable_cross_signing(
        self: Arc<Self>,
        auto_enable_cross_signing: bool,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.encryption_settings.auto_enable_cross_signing = auto_enable_cross_signing;
        Arc::new(builder)
    }

    /// Select a strategy to download room keys from the backup. By default
    /// we download after a decryption failure.
    ///
    /// Take a look at the [`BackupDownloadStrategy`] enum for more options.
    pub fn backup_download_strategy(
        self: Arc<Self>,
        backup_download_strategy: BackupDownloadStrategy,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.encryption_settings.backup_download_strategy = backup_download_strategy;
        Arc::new(builder)
    }

    /// Automatically create a backup version if no backup exists.
    pub fn auto_enable_backups(self: Arc<Self>, auto_enable_backups: bool) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.encryption_settings.auto_enable_backups = auto_enable_backups;
        Arc::new(builder)
    }

    pub async fn build(self: Arc<Self>) -> Result<Arc<Client>, ClientBuildError> {
        Ok(Arc::new(self.build_inner().await?))
    }

    /// Finish the building of the client and attempt to log in using the
    /// provided [`QrCodeData`].
    pub async fn build_with_qr_code(
        self: Arc<Self>,
        qr_code_data: &QrCodeData,
        oidc_configuration: &OidcConfiguration,
        progress_listener: Box<dyn QrLoginProgressListener>,
    ) -> Result<Arc<Client>, QrLoginError> {
        if let QrCodeModeData::Reciprocate { homeserver_url } = &qr_code_data.inner.mode_data {
            let mut builder = self.server_name_or_homeserver_url(homeserver_url.to_string());
            let uuid = uuid::Uuid::new_v4().to_string();

            // If a base directory was configured, create a random subdirectory, we will
            // rename this directory later on.
            let directories = if let Some(base_path) = &builder.base_path {
                let base_directory = PathBuf::from(base_path);
                let random_dir = base_directory.join(&uuid);

                tracing::debug!("Setting the Client storage path to {random_dir:?}");

                builder = builder.base_path(
                    random_dir
                        .to_str()
                        .expect("The base path and the uuid both are valid UTF-8 strings")
                        .to_string(),
                );

                Some((base_directory, random_dir))
            } else {
                None
            };

            let client = builder.build().await?;

            if client.sliding_sync_proxy().is_none() {
                return Err(QrLoginError::SlidingSyncNotAvailable);
            }

            let client_metadata =
                oidc_configuration.try_into().map_err(|_| QrLoginError::OidcMetadataInvalid)?;

            let oidc = client.inner.oidc();
            let login = oidc.login_with_qr_code(&qr_code_data.inner, client_metadata);

            let mut progress = login.subscribe_to_progress();

            let _progress_task = TaskHandle::new(RUNTIME.spawn(async move {
                while let Some(state) = progress.next().await {
                    progress_listener.on_update(state.into());
                }
            }));

            if let Err(e) = login.await {
                if let Some((_, random_dir)) = directories {
                    if let Err(e) = fs::remove_dir_all(random_dir) {
                        tracing::warn!(
                            "Couldn't clean up the random directory after a login failure: {e:?}"
                        )
                    }
                }

                return Err(e.into());
            }

            // Clients want to scope the per-client directory by the user ID, but the user
            // ID is only available once we logged in. So rename the uuid based
            // directory into the user name's directory.
            if let Some((base_directory, random_dir)) = directories {
                let user_id = client
                    .user_id()
                    .expect("Since the login above succeeded, we should know our user id now.");
                let user_dir = base_directory.join(sanitize(&user_id));

                tracing::debug!(
                    "Renaming the Client storage path from {random_dir:?} to {user_dir:?}"
                );

                fs::rename(random_dir, user_dir)?;
            }

            Ok(client)
        } else {
            Err(QrLoginError::InvalidIntent)
        }
    }
}

impl ClientBuilder {
    pub(crate) fn enable_cross_process_refresh_lock_inner(
        self: Arc<Self>,
        process_id: String,
        session_delegate: Arc<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.cross_process_refresh_lock_id = Some(process_id);
        builder.session_delegate = Some(session_delegate);
        Arc::new(builder)
    }

    pub(crate) fn set_session_delegate_inner(
        self: Arc<Self>,
        session_delegate: Arc<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_delegate = Some(session_delegate);
        Arc::new(builder)
    }

    pub(crate) async fn build_inner(self: Arc<Self>) -> Result<Client, ClientBuildError> {
        let builder = unwrap_or_clone_arc(self);
        let mut inner_builder = builder.inner;

        if let (Some(base_path), Some(username)) = (&builder.base_path, &builder.username) {
            // Determine store path
            let data_path = PathBuf::from(base_path).join(sanitize(username));
            fs::create_dir_all(&data_path)?;

            inner_builder = inner_builder.sqlite_store(&data_path, builder.passphrase.as_deref());
        } else if let Some(base_path) = &builder.base_path {
            let data_path = PathBuf::from(base_path);
            fs::create_dir_all(&data_path)?;

            inner_builder = inner_builder.sqlite_store(&data_path, builder.passphrase.as_deref());
        }

        // Determine server either from URL, server name or user ID.
        inner_builder = match builder.homeserver_cfg {
            Some(HomeserverConfig::Url(url)) => inner_builder.homeserver_url(url),
            Some(HomeserverConfig::ServerName(server_name)) => {
                let server_name = ServerName::parse(server_name)?;
                inner_builder.server_name(&server_name)
            }
            Some(HomeserverConfig::ServerNameOrUrl(server_name_or_url)) => {
                inner_builder.server_name_or_homeserver_url(server_name_or_url)
            }
            None => {
                if let Some(username) = builder.username {
                    let user = UserId::parse(username)?;
                    inner_builder.server_name(user.server_name())
                } else {
                    return Err(ClientBuildError::Generic {
                        message: "Failed to build: One of homeserver_url, server_name, server_name_or_homeserver_url or username must be called.".to_owned(),
                    });
                }
            }
        };

        let mut certificates = Vec::new();

        for certificate in builder.additional_root_certificates {
            // We don't really know what type of certificate we may get here, so let's try
            // first one type, then the other.
            if let Ok(cert) = Certificate::from_der(&certificate) {
                certificates.push(cert);
            } else {
                let cert =
                    Certificate::from_pem(&certificate).map_err(|e| ClientBuildError::Generic {
                        message: format!("Failed to add a root certificate {e:?}"),
                    })?;
                certificates.push(cert);
            }
        }

        inner_builder = inner_builder.add_root_certificates(certificates);

        if let Some(proxy) = builder.proxy {
            inner_builder = inner_builder.proxy(proxy);
        }

        if builder.disable_ssl_verification {
            inner_builder = inner_builder.disable_ssl_verification();
        }

        if !builder.disable_automatic_token_refresh {
            inner_builder = inner_builder.handle_refresh_tokens();
        }

        if let Some(user_agent) = builder.user_agent {
            inner_builder = inner_builder.user_agent(user_agent);
        }

        if let Some(server_versions) = builder.server_versions {
            inner_builder = inner_builder.server_versions(
                server_versions
                    .iter()
                    .map(|s| MatrixVersion::try_from(s.as_str()))
                    .collect::<Result<Vec<MatrixVersion>, UnknownVersionError>>()
                    .map_err(|e| ClientBuildError::Generic { message: e.to_string() })?,
            );
        }

        inner_builder = inner_builder.with_encryption_settings(builder.encryption_settings);

        let sdk_client = inner_builder.build().await?;

        // At this point, `sdk_client` might contain a `sliding_sync_proxy` that has
        // been configured by the homeserver (if it's a `ServerName` and the
        // `.well-known` file is filled as expected).
        //
        // If `builder.sliding_sync_proxy` contains `Some(_)`, it means one wants to
        // overwrite this value. It would be an error to call
        // `sdk_client.set_sliding_sync_proxy()` with `None`, as it would erase the
        // `sliding_sync_proxy` if any, and it's not the intended behavior.
        //
        // So let's call `sdk_client.set_sliding_sync_proxy()` if and only if there is
        // `Some(_)` value in `builder.sliding_sync_proxy`. That's really important: It
        // might not break an existing app session, but it is likely to break a new
        // session, which not immediate to detect if there is no test.
        if let Some(sliding_sync_proxy) = builder.sliding_sync_proxy {
            sdk_client.set_sliding_sync_proxy(Some(Url::parse(&sliding_sync_proxy)?));
        }

        Ok(Client::new(sdk_client, builder.cross_process_refresh_lock_id, builder.session_delegate)
            .await?)
    }
}
