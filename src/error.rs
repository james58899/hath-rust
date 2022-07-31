use core::fmt;

#[derive(Debug)]
pub enum Error {
    CertExpired,
    VersionTooOld,
    ApiResponseFail { fail_code: String, message: String },
    ConnectTestFail,
    InitSettingsMissing(String),
}

impl Error {
    pub fn connection_error(message: &str) -> Error {
        Error::ApiResponseFail {
            fail_code: "CONNECTION_FAILED".to_string(),
            message: message.to_string(),
        }
    }
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::CertExpired => write!(f, "Cert expired"),
            Error::VersionTooOld => write!(f, "Your client is too old to connect to the Hentai@Home Network."),
            Error::ApiResponseFail { fail_code, message } => write!(f, "Code={}, Message={}", fail_code, message),
            Error::ConnectTestFail => write!(f, "Connect test failed"),
            Error::InitSettingsMissing(settings) => write!(f, "Missing init settings: {}", settings),
        }
    }
}
