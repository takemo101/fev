#!/usr/bin/env python3
"""Offline, consumer-level installer regressions.

Run from any directory: python3 scripts/test-install.py
Prerequisites: a native target/release/fev (cargo build --release), Python 3,
real curl, and the installer's normal POSIX tools on macOS or Linux.

Every scenario gets a TemporaryDirectory for HOME, INSTALL_DIR, TMPDIR, the
working directory, and PATH adapters. Archives contain the REAL native release
binary; no application output or download results are mocked. A loopback HTTP
server serves only the release paths/target requested by that scenario. The
curl adapter rewrites GitHub URLs to loopback and invokes the real curl, refusing
all other origins so a test cannot access the network. Only uname is simulated.
Other-platform cases prove asset selection, NOT cross-platform binary execution:
their archives still contain the native host binary. Installed --version checks
prove that the actual host executable survives installation.
"""

import contextlib
import gzip
import hashlib
import http.server
import io
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest


ROOT = Path(__file__).resolve().parent.parent
INSTALLER = ROOT / "install.sh"
BINARY = ROOT / "target" / "release" / "fev"
DEFAULT_REPO = "takemo101/fev"
TARGETS = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "aarch64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Darwin", "amd64"): "x86_64-apple-darwin",
    ("Linux", "arm64"): "aarch64-unknown-linux-musl",
    ("Linux", "aarch64"): "aarch64-unknown-linux-musl",
    ("Linux", "x86_64"): "x86_64-unknown-linux-musl",
    ("Linux", "amd64"): "x86_64-unknown-linux-musl",
}


def archive_with_binary():
    """Use reproducible archive metadata without replacing native binary bytes."""
    output = io.BytesIO()

    def metadata(member):
        member.uid = member.gid = 0
        member.uname = member.gname = ""
        member.mtime = 0
        member.mode = 0o755
        return member

    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as zipped:
        with tarfile.open(fileobj=zipped, mode="w") as archive:
            archive.add(BINARY, arcname="fev", recursive=False, filter=metadata)
    return output.getvalue()


def archive_without_binary():
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as zipped:
        with tarfile.open(fileobj=zipped, mode="w") as archive:
            content = b"This release accidentally omitted its executable.\n"
            member = tarfile.TarInfo("release-notes.txt")
            member.size = len(content)
            archive.addfile(member, io.BytesIO(content))
    return output.getvalue()


class ReleaseFixture:
    """Own all mutable state; the server never reads repository files."""

    def __init__(self, root, routes, real_curl, system, machine):
        self.root = root
        self.routes = routes
        self.requests = []
        self.home = root / "isolated home"
        self.tmp = root / "temporary downloads"
        self.destination = root / "installation with spaces" / "bin"
        self.home.mkdir()
        self.tmp.mkdir()
        self.marker = self.tmp / "unrelated-file"
        self.marker.write_bytes(b"leave this alone\n")
        adapters = root / "transport adapters"
        adapters.mkdir()
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                fixture.requests.append(self.path)
                data = fixture.routes.get(self.path)
                if data is None:
                    self.send_error(404, "No fixture for this release asset")
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(
            target=self.server.serve_forever,
            kwargs={"poll_interval": 0.01},
            daemon=True,
        )
        self.thread.start()
        endpoint = "http://127.0.0.1:" + str(self.server.server_port)
        transport = adapters / "curl-transport.py"
        transport.write_text(
            "import subprocess, sys\n"
            "from urllib.parse import urlsplit\n"
            f"endpoint = {endpoint!r}\n"
            "arguments = []\n"
            "for argument in sys.argv[1:]:\n"
            "    if '://' in argument:\n"
            "        url = urlsplit(argument)\n"
            "        if (url.scheme, url.netloc) != ('https', 'github.com'):\n"
            "            sys.exit('Only fixed GitHub release URLs are permitted')\n"
            "        if url.query or url.fragment:\n"
            "            sys.exit('Unexpected URL query or fragment')\n"
            "        argument = endpoint + url.path\n"
            "    arguments.append(argument)\n"
            f"sys.exit(subprocess.call([{real_curl!r}, '-q', '--noproxy', '*'] + arguments))\n",
            encoding="utf-8",
        )
        curl = adapters / "curl"
        curl.write_text(
            "#!/bin/sh\nexec "
            + shlex.quote(sys.executable)
            + " "
            + shlex.quote(str(transport))
            + ' "$@"\n',
            encoding="utf-8",
        )
        curl.chmod(0o755)
        uname = adapters / "uname"
        uname.write_text(
            "#!/bin/sh\n"
            'case "${1:-}" in\n'
            '  -s|"") printf "%s\\n" "$FEV_TEST_SYSTEM" ;;\n'
            '  -m) printf "%s\\n" "$FEV_TEST_MACHINE" ;;\n'
            '  *) echo "Unexpected uname arguments" >&2; exit 1 ;;\n'
            "esac\n",
            encoding="utf-8",
        )
        uname.chmod(0o755)
        self.env = os.environ.copy()
        for variable in ("HOME", "INSTALL_DIR", "TMPDIR", "FEV_REPO", "VERSION"):
            self.env.pop(variable, None)
        self.env.update(
            HOME=str(self.home),
            INSTALL_DIR=str(self.destination),
            TMPDIR=str(self.tmp),
            PATH=str(adapters) + os.pathsep + os.environ.get("PATH", os.defpath),
            FEV_TEST_SYSTEM=system,
            FEV_TEST_MACHINE=machine,
            # Do not let a user's curl configuration redirect a fixture download.
            CURL_HOME=str(self.home),
            XDG_CONFIG_HOME=str(self.home / ".config"),
        )

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def install(self):
        return subprocess.run(
            ["sh", str(INSTALLER)],
            cwd=self.root,
            env=self.env,
            text=True,
            capture_output=True,
            timeout=30,
        )


class InstallerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not INSTALLER.is_file():
            raise AssertionError(f"Installer is absent: {INSTALLER}")
        if not BINARY.is_file():
            raise RuntimeError("Build the native fixture first: cargo build --release")
        cls.real_curl = shutil.which("curl")
        if cls.real_curl is None:
            raise RuntimeError("Install real curl before running installer regressions")
        cls.host = (platform.system(), platform.machine().lower())
        if cls.host not in TARGETS:
            raise RuntimeError("Native execution proof requires macOS or Linux on ARM64/x86_64")
        cls.binary_bytes = BINARY.read_bytes()
        cls.archive = archive_with_binary()
        version = subprocess.run(
            [str(BINARY), "--version"],
            text=True,
            capture_output=True,
            timeout=10,
            check=True,
        )
        cls.expected_version = version.stdout

    @contextlib.contextmanager
    def fixture(self, system=None, machine=None, version=None, repo=DEFAULT_REPO,
                archive=None, manifest="valid"):
        system = self.host[0] if system is None else system
        machine = self.host[1] if machine is None else machine
        target = TARGETS.get((system, machine))
        routes = {}
        if target is not None:
            asset = f"fev-{target}.tar.gz"
            prefix = f"/{repo}/releases/"
            prefix += "latest/download/" if version is None else f"download/{version}/"
            data = self.archive if archive is None else archive
            routes[prefix + asset] = data
            digest = hashlib.sha256(data).hexdigest()
            if manifest == "corrupt":
                digest = "0" * 64
            if manifest == "missing-entry":
                asset = "unrelated-release.tar.gz"
            if manifest != "missing":
                routes[prefix + "checksums.txt"] = f"{digest}  {asset}\n".encode()
        with tempfile.TemporaryDirectory(prefix="fev-install-test-") as directory:
            fixture = ReleaseFixture(Path(directory), routes, self.real_curl, system, machine)
            try:
                if version is not None:
                    fixture.env["VERSION"] = version
                if repo != DEFAULT_REPO:
                    fixture.env["FEV_REPO"] = repo
                yield fixture
            finally:
                fixture.close()

    def assert_cleaned(self, fixture):
        self.assertEqual(set(fixture.tmp.iterdir()), {fixture.marker})
        self.assertEqual(fixture.marker.read_bytes(), b"leave this alone\n")

    def assert_native_executable(self, executable, fixture):
        self.assertTrue(os.access(executable, os.X_OK), str(executable))
        self.assertEqual(executable.read_bytes(), self.binary_bytes)
        result = subprocess.run(
            [str(executable), "--version"],
            cwd=fixture.root,
            env=fixture.env,
            text=True,
            capture_output=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, self.expected_version)

    def assert_install_succeeds(self, fixture):
        result = fixture.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_native_executable(fixture.destination / "fev", fixture)
        self.assert_cleaned(fixture)

    def assert_install_rejected(self, fixture, existing):
        executable = fixture.destination / "fev"
        if existing:
            fixture.destination.mkdir(parents=True)
            shutil.copy2(BINARY, executable)
            original_stat = executable.stat()
        result = fixture.install()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        if existing:
            # Failure must not replace or rewrite even an already valid executable.
            self.assertEqual(executable.stat().st_ino, original_stat.st_ino)
            self.assertEqual(executable.stat().st_mtime_ns, original_stat.st_mtime_ns)
            self.assertEqual(executable.stat().st_mode, original_stat.st_mode)
            self.assert_native_executable(executable, fixture)
        else:
            self.assertFalse(executable.exists())
        self.assert_cleaned(fixture)

    def test_latest_default_directory_and_exact_tag_with_custom_repository(self):
        with self.fixture() as fixture:
            fixture.env.pop("INSTALL_DIR")
            fixture.destination = fixture.home / ".local" / "bin"
            self.assert_install_succeeds(fixture)
        with self.fixture(version="v0.9.7-test.2", repo="fixture-owner/fixture-fev") as fixture:
            self.assert_install_succeeds(fixture)

    def test_supported_architecture_aliases_select_only_available_asset(self):
        # All non-host OS/architecture combinations simulate mapping, not runtime.
        for system, machine in TARGETS:
            with self.subTest(system=system, machine=machine):
                with self.fixture(system=system, machine=machine) as fixture:
                    self.assert_install_succeeds(fixture)

    def test_unsupported_platform_fails_before_download_or_destination_mutation(self):
        for system, machine in (("FreeBSD", "x86_64"), ("Darwin", "i386"),
                                ("Linux", "riscv64")):
            with self.subTest(system=system, machine=machine):
                with self.fixture(system=system, machine=machine) as fixture:
                    self.assert_install_rejected(fixture, existing=False)
                    self.assertEqual(fixture.requests, [])
                    self.assertFalse(fixture.destination.parent.exists())

    def test_checksum_failures_preserve_existing_executable_and_clean_downloads(self):
        for manifest in ("corrupt", "missing", "missing-entry"):
            with self.subTest(manifest=manifest):
                with self.fixture(manifest=manifest) as fixture:
                    self.assert_install_rejected(fixture, existing=True)

    def test_directory_destination_is_rejected_without_overwriting_nested_files(self):
        for linked in (False, True):
            with self.subTest(symlink=linked):
                with self.fixture() as fixture:
                    fixture.destination.mkdir(parents=True)
                    destination = fixture.destination / "fev"
                    if linked:
                        directory = fixture.root / "unrelated directory"
                        directory.mkdir()
                        destination.symlink_to(directory, target_is_directory=True)
                    else:
                        directory = destination
                        directory.mkdir()
                    nested = directory / "fev"
                    nested.write_bytes(b"unrelated file must remain untouched\n")
                    original = nested.stat()
                    result = fixture.install()
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(nested.read_bytes(), b"unrelated file must remain untouched\n")
                    self.assertEqual(nested.stat().st_ino, original.st_ino)
                    self.assertEqual(nested.stat().st_mtime_ns, original.st_mtime_ns)
                    self.assert_cleaned(fixture)

    def test_invalid_archives_cannot_install_or_replace_executable(self):
        for label, archive in (("invalid-tar", b"not a gzip or tar archive\n"),
                               ("missing-binary", archive_without_binary())):
            for existing in (False, True):
                with self.subTest(archive=label, existing=existing):
                    # Valid checksums force the installer past download validation.
                    with self.fixture(archive=archive) as fixture:
                        self.assert_install_rejected(fixture, existing=existing)


if __name__ == "__main__":
    unittest.main(verbosity=2)
