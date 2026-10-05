//! Shared handling of the `ssl` / `server_hostname` / timeout keyword group.
//!
//! Every `asyncio` loop method that accepts TLS options repeats the same three
//! "only meaningful with ssl" checks before converting the Python `SSLContext`
//! into Rust TLS settings. Collecting the group here keeps those validations —
//! and their order, which callers observe through the raised `ValueError` — in
//! one place.

use std::sync::Arc;

use pyo3::{exceptions::PyValueError, prelude::*};

use crate::transport::tls::{
    ClientTlsSettings, ServerTlsSettings, client_tls_settings, server_tls_settings,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TlsValidationError {
    ServerHostname,
    HandshakeTimeout,
    ShutdownTimeout,
}

#[derive(Clone, Copy, Debug)]
struct TlsValidationInputs {
    has_ssl: bool,
    has_server_hostname: bool,
    has_handshake_timeout: bool,
    has_shutdown_timeout: bool,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn tls_validation_error(inputs: TlsValidationInputs) -> Option<TlsValidationError> {
    if inputs.has_ssl {
        None
    } else if inputs.has_server_hostname {
        Some(TlsValidationError::ServerHostname)
    } else if inputs.has_handshake_timeout {
        Some(TlsValidationError::HandshakeTimeout)
    } else if inputs.has_shutdown_timeout {
        Some(TlsValidationError::ShutdownTimeout)
    } else {
        None
    }
}

/// The TLS keyword group as it arrives from Python.
pub(super) struct TlsParams {
    pub(super) ssl: Option<Py<PyAny>>,
    pub(super) server_hostname: Option<Py<PyAny>>,
    pub(super) handshake_timeout: Option<f64>,
    pub(super) shutdown_timeout: Option<f64>,
}

impl TlsParams {
    #[cfg_attr(feature = "hotpath-profile", hotpath::measure(impl_type = "TlsParams"))]
    /// Options for a method that has no `server_hostname` parameter.
    pub(super) fn without_hostname(
        ssl: Option<Py<PyAny>>,
        handshake_timeout: Option<f64>,
        shutdown_timeout: Option<f64>,
    ) -> Self {
        Self {
            ssl,
            server_hostname: None,
            handshake_timeout,
            shutdown_timeout,
        }
    }

    #[cfg_attr(feature = "hotpath-profile", hotpath::measure(impl_type = "TlsParams"))]
    pub(super) fn is_enabled(&self) -> bool {
        self.ssl.is_some()
    }

    #[cfg_attr(feature = "hotpath-profile", hotpath::measure(impl_type = "TlsParams"))]
    /// Rejects TLS-only keywords passed without `ssl`. The check order matches
    /// what the individual methods used to do, so the reported error for a call
    /// that misuses several keywords at once does not change.
    pub(super) fn validate(&self) -> PyResult<()> {
        let error = tls_validation_error(TlsValidationInputs {
            has_ssl: self.ssl.is_some(),
            has_server_hostname: self.server_hostname.is_some(),
            has_handshake_timeout: self.handshake_timeout.is_some(),
            has_shutdown_timeout: self.shutdown_timeout.is_some(),
        });
        match error {
            None => Ok(()),
            Some(TlsValidationError::ServerHostname) => Err(PyValueError::new_err(
                "server_hostname is only meaningful with ssl",
            )),
            Some(TlsValidationError::HandshakeTimeout) => Err(PyValueError::new_err(
                "ssl_handshake_timeout is only meaningful with ssl",
            )),
            Some(TlsValidationError::ShutdownTimeout) => Err(PyValueError::new_err(
                "ssl_shutdown_timeout is only meaningful with ssl",
            )),
        }
    }

    #[cfg_attr(feature = "hotpath-profile", hotpath::measure(impl_type = "TlsParams"))]
    /// Client-side settings, or `None` when the caller passed no `ssl`.
    pub(super) fn client_settings(&self, py: Python<'_>) -> PyResult<Option<ClientTlsSettings>> {
        let Some(ssl) = self.ssl.as_ref() else {
            return Ok(None);
        };
        client_tls_settings(
            py,
            ssl.bind(py),
            self.server_hostname.as_ref().map(|value| value.bind(py)),
            self.handshake_timeout,
            self.shutdown_timeout,
        )
        .map(Some)
    }

    #[cfg_attr(feature = "hotpath-profile", hotpath::measure(impl_type = "TlsParams"))]
    /// Server-side settings, or `None` when the caller passed no `ssl`.
    pub(super) fn server_settings(&self, py: Python<'_>) -> PyResult<Option<ServerTlsSettings>> {
        let Some(ssl) = self.ssl.as_ref() else {
            return Ok(None);
        };
        server_tls_settings(
            py,
            ssl.bind(py),
            self.handshake_timeout,
            self.shutdown_timeout,
        )
        .map(Some)
    }

    #[cfg_attr(feature = "hotpath-profile", hotpath::measure(impl_type = "TlsParams"))]
    /// Server settings shared with the accept tasks that outlive this call.
    pub(super) fn shared_server_settings(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Arc<ServerTlsSettings>>> {
        Ok(self.server_settings(py)?.map(Arc::new))
    }
}

#[cfg(kani)]
mod verification {
    use super::{TlsValidationError, TlsValidationInputs, tls_validation_error};

    #[kani::proof]
    fn merge_tls_validation_preserves_error_precedence() {
        let ssl: bool = kani::any();
        let hostname: bool = kani::any();
        let handshake: bool = kani::any();
        let shutdown: bool = kani::any();

        let error = tls_validation_error(TlsValidationInputs {
            has_ssl: ssl,
            has_server_hostname: hostname,
            has_handshake_timeout: handshake,
            has_shutdown_timeout: shutdown,
        });
        let expected = if ssl {
            None
        } else if hostname {
            Some(TlsValidationError::ServerHostname)
        } else if handshake {
            Some(TlsValidationError::HandshakeTimeout)
        } else if shutdown {
            Some(TlsValidationError::ShutdownTimeout)
        } else {
            None
        };
        assert_eq!(error, expected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validation_error(params: &TlsParams) -> String {
        params
            .validate()
            .expect_err("TLS-only keyword should fail")
            .to_string()
    }

    #[test]
    fn tls_only_keyword_validation_preserves_observable_error_order() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let hostname = TlsParams {
                ssl: None,
                server_hostname: Some(py.None()),
                handshake_timeout: Some(1.0),
                shutdown_timeout: Some(2.0),
            };
            assert!(validation_error(&hostname).contains("server_hostname"));

            let handshake = TlsParams {
                ssl: None,
                server_hostname: None,
                handshake_timeout: Some(1.0),
                shutdown_timeout: Some(2.0),
            };
            assert!(validation_error(&handshake).contains("ssl_handshake_timeout"));

            let shutdown = TlsParams::without_hostname(None, None, Some(2.0));
            assert!(validation_error(&shutdown).contains("ssl_shutdown_timeout"));
        });
    }

    #[test]
    fn disabled_tls_has_no_settings_and_enabled_tls_accepts_related_keywords() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let disabled = TlsParams::without_hostname(None, None, None);
            assert!(!disabled.is_enabled());
            disabled.validate().expect("plain transport options");
            assert!(
                disabled
                    .client_settings(py)
                    .expect("client settings")
                    .is_none()
            );
            assert!(
                disabled
                    .server_settings(py)
                    .expect("server settings")
                    .is_none()
            );
            assert!(
                disabled
                    .shared_server_settings(py)
                    .expect("shared settings")
                    .is_none()
            );

            let enabled = TlsParams {
                ssl: Some(py.None()),
                server_hostname: Some(py.None()),
                handshake_timeout: Some(1.0),
                shutdown_timeout: Some(2.0),
            };
            assert!(enabled.is_enabled());
            enabled.validate().expect("TLS keywords with ssl");
        });
    }
}
