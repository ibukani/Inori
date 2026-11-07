"""Shared development and allowlisted distribution entry point; never installs prerequisites."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import zipfile

if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11以上が必要です。Pythonを更新してください。")
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {"Windows": "windows", "Darwin": "macos", "Linux": "linux"}
SAFE_DOCUMENT_SUFFIXES = {".md", ".txt", ".sh", ".cmd", ".ps1", ".toml"}


def registry(path: Path = ROOT / "apps/manifest.toml") -> dict:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    if data.get("schema_version") != 1 or not data.get("apps"):
        raise ValueError("unsupported/empty application manifest")
    for app_id, app in data["apps"].items():
        if not re.fullmatch(r"[a-z0-9-]{1,64}", app_id):
            raise ValueError("invalid application ID")
        for field in ["package", "binary"]:
            if not re.fullmatch(r"[a-zA-Z0-9_-]+", app[field]):
                raise ValueError(f"invalid {field}")
        if not app["platforms"] or set(app["platforms"]) - set(PLATFORMS.values()):
            raise ValueError("invalid supported platforms")
        if not isinstance(app["setup_args"], list) or not all(isinstance(v, str) for v in app["setup_args"]):
            raise ValueError("invalid setup arguments")
    return data["apps"]


def invoke(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    if kwargs.get("text"):
        kwargs.setdefault("encoding", "utf-8")
    return subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def prerequisites(app: dict, sdk: bool = False) -> bool:
    good = True
    requirements = [("cargo", "Rust: https://rustup.rs/ （rust-toolchain.tomlの固定版を使用）")]
    current = PLATFORMS[platform.system()]
    if current == "windows":
        # cl.exe is normally absent from PATH outside a VS developer shell.
        vswhere = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe"
        installed = False
        if vswhere.is_file():
            result = subprocess.run([str(vswhere), "-latest", "-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"], capture_output=True, text=True, check=False)
            installed = result.returncode == 0 and bool(result.stdout.strip())
        if not installed and not shutil.which("cl"):
            print("不足: Visual Studio Build Toolsの「C++によるデスクトップ開発」とWindows SDKを導入してください。")
            good = False
        sdk_libs = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Windows Kits/10/Lib"
        if not any((entry / "um/x64/kernel32.lib").is_file() and (entry / "ucrt/x64/ucrt.lib").is_file()
                   for entry in sdk_libs.glob("10.*")):
            print("不足: Visual Studio InstallerからWindows 10/11 SDKを導入してください。")
            good = False
    else:
        requirements.append(("cc", "macOS: xcode-select --install / Debian・Ubuntu: sudo apt-get install build-essential pkg-config"))
    if sdk:
        requirements.extend([("node", "Node.jsを導入してください（SDK開発時のみ）"), ("corepack", "Corepackを導入し corepack pnpm install --frozen-lockfile を実行してください")])
    for name, hint in requirements:
        found = shutil.which(name)
        print(f"{name}: {found or '不足'}")
        if not found:
            print(hint)
            good = False
    if good:
        if shutil.which("rustup"):
            # cargo/rustc shims may install a missing override toolchain; list first instead.
            result = subprocess.run(["rustup", "toolchain", "list"], cwd=ROOT, capture_output=True, text=True, check=False)
            installed = result.returncode == 0 and any(line.startswith("1.95.0-") for line in result.stdout.splitlines())
        else:
            result = subprocess.run(["rustc", "--version"], cwd=ROOT, capture_output=True, text=True, check=False) if shutil.which("rustc") else None
            installed = result is not None and result.returncode == 0 and result.stdout.startswith("rustc 1.95.0 ")
        if not installed:
            print("固定Rustツールチェーンを利用できません。rustup toolchain install 1.95.0 --profile minimal --component rustfmt,clippy を実行してください。")
            good = False
    return good


def binary_path(app: dict, release: bool, target: str | None = None) -> Path:
    directory = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not directory.is_absolute():
        directory = ROOT / directory
    if target:
        directory /= target
    directory /= "release" if release else "debug"
    windows = "windows" in target if target else os.name == "nt"
    return directory / (app["binary"] + (".exe" if windows else ""))


def build(app: dict, release: bool, target: str | None) -> None:
    command = ["cargo", "build", "--locked", "-p", app["package"]]
    if release:
        command.append("--release")
    if target:
        command += ["--target", target]
    invoke(command)


def safe_document(source: str, target: str, root: Path = ROOT) -> tuple[Path, str]:
    relative = PurePosixPath(source)
    archive_name = PurePosixPath(target)
    if relative.is_absolute() or ".." in relative.parts or archive_name.is_absolute() or ".." in archive_name.parts or "\\" in target or ":" in target:
        raise ValueError("unsafe distribution path")
    path = (root / source).resolve(strict=True)
    path.relative_to(root.resolve())
    if path.suffix.lower() not in SAFE_DOCUMENT_SUFFIXES or any(part.startswith(".") for part in relative.parts):
        raise ValueError("distribution documents must be explicitly allowlisted text files")
    if any(word in path.name.lower() for word in ["secret", "token", "credential", ".env"]):
        raise ValueError("credential files cannot be distributed")
    return path, str(archive_name)


def license_documents(app: dict, target: str | None) -> dict[str, bytes]:
    metadata = json.loads(invoke(["cargo", "metadata", "--locked", "--format-version", "1"], capture_output=True, text=True).stdout)
    command = ["cargo", "tree", "--locked", "-p", app["package"], "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
    if target:
        command += ["--target", target]
    tree = invoke(command, capture_output=True, text=True).stdout
    used = set(re.findall(r"^([^ \r\n]+) v([^ \r\n]+)", tree, re.MULTILINE))
    notices = ["Third-party Rust dependencies; license texts are included where supplied by the package."]
    documents = {}
    for package in metadata["packages"]:
        if (package["name"], package["version"]) not in used or package["source"] is None:
            continue
        label = f"{package['name']}-{package['version']}"
        notices.append(f"{label}: {package.get('license') or 'See package license file'} | {package.get('repository') or ''}")
        directory = Path(package["manifest_path"]).parent
        candidates = list(directory.glob("LICENSE*")) + list(directory.glob("COPYING*"))
        if package.get("license_file"):
            candidates.append(directory / package["license_file"])
        for path in candidates:
            try:
                path.resolve().relative_to(directory.resolve())
            except ValueError:
                continue
            if path.is_file() and path.stat().st_size < 1024 * 1024:
                documents[f"licenses/{label}/{path.name}"] = path.read_bytes()
    for path in [ROOT / "LICENSE", ROOT / "LICENSE.md", ROOT / "LICENSE.txt"]:
        if path.is_file():
            documents[path.name] = path.read_bytes()
    notices.append("Inori itself: no project license has been declared; this bundle is for the private project's use.")
    documents["THIRD_PARTY_NOTICES.txt"] = ("\n".join(notices) + "\n").encode()
    return documents


def package_artifact(app: dict, executable: Path, version: str, current: str, architecture: str,
                     output_dir: Path, notices: dict[str, bytes], root: Path = ROOT) -> Path:
    if current not in app["platforms"]:
        raise ValueError("unsupported target platform")
    if architecture not in {"x86_64", "aarch64"} or (current != "macos" and architecture != "x86_64"):
        raise ValueError("unsupported target architecture")
    prefix = f"{app['binary']}-{version}-{current}-{architecture}"
    contents = {executable.name: executable.read_bytes(), **notices}
    for entry in app["files"]:
        if current not in entry.get("platforms", app["platforms"]):
            continue
        path, name = safe_document(entry["source"], entry["target"], root)
        if name in contents:
            raise ValueError("duplicate distribution entry")
        contents[name] = path.read_bytes()
    contents["SHA256SUMS"] = ("\n".join(f"{hashlib.sha256(data).hexdigest()}  {name}" for name, data in sorted(contents.items())) + "\n").encode()
    output_dir.mkdir(parents=True, exist_ok=True)
    destination = output_dir / (prefix + (".zip" if current == "windows" else ".tar.gz"))
    if destination.exists():
        raise FileExistsError(f"distribution already exists: {destination}")
    if current == "windows":
        with zipfile.ZipFile(destination, "x", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, data in sorted(contents.items()):
                archive.writestr(f"{prefix}/{name}", data)
    else:
        with tarfile.open(destination, "x:gz") as archive:
            for name, data in sorted(contents.items()):
                info = tarfile.TarInfo(f"{prefix}/{name}")
                info.size = len(data)
                info.mode = 0o755 if name == executable.name or name.endswith(".sh") else 0o644
                info.mtime = 0
                archive.addfile(info, io.BytesIO(data))
    destination.with_name(destination.name + ".sha256").write_text(f"{hashlib.sha256(destination.read_bytes()).hexdigest()}  {destination.name}\n", encoding="utf-8")
    return destination


def require_local_server(command: str, app_id: str, current: str, target: str | None) -> None:
    if app_id != "server" or current not in {"windows", "macos", "linux"}:
        raise ValueError(f"{command} is supported only for the server on Windows, macOS, or Linux")
    if target:
        raise ValueError(f"{command} operates on this machine and does not accept --target")


def update_bundle_member_parts(member_name: str, bundle_name: str) -> tuple[str, ...]:
    if not member_name or "\\" in member_name or "\x00" in member_name or ":" in member_name:
        raise ValueError("generated update archive contains an unexpected entry")
    path = PurePosixPath(member_name)
    if (
        path.is_absolute()
        or path.as_posix() != member_name
        or len(path.parts) < 2
        or path.parts[0] != bundle_name
        or any(part in {"", ".", ".."} for part in path.parts)
    ):
        raise ValueError("generated update archive contains an unexpected entry")
    return path.parts[1:]


def installed_server_paths(current: str) -> tuple[Path, Path]:
    if current == "windows":
        program_files = Path(os.environ.get("ProgramFiles", r"C:\Program Files"))
        program_data = Path(os.environ.get("ProgramData", r"C:\ProgramData"))
        return (program_files / "Inori" / "server" / "inori-server.exe",
                program_data / "Inori" / "server")
    if current == "macos":
        return (Path("/usr/local/lib/inori/inori-server"),
                Path("/Library/Application Support/Inori/server"))
    return Path("/opt/inori/inori-server"), Path("/var/lib/inori")


def require_windows_admin() -> None:
    import ctypes
    if not ctypes.windll.shell32.IsUserAnAdmin():
        raise ValueError("管理者として開いたPowerShellから実行してください。")


def privileged_command(command: list[str], current: str, preserve_env: list[str] | None = None) -> list[str]:
    if current == "windows" or os.geteuid() == 0:
        return command
    names = [name for name in (preserve_env or []) if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) and name in os.environ]
    sudo_args = [f"--preserve-env={','.join(names)}"] if names else []
    return ["sudo", *sudo_args, *command]


def setup_environment_names(arguments: list[str]) -> list[str]:
    names = []
    for index, argument in enumerate(arguments):
        if argument == "--api-key-env" and index + 1 < len(arguments):
            names.append(arguments[index + 1])
        elif argument.startswith("--api-key-env="):
            names.append(argument.split("=", 1)[1])
    return names


def install_local_server(executable: Path, app: dict, extra: list[str], current: str) -> None:
    command = [str(executable), *app["setup_args"], "--autostart", "--start", *extra]
    invoke(privileged_command(command, current, setup_environment_names(extra)))


def uninstall_local_server(current: str) -> None:
    executable, data_dir = installed_server_paths(current)
    if not executable.is_file():
        print("Inori server is not installed.")
        return
    invoke(privileged_command([str(executable), "--data-dir", str(data_dir), "service", "uninstall"], current))
    if current == "windows":
        executable.unlink()
    else:
        invoke(privileged_command(["rm", "-f", "--", str(executable)], current))
    print(f"Inori server was uninstalled. Data and settings remain in {data_dir}.")


def update_local_server(app: dict, executable: Path, version: str, architecture: str,
                        current: str, target: str | None) -> None:
    with tempfile.TemporaryDirectory(prefix="inori-dev-update-") as temporary:
        temporary_dir = Path(temporary)
        output_dir = temporary_dir / "dist"
        output_dir.mkdir()
        artifact = package_artifact(app, executable, version, current, architecture, output_dir,
                                   license_documents(app, target))
        bundle_name = artifact.name.removesuffix(".zip" if current == "windows" else ".tar.gz")
        bundle_dir = temporary_dir / bundle_name
        bundle_dir.mkdir()
        if current == "windows":
            with zipfile.ZipFile(artifact) as archive:
                seen: set[tuple[str, ...]] = set()
                for member in archive.infolist():
                    parts = update_bundle_member_parts(member.filename, bundle_name)
                    mode = member.external_attr >> 16
                    if member.is_dir() or stat.S_IFMT(mode) not in {0, stat.S_IFREG} or parts in seen:
                        raise ValueError("generated update archive contains an unexpected entry")
                    seen.add(parts)
                    destination = bundle_dir.joinpath(*parts)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(archive.read(member))
            command = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(bundle_dir / "update.ps1")]
        else:
            with tarfile.open(artifact, "r:gz") as archive:
                seen: set[tuple[str, ...]] = set()
                for member in archive.getmembers():
                    parts = update_bundle_member_parts(member.name, bundle_name)
                    if not member.isfile() or parts in seen:
                        raise ValueError("generated update archive contains an unexpected entry")
                    seen.add(parts)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ValueError("cannot read generated update archive entry")
                    destination = bundle_dir.joinpath(*parts)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(source.read())
                    destination.chmod(member.mode & 0o777)
            command = [str(bundle_dir / "update.sh")]
        invoke(privileged_command(command, current))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["doctor", "setup", "build", "run", "package", "install", "update", "uninstall"])
    parser.add_argument("--app", default="server")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--target")
    parser.add_argument("--sdk", action="store_true")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/dist")
    args, extra = parser.parse_known_args()
    if extra[:1] == ["--"]:
        extra.pop(0)
    apps = registry()
    if args.app not in apps:
        parser.error(f"unknown application {args.app!r}; available: {', '.join(apps)}")
    app = apps[args.app]
    current = PLATFORMS.get(platform.system())
    if current not in app["platforms"]:
        parser.error("this application does not support the current OS")
    if args.command in {"install", "update", "uninstall"}:
        try:
            require_local_server(args.command, args.app, current, args.target)
            if current == "windows":
                require_windows_admin()
        except ValueError as error:
            parser.error(str(error))
        if args.command in {"update", "uninstall"} and extra:
            parser.error(f"unexpected extra arguments for {args.command}")
        if args.command == "uninstall":
            try:
                uninstall_local_server(current)
            except (OSError, subprocess.CalledProcessError, ValueError) as error:
                raise SystemExit(str(error)) from None
            return 0
    if args.command == "doctor":
        return 0 if prerequisites(app, args.sdk) else 1
    if args.command in {"build", "package", "update"} and extra:
        parser.error("unexpected extra arguments")
    release = args.release or args.command in {"package", "install", "update"}
    if not args.no_build:
        if not prerequisites(app, args.sdk):
            return 1
        build(app, release, args.target)
    executable = binary_path(app, release, args.target)
    if args.command == "install":
        install_local_server(executable, app, extra, current)
    elif args.command == "update":
        version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
        architecture = {"AMD64": "x86_64", "arm64": "aarch64"}.get(platform.machine(), platform.machine())
        update_local_server(app, executable, version, architecture, current, args.target)
    elif args.command == "setup":
        invoke([str(executable), *app["setup_args"], *extra])
    elif args.command == "run":
        invoke([str(executable), *extra])
    elif args.command == "package":
        version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
        target_os = next((value for value in app["platforms"] if args.target and (value in args.target or value == "macos" and "apple-darwin" in args.target)), current)
        architecture = args.target.split("-")[0] if args.target else {"AMD64": "x86_64", "arm64": "aarch64"}.get(platform.machine(), platform.machine())
        print(package_artifact(app, executable, version, target_os, architecture, args.output, license_documents(app, args.target)))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from None
