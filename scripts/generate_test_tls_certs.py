from __future__ import annotations

import os
import pathlib
import shutil
import subprocess
import sys
import textwrap


CA_CONFIG = """
[req]
distinguished_name = dn
x509_extensions = v3_ca
prompt = no

[dn]
CN = rsloop-test-ca

[v3_ca]
basicConstraints = critical, CA:true
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
"""

CERT_CONFIG = """
[req]
distinguished_name = dn
req_extensions = v3_req
prompt = no

[dn]
CN = localhost

[v3_req]
subjectAltName = @alt_names
basicConstraints = CA:false
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth

[v3_cert]
subjectAltName = @alt_names
basicConstraints = CA:false
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer

[alt_names]
DNS.1 = localhost
IP.1 = 127.0.0.1
"""


def find_openssl() -> pathlib.Path:
    candidates: list[str | pathlib.Path | None] = [
        os.environ.get("OPENSSL"),
        shutil.which("openssl"),
    ]

    if os.name == "nt":
        git = shutil.which("git")
        if git:
            git_root = pathlib.Path(git).resolve().parent.parent
            candidates.extend(
                [
                    git_root / "usr" / "bin" / "openssl.exe",
                    git_root / "mingw64" / "bin" / "openssl.exe",
                    git_root / "mingw32" / "bin" / "openssl.exe",
                ]
            )
        program_files = pathlib.Path(os.environ.get("ProgramFiles", "C:/Program Files"))
        candidates.extend(
            [
                program_files / "Git" / "usr" / "bin" / "openssl.exe",
                program_files / "Git" / "mingw64" / "bin" / "openssl.exe",
            ]
        )

    for candidate in candidates:
        if candidate and pathlib.Path(candidate).is_file():
            return pathlib.Path(candidate)

    raise RuntimeError(
        "OpenSSL was not found. Install OpenSSL, install Git for Windows, or set "
        "the OPENSSL environment variable to the executable path."
    )


def run(openssl: pathlib.Path, *args: str) -> None:
    try:
        subprocess.run(
            [str(openssl), *args],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
    except subprocess.CalledProcessError as exc:
        raise RuntimeError(f"OpenSSL failed: {exc.stderr.strip()}") from exc


def generate(outdir: pathlib.Path) -> None:
    outdir.mkdir(parents=True, exist_ok=True)
    generated = {
        name: outdir / name
        for name in (
            "ca-cert.pem",
            "ca-key.pem",
            "cert.pem",
            "key.pem",
            "cert.csr",
            "cert.cnf",
            "ca.cnf",
            "ca-cert.srl",
        )
    }
    for path in generated.values():
        path.unlink(missing_ok=True)

    generated["ca.cnf"].write_text(
        textwrap.dedent(CA_CONFIG).lstrip(), encoding="utf-8"
    )
    generated["cert.cnf"].write_text(
        textwrap.dedent(CERT_CONFIG).lstrip(), encoding="utf-8"
    )

    openssl = find_openssl()
    try:
        run(
            openssl,
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-nodes",
            "-keyout",
            str(generated["ca-key.pem"]),
            "-out",
            str(generated["ca-cert.pem"]),
            "-days",
            "3650",
            "-config",
            str(generated["ca.cnf"]),
        )
        run(
            openssl,
            "req",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-nodes",
            "-keyout",
            str(generated["key.pem"]),
            "-out",
            str(generated["cert.csr"]),
            "-config",
            str(generated["cert.cnf"]),
        )
        run(
            openssl,
            "x509",
            "-req",
            "-in",
            str(generated["cert.csr"]),
            "-CA",
            str(generated["ca-cert.pem"]),
            "-CAkey",
            str(generated["ca-key.pem"]),
            "-CAcreateserial",
            "-out",
            str(generated["cert.pem"]),
            "-days",
            "3650",
            "-sha256",
            "-extfile",
            str(generated["cert.cnf"]),
            "-extensions",
            "v3_cert",
        )
    finally:
        for name in ("cert.csr", "cert.cnf", "ca.cnf", "ca-cert.srl"):
            generated[name].unlink(missing_ok=True)

    print(f"Generated TLS test certs in {outdir}")


if __name__ == "__main__":
    output = (
        pathlib.Path(sys.argv[1])
        if len(sys.argv) > 1
        else pathlib.Path("tests/fixtures/tls")
    )
    generate(output)
