use std::fs;
use std::io::{self, BufReader, Cursor};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;

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
        let native = rustls_native_certs::load_native_certs();
        if let Some(error) = native.errors.into_iter().next() {
            return Err(PyRuntimeError::new_err(format!(
                "failed to load native CA certificates: {error}"
            )));
        }
        for cert in native.certs {
            roots.add(cert).map_err(to_py_tls_err)?;
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
