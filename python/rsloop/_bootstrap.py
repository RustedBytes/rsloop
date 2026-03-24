from __future__ import annotations

import os as __os
import ssl as __ssl
import sys as __sys

__DLL_DIR_HANDLES: list[object] = []


def configure_windows_dll_search_path() -> None:
    if __sys.platform != "win32" or not hasattr(__os, "add_dll_directory"):
        return

    import shutil as __shutil

    candidate_dirs: list[str] = []
    gcc = __shutil.which("gcc")
    if gcc is not None:
        candidate_dirs.append(__os.path.dirname(gcc))

    msys_prefix = __os.environ.get("MSYSTEM_PREFIX")
    if msys_prefix:
        candidate_dirs.append(__os.path.join(msys_prefix, "bin"))

    candidate_dirs.extend(
        [
            r"C:\msys64\ucrt64\bin",
            r"C:\msys64\mingw64\bin",
            r"C:\msys64\clang64\bin",
        ]
    )

    seen: set[str] = set()
    for directory in candidate_dirs:
        normalized = __os.path.normcase(__os.path.abspath(directory))
        if normalized in seen or not __os.path.isdir(directory):
            continue
        if not __os.path.exists(__os.path.join(directory, "libstdc++-6.dll")):
            continue
        __DLL_DIR_HANDLES.append(__os.add_dll_directory(directory))
        seen.add(normalized)


def install_ssl_tracking() -> None:
    context_cls = __ssl.SSLContext
    if getattr(context_cls, "_rsloop_tracking_installed", False):
        return

    def mark_default_verify_paths(context):
        context.__dict__["_rsloop_use_default_verify_paths"] = True
        return context

    orig_create_default_context = __ssl.create_default_context
    orig_load_cert_chain = context_cls.load_cert_chain
    orig_load_default_certs = context_cls.load_default_certs
    orig_set_default_verify_paths = context_cls.set_default_verify_paths

    def create_default_context(*args, **kwargs):
        return mark_default_verify_paths(orig_create_default_context(*args, **kwargs))

    def load_cert_chain(self, certfile, keyfile=None, password=None):
        result = orig_load_cert_chain(
            self,
            certfile,
            keyfile=keyfile,
            password=password,
        )
        if callable(password):
            password_value = password()
        else:
            password_value = password
        if isinstance(password_value, str):
            password_value = password_value.encode()
        if password_value is not None and not isinstance(password_value, bytes):
            password_value = bytes(password_value)
        self.__dict__["_rsloop_certfile"] = __os.fspath(certfile)
        self.__dict__["_rsloop_keyfile"] = (
            __os.fspath(keyfile) if keyfile is not None else __os.fspath(certfile)
        )
        self.__dict__["_rsloop_key_password"] = password_value
        return result

    def load_default_certs(self, *args, **kwargs):
        result = orig_load_default_certs(self, *args, **kwargs)
        mark_default_verify_paths(self)
        return result

    def set_default_verify_paths(self):
        result = orig_set_default_verify_paths(self)
        mark_default_verify_paths(self)
        return result

    __ssl.create_default_context = create_default_context
    context_cls.load_cert_chain = load_cert_chain
    context_cls.load_default_certs = load_default_certs
    context_cls.set_default_verify_paths = set_default_verify_paths
    context_cls._rsloop_tracking_installed = True


def bootstrap() -> None:
    configure_windows_dll_search_path()
    install_ssl_tracking()
