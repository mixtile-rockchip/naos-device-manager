"""Build the static ARM64 release, its SPDX 2.3 inventory and checksums."""
import argparse
from datetime import datetime, timezone
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGET = "aarch64-unknown-linux-musl"
STEM = "naos-core-linux-arm64"
LICENSES = "usr/share/licenses/naos-core"
REPO = "https://github.com/mixtile-rockchip/naos-device-manager"


def output(*cmd):
    return subprocess.check_output(cmd, cwd=ROOT, text=True).strip()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(obj):
    return (json.dumps(obj, sort_keys=True, indent=2) + "\n").encode()


def checksum(algorithm, value):
    return {"algorithm": algorithm, "checksumValue": value}


def pkg(identifier, name, version, location, license_id="NOASSERTION"):
    record = {"SPDXID": identifier, "name": name,
            "downloadLocation": location, "filesAnalyzed": False, "licenseDeclared": license_id,
            "licenseConcluded": "NOASSERTION", "copyrightText": "NOASSERTION"}
    if version:
        record["versionInfo"] = version
    return record


def relation(source, kind, target):
    return {"spdxElementId": source, "relationshipType": kind, "relatedSpdxElement": target}


def build():
    cmd = ["cargo", "build", "--locked", "--release", "--bin", "naos-core", "--target", TARGET,
           "--message-format=json-render-diagnostics"]
    events = [json.loads(line) for line in output(*cmd).splitlines()]
    artifacts = [e for e in events if e["reason"] == "compiler-artifact"]
    binary, = [Path(a["executable"]) for a in artifacts if a.get("executable") and a["target"]["name"] == "naos-core"]
    headers = output("readelf", "-hldW", binary)
    if not re.search(r"Machine:\s+AArch64", headers) or "INTERP" in headers or "(NEEDED)" in headers:
        raise ValueError("Expected a static ARM64 executable")
    crates = set()
    for a in artifacts:
        manifest = Path(a["manifest_path"])
        if manifest == ROOT / "Cargo.toml":
            continue
        if any(Path(f).suffix == ".rlib" and Path(f).is_relative_to(binary.parent) for f in a["filenames"]):
            if not a["package_id"].startswith("registry+"):
                raise ValueError(f"Release dependency has no registry checksum: {manifest.parent.name}")
            crates.add(manifest)
    if not crates:
        raise ValueError("Cargo reported no target libraries")
    return binary, crates


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("--output", type=Path, default=ROOT / "target/dist")
    args = parser.parse_args()
    binary, manifests = build()
    revision = output("git", "rev-parse", "HEAD")
    epoch = int(output("git", "show", "-s", "--format=%ct", "HEAD"))
    rust = output("rustc", "--version")
    sysroot = Path(output("rustc", "--print", "sysroot"))
    lock = (ROOT / "Cargo.lock").read_bytes()
    locked = {(p["name"], p["version"]): p["checksum"] for p in tomllib.loads(lock.decode())["package"] if "checksum" in p}
    exe = "usr/bin/naos-core"
    payload = {exe: (binary.read_bytes(), 0o755)}
    for src, dest, mode in (("config/device.ini.example", "etc/naos-core/device.ini", 0o600),
                            ("config/naosd.yml", "etc/naosd/config.yml", 0o644),
                            ("init.d/naos-core", "etc/init.d/naos-core", 0o755),
                            ("init.d/naosd", "etc/init.d/naosd", 0o755)):
        payload[dest] = ((ROOT / src).read_bytes(), mode)
    app = "SPDXRef-Core"
    primary = pkg(app, "naos-core", args.version, REPO + "/tree/" + revision)
    primary.update(packageFileName=exe, checksums=[checksum("SHA256", sha(payload[exe][0]))])
    packages = [primary]
    relations = [relation("SPDXRef-DOCUMENT", "DESCRIBES", app)]
    for manifest in sorted(manifests, key=lambda p: p.parent.name):
        crate = tomllib.loads(manifest.read_text())["package"]
        name, version = crate["name"], crate["version"]
        archive_hash = locked[name, version]
        identifier = "SPDXRef-Crate-" + archive_hash
        record = pkg(identifier, name, version, f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
                     crate.get("license", "NOASSERTION").replace("/", " OR "))
        record["checksums"] = [checksum("SHA256", archive_hash)]
        record["externalRefs"] = [{"referenceCategory": "PACKAGE-MANAGER", "referenceType": "purl",
                                   "referenceLocator": f"pkg:cargo/{name}@{version}"}]
        packages.append(record)
        relations.append(relation(app, "STATIC_LINK", identifier))
        directory = manifest.parent
        notices = {f for f in directory.iterdir() if f.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT"))}
        if crate.get("license-file"):
            notices.add(directory / crate["license-file"])
        for notice in sorted(notices):
            for f in sorted(notice.rglob("*") if notice.is_dir() else [notice]):
                if f.is_file() and f.resolve().is_relative_to(directory.resolve()):
                    payload[f"{LICENSES}/{name}-{version}/{f.relative_to(directory)}"] = (f.read_bytes(), 0o644)
    rust_version = rust.split()[1]
    std = pkg("SPDXRef-RustStd", "Rust standard library", rust_version,
              f"https://static.rust-lang.org/dist/rust-std-{rust_version}-{TARGET}.tar.xz")
    packages.append(std)
    relations.append(relation(app, "STATIC_LINK", std["SPDXID"]))
    docs = sysroot / "share/doc/rust"
    payload[f"{LICENSES}/rust/COPYRIGHT-library.html"] = ((docs / "COPYRIGHT-library.html").read_bytes(), 0o644)
    for f in sorted((docs / "licenses").glob("*.txt")):
        payload[f"{LICENSES}/rust/{f.name}"] = (f.read_bytes(), 0o644)
    native = sysroot / "lib/rustlib" / TARGET / "lib/self-contained"
    for name, archive in (("musl libc", "libc.a"), ("LLVM libunwind", "libunwind.a")):
        identifier = "SPDXRef-" + archive.split(".")[0]
        version = None
        if archive == "libc.a":
            versions = set(re.findall(r"^1\.\d+\.\d+$", output("strings", native / archive), re.MULTILINE))
            if len(versions) == 1:
                version = versions.pop()
        item = pkg(identifier, name, version, std["downloadLocation"])
        item["packageFileName"] = archive
        item["sourceInfo"] = f"{archive} from the Rust {rust_version} {TARGET} standard library distribution"
        item["checksums"] = [checksum("SHA256", sha((native / archive).read_bytes()))]
        packages.append(item)
        relations.append(relation(app, "STATIC_LINK", identifier))
    info = {"schema_version": 2, "version": args.version, "source_revision": revision, "target": TARGET,
            "rustc": rust, "cargo_lock_sha256": sha(lock), "binary_sha256": sha(payload[exe][0])}
    payload["build-info.json"] = (json_bytes(info), 0o644)
    files = []
    for path, (data, _) in sorted(payload.items()):
        identifier = "SPDXRef-File-" + sha(path.encode())
        files.append({"SPDXID": identifier, "fileName": path, "checksums": [
                          checksum("SHA1", hashlib.sha1(data).hexdigest()), checksum("SHA256", sha(data))],
                      "licenseConcluded": "NOASSERTION", "copyrightText": "NOASSERTION"})
        relations.append(relation(app, "CONTAINS", identifier))
    file_hashes = sorted(f["checksums"][0]["checksumValue"] for f in files)
    primary.update(filesAnalyzed=True, licenseInfoFromFiles=["NOASSERTION"],
                   packageVerificationCode={"packageVerificationCodeValue": hashlib.sha1("".join(file_hashes).encode()).hexdigest(),
                                            "packageVerificationCodeExcludedFiles": ["sbom.spdx.json"]})
    document = {"spdxVersion": "SPDX-2.3", "dataLicense": "CC0-1.0", "SPDXID": "SPDXRef-DOCUMENT",
                "name": STEM + "-" + args.version, "creationInfo": {"creators": ["Organization: NAOS"],
                "created": datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")},
                "comment": "Cargo library inputs come from this release build for the ARM64 target. "
                           "Link-time optimization may remove unused code.",
                "packages": packages, "relationships": relations, "files": files}
    document["documentNamespace"] = "https://spdx.org/spdxdocs/naos-core-" + sha(json_bytes(document))
    sbom = json_bytes(document)
    payload["sbom.spdx.json"] = (sbom, 0o644)
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / (STEM + ".tar.gz")
    with archive.open("wb") as raw, gzip.GzipFile(fileobj=raw, filename="", mode="wb", mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|", format=tarfile.GNU_FORMAT) as tar:
            for path, (data, mode) in sorted(payload.items()):
                entry = tarfile.TarInfo(path)
                entry.size, entry.mode, entry.mtime = len(data), mode, epoch
                tar.addfile(entry, io.BytesIO(data))
    sidecar = args.output / (STEM + ".spdx.json")
    sidecar.write_bytes(sbom)
    (args.output / "SHA256SUMS").write_text("".join(f"{sha(p.read_bytes())}  {p.name}\n" for p in (archive, sidecar)))
    print(f"{archive} ({len(manifests)} target crates, {len(files)} payload files)")


if __name__ == "__main__":
    main()
