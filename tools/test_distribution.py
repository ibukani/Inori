"""Test manifest-driven packaging with synthetic bytes, never runtime data."""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile
import tarfile
import inori


class DistributionTests(unittest.TestCase):
    def setUp(self):
        (inori.ROOT / ".cache").mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="distribution-", dir=inori.ROOT / ".cache")
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.executable = self.work / "inori-server.exe"
        self.executable.write_bytes(b"synthetic executable")

    def test_allowlist_checksums_and_future_application_registration(self):
        app = dict(inori.registry()["server"])
        (self.work / "owner.token").write_text("synthetic-never-package")
        (self.work / "conversation.sqlite").write_bytes(b"synthetic-never-package")
        path = inori.package_artifact(app, self.executable, "test", "windows", "x86_64", self.work / "out", {})
        with zipfile.ZipFile(path) as archive:
            names = archive.namelist()
            self.assertTrue(any(name.endswith("/setup.cmd") for name in names))
            self.assertFalse(any("owner.token" in name or ".sqlite" in name or name.endswith("/setup.sh") for name in names))
            checksums = archive.read(next(name for name in names if name.endswith("/SHA256SUMS"))).decode()
            self.assertIn("inori-server.exe", checksums)
        with self.assertRaises(FileExistsError):
            inori.package_artifact(app, self.executable, "test", "windows", "x86_64", self.work / "out", {})
        manifest = self.work / "manifest.toml"
        manifest.write_text('schema_version=1\n[apps.future]\npackage="future-app"\nbinary="future-app"\nplatforms=["windows"]\nsetup_args=[]\nfiles=[]\n')
        self.assertEqual(inori.registry(manifest)["future"]["binary"], "future-app")

    def test_traversal_secrets_and_unsupported_architectures_are_rejected(self):
        for source, destination in [("../outside.md", "README.md"), ("README.md", "../README.md"), (".git/config", "config.toml")]:
            with self.assertRaises((ValueError, FileNotFoundError)):
                inori.safe_document(source, destination)
        secret = self.work / "server.secrets.toml"
        secret.write_text("synthetic")
        with self.assertRaises(ValueError):
            inori.safe_document(secret.name, secret.name, self.work)
        with self.assertRaises(ValueError):
            inori.package_artifact(inori.registry()["server"], self.executable, "test", "linux", "aarch64", self.work / "out", {})

    def test_unix_bundle_contains_executable_entry_point(self):
        self.executable = self.work / "inori-server"
        self.executable.write_bytes(b"synthetic")
        path = inori.package_artifact(inori.registry()["server"], self.executable, "test", "linux", "x86_64", self.work / "out", {})
        with tarfile.open(path) as archive:
            entries = {Path(member.name).name: member for member in archive.getmembers()}
            self.assertEqual(entries["setup.sh"].mode, 0o755)
            self.assertEqual(entries["inori-server"].mode, 0o755)
            self.assertNotIn("setup.cmd", entries)

    def test_update_passes_cross_target_to_license_collection(self):
        app = inori.registry()["server"]
        target = "x86_64-unknown-linux-gnu"
        with patch.object(inori, "license_documents", return_value={}) as license_docs, \
                patch.object(inori, "invoke") as invoke:
            inori.update_local_server(app, self.executable, "test", "x86_64", "linux", target)
        license_docs.assert_called_once_with(app, target)
        invoke.assert_called_once()


if __name__ == "__main__":
    unittest.main()
