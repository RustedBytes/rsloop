use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use rustls::client::ClientConfig;
use rustls::pki_types::ServerName;
use rustls::server::ServerConfig;
use rustls::SupportedProtocolVersion;

mod material;
use material::{
    load_cert_chain_metadata, root_store_from_context, ssl_verify_constant, to_py_tls_err,
    verify_mode_value,
};

const DEFAULT_HANDSHAKE_TIMEOUT_SECS: f64 = 60.0;
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: f64 = 30.0;

pub struct ClientTlsSettings {
    pub config: Arc<ClientConfig>,
    pub server_name: ServerName<'static>,
    pub handshake_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub ssl_context: Py<PyAny>,
}

pub struct ServerTlsSettings {
    pub config: Arc<ServerConfig>,
    pub handshake_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub ssl_context: Py<PyAny>,
}

pub fn client_tls_settings(
    py: Python<'_>,
    ssl: &Bound<'_, PyAny>,
    server_hostname: Option<&Bound<'_, PyAny>>,
    ssl_handshake_timeout: Option<f64>,
    ssl_shutdown_timeout: Option<f64>,
) -> PyResult<ClientTlsSettings> {
    let ssl_context = normalize_client_ssl_context(py, ssl)?;
    let hostname = resolve_server_hostname(py, &ssl_context, server_hostname)?;
    let config = build_client_config(py, &ssl_context)?;
    let handshake_timeout = handshake_timeout(ssl_handshake_timeout)?;
    let shutdown_timeout = shutdown_timeout(ssl_shutdown_timeout)?;

    Ok(ClientTlsSettings {
        config: Arc::new(config),
        server_name: ServerName::try_from(hostname.clone())
            .map_err(|_| PyValueError::new_err(format!("invalid server_hostname: {hostname}")))?,
        handshake_timeout,
        shutdown_timeout,
        ssl_context,
    })
}

pub fn server_tls_settings(
    py: Python<'_>,
    ssl: &Bound<'_, PyAny>,
    ssl_handshake_timeout: Option<f64>,
    ssl_shutdown_timeout: Option<f64>,
) -> PyResult<ServerTlsSettings> {
    let ssl_context = normalize_server_ssl_context(py, ssl)?;
    let config = build_server_config(py, &ssl_context)?;
    let handshake_timeout = handshake_timeout(ssl_handshake_timeout)?;
    let shutdown_timeout = shutdown_timeout(ssl_shutdown_timeout)?;

    Ok(ServerTlsSettings {
        config: Arc::new(config),
        handshake_timeout,
        shutdown_timeout,
        ssl_context,
    })
}

fn handshake_timeout(value: Option<f64>) -> PyResult<Duration> {
    let secs = value.unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_SECS);
    if !secs.is_finite() || secs <= 0.0 {
        return Err(PyValueError::new_err(
            "ssl_handshake_timeout must be a positive finite number",
        ));
    }
    Ok(Duration::from_secs_f64(secs))
}

fn shutdown_timeout(value: Option<f64>) -> PyResult<Duration> {
    let secs = value.unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
    if !secs.is_finite() || secs <= 0.0 {
        return Err(PyValueError::new_err(
            "ssl_shutdown_timeout must be a positive finite number",
        ));
    }
    Ok(Duration::from_secs_f64(secs))
}

pub fn tls_extra(
    py: Python<'_>,
    ssl_context: &Py<PyAny>,
) -> std::collections::HashMap<String, Py<PyAny>> {
    let mut extra = std::collections::HashMap::with_capacity(2);
    extra.insert(
        "sslcontext".to_owned(),
        ssl_context.clone_ref(py).into_any(),
    );
    extra.insert("ssl_object".to_owned(), py.None());
    extra
}

fn normalize_client_ssl_context(py: Python<'_>, ssl: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if ssl.is_none() {
        return Err(PyTypeError::new_err("ssl must not be None"));
    }
    if let Ok(enabled) = ssl.extract::<bool>() {
        if !enabled {
            return Err(PyTypeError::new_err("ssl=False does not enable TLS"));
        }
        let ssl_mod = py.import("ssl")?;
        let ctx = ssl_mod.getattr("create_default_context")?.call0()?;
        return Ok(ctx.unbind());
    }
    ensure_ssl_context(py, ssl)?;
    Ok(ssl.clone().unbind())
}

fn normalize_server_ssl_context(py: Python<'_>, ssl: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if ssl.is_none() {
        return Err(PyTypeError::new_err("ssl must not be None"));
    }
    if ssl.extract::<bool>().unwrap_or(false) {
        return Err(PyTypeError::new_err(
            "server TLS requires an ssl.SSLContext instance",
        ));
    }
    ensure_ssl_context(py, ssl)?;
    Ok(ssl.clone().unbind())
}

fn ensure_ssl_context(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<()> {
    let ssl_mod = py.import("ssl")?;
    let cls = ssl_mod.getattr("SSLContext")?;
    if value.is_instance(&cls)? {
        return Ok(());
    }
    Err(PyTypeError::new_err(
        "ssl must be True or an ssl.SSLContext instance",
    ))
}

fn resolve_server_hostname(
    py: Python<'_>,
    ssl_context: &Py<PyAny>,
    server_hostname: Option<&Bound<'_, PyAny>>,
) -> PyResult<String> {
    if let Some(server_hostname) = server_hostname {
        if server_hostname.is_none() {
            return Err(PyValueError::new_err(
                "server_hostname cannot be None when TLS is enabled",
            ));
        }
        return server_hostname.extract::<String>();
    }

    let check_hostname = ssl_context
        .getattr(py, "check_hostname")?
        .extract::<bool>(py)?;
    if check_hostname {
        return Err(PyValueError::new_err(
            "server_hostname is required when TLS hostname checks are enabled",
        ));
    }

    Ok("localhost".to_owned())
}

fn build_client_config(py: Python<'_>, ssl_context: &Py<PyAny>) -> PyResult<ClientConfig> {
    let roots = root_store_from_context(py, ssl_context)?;
    let maybe_identity = load_cert_chain_metadata(py, ssl_context)?;

    let builder = ClientConfig::builder_with_protocol_versions(default_protocol_versions());
    let mut config = match maybe_identity {
        Some((certs, key)) => builder
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)
            .map_err(to_py_tls_err)?,
        None => builder.with_root_certificates(roots).with_no_client_auth(),
    };
    config.enable_sni = true;
    Ok(config)
}

fn build_server_config(py: Python<'_>, ssl_context: &Py<PyAny>) -> PyResult<ServerConfig> {
    let (certs, key) = load_cert_chain_metadata(py, ssl_context)?.ok_or_else(|| {
        PyRuntimeError::new_err(
            "server SSLContext is missing tracked certificate metadata; call SSLContext.load_cert_chain() after importing rsloop",
        )
    })?;
    let verify_mode = verify_mode_value(py, ssl_context)?;
    let cert_required = ssl_verify_constant(py, "CERT_REQUIRED")?;

    let builder = ServerConfig::builder_with_protocol_versions(default_protocol_versions());
    let mut config = if verify_mode == cert_required {
        let roots = root_store_from_context(py, ssl_context)?;
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(to_py_tls_err)?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(to_py_tls_err)?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(to_py_tls_err)?
    };
    config.alpn_protocols = vec![];
    Ok(config)
}

fn default_protocol_versions() -> &'static [&'static SupportedProtocolVersion] {
    rustls::DEFAULT_VERSIONS
}
