//! Certificate, private-key, and root-store loading.

use std::fs;
use std::io::{self, BufReader, Cursor};
use std::sync::OnceLock;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

static NATIVE_ROOTS: OnceLock<Result<Vec<CertificateDer<'static>>, String>> = OnceLock::new();

fn native_root_certificates() -> PyResult<&'static [CertificateDer<'static>]> {
    let result = NATIVE_ROOTS.get_or_init(|| {
        let native = rustls_native_certs::load_native_certs();
        if let Some(error) = native.errors.into_iter().next() {
            return Err(error.to_string());
        }
        Ok(native.certs)
    });
    result.as_deref().map_err(|error| {
        PyRuntimeError::new_err(format!("failed to load native CA certificates: {error}"))
    })
}

pub(super) fn root_store_from_context(
    py: Python<'_>,
    ssl_context: &Py<PyAny>,
) -> PyResult<RootCertStore> {
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

    let use_default_verify_paths = ssl_context
        .bind(py)
        .getattr("__dict__")?
        .cast::<PyDict>()?
        .get_item("_rsloop_use_default_verify_paths")?
        .and_then(|value| value.extract::<bool>().ok())
        .unwrap_or(false);
    if use_default_verify_paths {
        for cert in native_root_certificates()? {
            roots.add(cert.clone()).map_err(to_py_tls_err)?;
        }
    }

    Ok(roots)
}

pub(super) fn load_cert_chain_metadata(
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

pub(super) fn verify_mode_value(py: Python<'_>, ssl_context: &Py<PyAny>) -> PyResult<i32> {
    ssl_context.getattr(py, "verify_mode")?.extract::<i32>(py)
}

pub(super) fn ssl_verify_constant(py: Python<'_>, name: &str) -> PyResult<i32> {
    py.import("ssl")?.getattr(name)?.extract::<i32>()
}

fn io_err_to_py(err: io::Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

pub(super) fn to_py_tls_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[cfg(test)]
mod tests {
    use pyo3::Python;

    use super::{load_pem_identity, load_private_key};

    const CERTFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/cert.pem");
    const KEYFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/key.pem");

    #[test]
    fn loads_checked_in_certificate_chain_and_private_key() {
        let identity = load_pem_identity(CERTFILE, KEYFILE, None)
            .expect("fixture identity should parse")
            .expect("fixture identity should be present");

        assert!(!identity.0.is_empty());
        assert!(!identity.1.secret_der().is_empty());
    }

    #[test]
    fn rejects_password_protected_key_configuration() {
        crate::initialize_python_for_tests();
        let result = load_pem_identity(CERTFILE, KEYFILE, Some(b"secret"));
        let Err(error) = result else {
            panic!("password-protected identity should fail");
        };

        Python::attach(|py| {
            assert!(
                error
                    .value(py)
                    .to_string()
                    .contains("encrypted private keys are not supported")
            );
        });
    }

    #[test]
    fn rejects_data_without_a_supported_private_key() {
        crate::initialize_python_for_tests();
        let result = load_private_key(b"not a PEM private key".to_vec());
        let Err(error) = result else {
            panic!("invalid private key data should fail");
        };

        Python::attach(|py| {
            assert_eq!(
                error.value(py).to_string(),
                "no supported private key found"
            );
        });
    }

    #[test]
    fn accepts_pkcs1_and_sec1_private_key_pem_labels() {
        use rustls::pki_types::PrivateKeyDer;

        let pkcs1 = load_private_key(
            b"-----BEGIN RSA PRIVATE KEY-----\nAQID\n-----END RSA PRIVATE KEY-----\n".to_vec(),
        )
        .expect("PKCS#1 PEM key");
        assert!(matches!(pkcs1, PrivateKeyDer::Pkcs1(_)));

        let sec1 = load_private_key(
            b"-----BEGIN EC PRIVATE KEY-----\nAQID\n-----END EC PRIVATE KEY-----\n".to_vec(),
        )
        .expect("SEC1 PEM key");
        assert!(matches!(sec1, PrivateKeyDer::Sec1(_)));
    }

    #[test]
    fn missing_identity_files_surface_the_io_error() {
        crate::initialize_python_for_tests();
        let result = load_pem_identity(
            "/definitely/missing/rsloop-cert.pem",
            "/definitely/missing/rsloop-key.pem",
            None,
        );
        let Err(error) = result else {
            panic!("missing certificate should fail");
        };

        Python::attach(|py| {
            let message = error.value(py).to_string();
            assert!(message.contains("No such file") || message.contains("cannot find"));
        });
    }
}
