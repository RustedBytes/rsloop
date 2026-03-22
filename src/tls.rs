use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Cursor};
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use rustls::client::ClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConfig;
use rustls::{RootCertStore, SupportedProtocolVersion};

const DEFAULT_HANDSHAKE_TIMEOUT_SECS: f64 = 60.0;

pub struct ClientTlsSettings {
    pub config: Arc<ClientConfig>,
    pub server_name: ServerName<'static>,
    pub handshake_timeout: Duration,
    pub extra: HashMap<String, Py<PyAny>>,
}

pub struct ServerTlsSettings {
    pub config: Arc<ServerConfig>,
    pub handshake_timeout: Duration,
    pub extra: HashMap<String, Py<PyAny>>,
}

pub fn client_tls_settings(
    py: Python<'_>,
    ssl: &Bound<'_, PyAny>,
    server_hostname: Option<&Bound<'_, PyAny>>,
    ssl_handshake_timeout: Option<f64>,
) -> PyResult<ClientTlsSettings> {
    let ssl_context = normalize_client_ssl_context(py, ssl)?;
    let hostname = resolve_server_hostname(py, &ssl_context, server_hostname)?;
    let config = build_client_config(py, &ssl_context)?;
    let handshake_timeout = handshake_timeout(ssl_handshake_timeout)?;
    let extra = tls_extra(py, &ssl_context)?;

    Ok(ClientTlsSettings {
        config: Arc::new(config),
        server_name: ServerName::try_from(hostname.clone())
            .map_err(|_| PyValueError::new_err(format!("invalid server_hostname: {hostname}")))?,
        handshake_timeout,
        extra,
    })
}

pub fn server_tls_settings(
    py: Python<'_>,
    ssl: &Bound<'_, PyAny>,
    ssl_handshake_timeout: Option<f64>,
) -> PyResult<ServerTlsSettings> {
    let ssl_context = normalize_server_ssl_context(py, ssl)?;
    let config = build_server_config(py, &ssl_context)?;
    let handshake_timeout = handshake_timeout(ssl_handshake_timeout)?;
    let extra = tls_extra(py, &ssl_context)?;

    Ok(ServerTlsSettings {
        config: Arc::new(config),
        handshake_timeout,
        extra,
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

fn tls_extra(py: Python<'_>, ssl_context: &Py<PyAny>) -> PyResult<HashMap<String, Py<PyAny>>> {
    let mut extra = HashMap::with_capacity(2);
    extra.insert(
        "sslcontext".to_owned(),
        ssl_context.clone_ref(py).into_any(),
    );
    extra.insert("ssl_object".to_owned(), py.None());
    Ok(extra)
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

    let check_hostname = ssl_context.getattr(py, "check_hostname")?.extract::<bool>(py)?;
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

fn root_store_from_context(py: Python<'_>, ssl_context: &Py<PyAny>) -> PyResult<RootCertStore> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("binary_form", true)?;
    let certs = ssl_context.call_method(py, "get_ca_certs", (), Some(&kwargs))?;
    let mut roots = RootCertStore::empty();
    for cert in certs.bind(py).try_iter()? {
        let cert = cert?;
        let bytes = cert.cast::<PyBytes>()?;
        roots
            .add(CertificateDer::from(bytes.as_bytes().to_vec()))
            .map_err(to_py_tls_err)?;
    }
    Ok(roots)
}

fn load_cert_chain_metadata(
    py: Python<'_>,
    ssl_context: &Py<PyAny>,
) -> PyResult<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>> {
    let dict_obj = ssl_context.bind(py).getattr("__dict__")?;
    let dict = dict_obj.cast::<PyDict>()?;
    let Some(certfile) = dict.get_item("_rsloop_certfile")? else {
        return Ok(None);
    };
    let keyfile = dict
        .get_item("_rsloop_keyfile")?
        .unwrap_or_else(|| certfile.clone());
    let password = dict.get_item("_rsloop_key_password")?;
    let certfile = certfile.extract::<String>()?;
    let keyfile = keyfile.extract::<String>()?;
    let password = password
        .filter(|value| !value.is_none())
        .map(|value| value.extract::<Vec<u8>>())
        .transpose()?;
    load_pem_identity(&certfile, &keyfile, password.as_deref())
}

fn load_pem_identity(
    certfile: &str,
    keyfile: &str,
    password: Option<&[u8]>,
) -> PyResult<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>> {
    if password.is_some() {
        return Err(PyRuntimeError::new_err(
            "encrypted private keys are not supported by the rustls backend yet",
        ));
    }

    let cert_data = fs::read(certfile).map_err(io_err_to_py)?;
    let key_data = fs::read(keyfile).map_err(io_err_to_py)?;

    let mut cert_reader = BufReader::new(Cursor::new(cert_data));
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_err_to_py)?;
    if certs.is_empty() {
        return Err(PyRuntimeError::new_err("certificate chain is empty"));
    }

    let key = load_private_key(key_data)?;
    Ok(Some((certs, key)))
}

fn load_private_key(key_data: Vec<u8>) -> PyResult<PrivateKeyDer<'static>> {
    let mut pkcs8_reader = BufReader::new(Cursor::new(key_data.clone()));
    if let Some(key) = rustls_pemfile::pkcs8_private_keys(&mut pkcs8_reader)
        .next()
        .transpose()
        .map_err(io_err_to_py)?
    {
        return Ok(PrivateKeyDer::from(key));
    }

    let mut rsa_reader = BufReader::new(Cursor::new(key_data.clone()));
    if let Some(key) = rustls_pemfile::rsa_private_keys(&mut rsa_reader)
        .next()
        .transpose()
        .map_err(io_err_to_py)?
    {
        return Ok(PrivateKeyDer::from(key));
    }

    let mut sec1_reader = BufReader::new(Cursor::new(key_data));
    if let Some(key) = rustls_pemfile::ec_private_keys(&mut sec1_reader)
        .next()
        .transpose()
        .map_err(io_err_to_py)?
    {
        return Ok(PrivateKeyDer::from(key));
    }

    Err(PyRuntimeError::new_err("no supported private key found"))
}

fn verify_mode_value(py: Python<'_>, ssl_context: &Py<PyAny>) -> PyResult<i32> {
    ssl_context.getattr(py, "verify_mode")?.extract::<i32>(py)
}

fn ssl_verify_constant(py: Python<'_>, name: &str) -> PyResult<i32> {
    py.import("ssl")?.getattr(name)?.extract::<i32>()
}

fn io_err_to_py(err: io::Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

fn to_py_tls_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

fn default_protocol_versions() -> &'static [&'static SupportedProtocolVersion] {
    rustls::DEFAULT_VERSIONS
}
