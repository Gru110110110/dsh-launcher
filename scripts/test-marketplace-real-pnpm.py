#!/usr/bin/env python3
"""Exercise marketplace transactions against real pnpm and a loopback-only registry.
All package/config/store/runtime fixtures live under one disposable directory.
No actual Harness runtime, production homes, credentials, or package scripts run.
"""
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit
from urllib.request import urlopen


def main():
    repo = Path(__file__).resolve().parent.parent
    node = shutil.which("node")
    if not node:
        raise SystemExit("Install Node before running this check")
    with tempfile.TemporaryDirectory(prefix="dsh-market-real-pnpm-") as directory:
        root = Path(directory)
        # Download only the pinned package manager into the fixture directory.
        # The system pnpm may delegate to a different version per working dir.
        with urlopen("https://registry.npmjs.org/pnpm/10.12.3", timeout=30) as response:
            metadata = json.load(response)
        with urlopen("https://registry.npmjs.org/pnpm/-/pnpm-10.12.3.tgz", timeout=30) as response:
            archive = response.read()
        integrity = "sha512-" + base64.b64encode(hashlib.sha512(archive).digest()).decode()
        if integrity != metadata["dist"]["integrity"]:
            raise SystemExit("Pinned pnpm integrity check failed")
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
            for member in tar.getmembers():
                if member.issym() or member.islnk():
                    raise SystemExit("Unexpected link in pnpm archive")
            tar.extractall(root / "pnpm", filter="data")
        pnpm = str(root / "pnpm/package/bin/pnpm.cjs")
        (root / "empty.npmrc").write_text("")
        packages = {}
        for name in ("provider", "consumer", "library", "plain-library", "bad-peer"):
            manifest = {"name": "dsh-market-fixture-" + name, "version": "1.0.0",
                        "scripts": {"install": "node -e \"require('fs').writeFileSync(process.env.DSH_FIXTURE_SCRIPT_MARKER, 'ran')\""}}
            if name not in ("library", "plain-library"):
                manifest["dsh"] = {"bundle": {"patch": {}}}
            if name in ("consumer", "bad-peer"):
                manifest["peerDependencies"] = {"dsh-market-fixture-provider": "^1.0.0" if name == "consumer" else "^2.0.0"}
            blob = io.BytesIO()
            with tarfile.open(fileobj=blob, mode="w:gz") as tar:
                data = json.dumps(manifest).encode()
                info = tarfile.TarInfo("package/package.json")
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            packages[manifest["name"]] = (manifest, blob.getvalue())

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                key = unquote(urlsplit(self.path).path).strip("/")
                tarball = key.endswith(".tgz")
                name = key[:-4] if tarball else key
                if name not in packages:
                    print("fixture registry missing:", key, flush=True)
                    self.send_error(404)
                    return
                manifest, data = packages[name]
                if not tarball:
                    version = dict(manifest)
                    version["dist"] = {"tarball": f"{registry}/{name}.tgz", "integrity": "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode()}
                    data = json.dumps({"name": name, "dist-tags": {"latest": "1.0.0"}, "versions": {"1.0.0": version}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream" if tarball else "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        registry = f"http://127.0.0.1:{server.server_port}"
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        bin_dir = root / "bin"
        bin_dir.mkdir()
        # A fresh allowlisted environment prevents inherited npm tokens/config
        # from entering even this loopback registry process.
        child_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "TMPDIR": str(root),
                     "npm_config_userconfig": str(root / "empty.npmrc"),
                     "npm_config_globalconfig": str(root / "global.npmrc"),
                     "npm_config_cache": str(root / "npm-cache"),
                     "npm_config_store_dir": str(root / "store"),
                     "npm_config_registry": registry, "npm_config_fetch_retries": "0",
                     "npm_config_update_notifier": "false",
                     "npm_config_manage_package_manager_versions": "false",
                     "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "data"),
                     "XDG_CACHE_HOME": str(root / "cache"), "XDG_STATE_HOME": str(root / "state"),
                     "DSH_FIXTURE_SCRIPT_MARKER": str(root / "scripts-ran")}
        child_env.update({key: str(root / key.lower()) for key in
                          ("DSH_DESKTOP_HOME", "DSH_HOME", "DSH_DESKTOP_SOURCE_HOME", "DSH_DESKTOP_CC_SWITCH_HOME")})
        (root / "global.npmrc").write_text("")
        wrapper = bin_dir / "pnpm"
        wrapper.write_text("#!/usr/bin/python3\nimport os, sys\nos.execve(" + repr(node) + ", " + repr([node, pnpm]) + " + sys.argv[1:], " + repr(child_env) + ")\n")
        wrapper.chmod(0o755)
        try:
            version = subprocess.check_output([str(wrapper), "--version"], cwd=root, text=True).strip()
            if version != "10.12.3":
                raise SystemExit(f"Expected pnpm 10.12.3, found {version}")
            env = dict(os.environ)
            env.update({key: value for key, value in child_env.items() if key.startswith("DSH_")})
            env.update({"DSH_MARKET_REAL_PNPM_BIN_DIR": str(bin_dir), "TMPDIR": str(root)})
            subprocess.run(["cargo", "test", "--offline", "-p", "dsh-core", "--lib",
                            "marketplace::review_tests::real_pnpm_group_lifecycle", "--", "--ignored", "--exact"],
                           cwd=repo, env=env, check=True)
            assert not (root / "scripts-ran").exists(), "lifecycle script unexpectedly executed"
            print("PASS: real pnpm group install, failure isolation, force peers, uninstall, rollback; scripts suppressed")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
