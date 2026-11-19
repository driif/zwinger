"""Run one macOS adapter test in GitHub-hosted CI using private copies of Apple's public empty fixture."""
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from urllib.request import urlopen

APPLE_REVISION = "db15acbe6a7f257a859ad9a3bb86097bfe0679d9"
APPLE_HEADER = "OSX/libsecurity_keychain/regressions/kc-30-xara-upgrade-helpers.h"
EMPTY_SIZE = 20_460
EMPTY_SHA256 = "e91b3a68e61aea2ee825ab3140ef03749ac9efdf0a5837cf887b31a3f5df5d04"
TEST = "legacy_keychain::hosted_smoke::only_the_fixture_service_is_imported_and_removed"

def get_empty_fixture():
    url = f"https://raw.githubusercontent.com/apple-oss-distributions/Security/{APPLE_REVISION}/{APPLE_HEADER}"
    with urlopen(url, timeout=30) as resp:
        header = resp.read(600_001)
    if len(header) > 600_000:
        raise ValueError("Unexpected Apple regression fixture header length.")
    declaration = rb"uint8_t empty_v512\[EMPTY_V512_SIZE\]\s*=\s*\{([^}]*)\};"
    matches = re.findall(declaration, header)
    if len(matches) != 1:
        raise ValueError("Apple's empty fixture declaration is missing or ambiguous.")
    literal = matches[0].strip().rstrip(b",")
    tokens = [token.strip() for token in literal.split(b",")]
    if len(tokens) != EMPTY_SIZE or not all(re.fullmatch(rb"0x[0-9a-fA-F]{2}", t) for t in tokens):
        raise ValueError("Unexpected Apple regression fixture encoding.")
    data = bytes(int(token, 16) for token in tokens)
    if hashlib.sha256(data).hexdigest() != EMPTY_SHA256:
        raise ValueError("Apple's empty fixture checksum does not match.")
    return data

def main():
    if (
        sys.platform != "darwin" or os.environ.get("GITHUB_ACTIONS") != "true"
        or os.environ.get("RUNNER_OS") != "macOS"
        or os.environ.get("ZWINGER_HOSTED_RUNNER") != "github-hosted"
        or not os.environ.get("GITHUB_RUN_ID", "").isdigit()
    ):
        raise SystemExit("This test requires the GitHub-hosted macOS CI step.")
    runner_tmp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="zwinger-legacy-smoke-", dir=runner_tmp) as tmp:
        empty = get_empty_fixture()
        for name in ("fixture.keychain-db", "other.keychain-db"):
            with (Path(tmp) / name).open("xb") as output:
                os.fchmod(output.fileno(), 0o600)
                output.write(empty)
        proc = subprocess.run(
            ["cargo", "test", "--locked", "--offline", "--manifest-path", "src-tauri/Cargo.toml", "--lib", TEST,
             "--", "--ignored", "--exact", "--test-threads=1"],
            env={**os.environ, "ZWINGER_HOSTED_KEYCHAIN_FIXTURE": str(Path(tmp) / "fixture.keychain-db")},
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=180
        )
        print(proc.stdout, end="")
        if proc.returncode:
            raise SystemExit(proc.returncode)
        if f"test {TEST} ... ok" not in proc.stdout or "1 passed; 0 failed;" not in proc.stdout:
            raise SystemExit("The named hosted smoke test did not run exactly once.")

if __name__ == "__main__":
    main()
