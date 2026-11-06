"""Check internal dependency direction without requiring external services."""
from pathlib import Path
import tomllib

root = Path(__file__).resolve().parents[1]
allowed = {
    "inori-domain": set(), "inori-protocol": set(),
    "inori-module-api": {"inori-domain", "inori-protocol"},
    "inori-composition": {"inori-module-api"},
    "inori-runtime": {"inori-domain", "inori-protocol", "inori-module-api", "inori-composition"},
    "inori-storage-sqlite": {"inori-domain", "inori-runtime"},
    "inori-client-core": {"inori-domain", "inori-protocol"},
    "inori-extension-sdk": {"inori-protocol"},
    "inori-extension-host": {"inori-extension-sdk", "inori-protocol"},
    "inori-server": {"inori-domain", "inori-runtime", "inori-protocol", "inori-storage-sqlite", "inori-extension-host"},
    "inori-desktop": {"inori-client-core", "inori-domain", "inori-protocol"},
}
modules = {"inori-" + name for name in (
    "perception-basic", "dialogue-standard", "memory-standard", "autonomy-standard",
    "inference-providers", "speech-whisper", "speech-voicevox", "actions-extensions", "channel-standard")}
for name in modules:
    allowed[name] = {"inori-module-api", "inori-domain", "inori-protocol"}
for name in ("inori-inference-providers", "inori-actions-extensions"):
    allowed[name].add("inori-extension-host")
allowed["inori-server"] |= modules | {"inori-module-api", "inori-composition"}
for manifest in root.glob("**/Cargo.toml"):
    if any(part in {"target", "node_modules", ".cache"} for part in manifest.relative_to(root).parts):
        continue
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    if "package" not in data: continue
    name = data["package"]["name"]
    groups = [data.get("dependencies", {})] + [target.get("dependencies", {}) for target in data.get("target", {}).values()]
    dependencies = {dep for group in groups for dep in group}
    internal = {dep for dep in dependencies if dep.startswith("inori-")}
    if name not in allowed or internal - allowed[name]:
        raise SystemExit(f"invalid dependency direction: {name} -> {internal - allowed.get(name, set())}")
    if name == "inori-domain" and dependencies - {"serde", "serde_json", "thiserror"}:
        raise SystemExit("domain must remain free of IO/framework dependencies")
    if name in modules | {"inori-module-api", "inori-composition", "inori-runtime", "inori-server", "inori-storage-sqlite"} and dependencies & {"gpui", "gpui-component", "cpal", "inori-client-core"}:
        raise SystemExit(f"GUI/audio client dependency leaked into {name}")
    if name == "inori-runtime" and dependencies & {"reqwest", "axum", "sqlx"}:
        raise SystemExit("runtime must use ports instead of concrete adapters")
print("Dependency boundaries: OK")
