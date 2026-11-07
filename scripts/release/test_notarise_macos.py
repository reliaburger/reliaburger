"""notarise_macos.sh skips cleanly without Apple secrets and signs before it notarises (#1583).

Apple's tools are stubs here, logging their arguments, so these tests prove
the script's decisions and its order, not Apple's answers. The real path
runs for the first time on a candidate build once the secrets exist.
"""
import base64
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("notarise_macos.sh")
SECRETS = ("APPLE_DEVELOPER_ID_P12", "APPLE_DEVELOPER_ID_P12_PASSWORD", "APPLE_NOTARY_KEY",
           "APPLE_NOTARY_KEY_ID", "APPLE_NOTARY_ISSUER_ID")

STUB = """#!/bin/sh
printf '%s' "$(basename "$0")" >> "$CALLS"
for argument do printf ' %s' "$argument" >> "$CALLS"; done
printf '\\n' >> "$CALLS"
{body}
"""
STUBS = {
    "security": """case $1 in
  find-identity) printf '  1) %s "Developer ID Application: Example (TEAM123456)"\\n     1 valid identities found\\n' "${IDENTITY_HASH:-ABC123}"
    [ -z "${SECOND_IDENTITY:-}" ] || printf '  2) DEF456 "Developer ID Application: Other (TEAM654321)"\\n' ;;
  list-keychains) [ "$2" = -d ] && [ "$#" -eq 3 ] && printf '    "/Users/runner/Library/Keychains/login.keychain-db"\\n' ;;
esac
exit 0""",
    "codesign": "",
    "ditto": "",
    "xcrun": """[ "$2" = submit ] && printf '{"id": "1111-2222", "status": "%s"}\\n' "${NOTARY_STATUS:-Accepted}"
exit 0""",
}


class NotariseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        tools = self.root / "tools"
        tools.mkdir()
        for name, body in STUBS.items():
            (tools / name).write_text(STUB.format(body=body))
            (tools / name).chmod(0o755)
        self.calls = self.root / "calls"
        self.binary = self.root / "relish-macos-aarch64"
        self.binary.write_text(f"#!/bin/sh\necho \"relish 0.2.7\" >> {self.calls}\n")
        self.binary.chmod(0o755)
        self.output = self.root / "github-output"
        self.summary = self.root / "summary"
        self.env = {key: value for key, value in os.environ.items() if key not in SECRETS}
        self.env.update(PATH=f"{tools}:{os.environ['PATH']}", CALLS=str(self.calls),
                        GITHUB_OUTPUT=str(self.output), GITHUB_STEP_SUMMARY=str(self.summary),
                        RUNNER_TEMP=str(self.root))

    def run_script(self, **secrets):
        return subprocess.run(["bash", str(SCRIPT), str(self.binary)], env=dict(self.env, **secrets),
                              capture_output=True, text=True)

    def all_secrets(self):
        return dict(APPLE_DEVELOPER_ID_P12=base64.b64encode(b"p12").decode(),
                    APPLE_DEVELOPER_ID_P12_PASSWORD="password",
                    APPLE_NOTARY_KEY=base64.b64encode(b"-----BEGIN PRIVATE KEY-----").decode(),
                    APPLE_NOTARY_KEY_ID="KEY1234567", APPLE_NOTARY_ISSUER_ID="issuer-uuid")

    def calls_made(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_without_secrets_it_says_so_and_leaves_the_binary_alone(self):
        before = self.binary.read_bytes()
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NOT notarised", result.stdout)
        self.assertIn("::notice", result.stdout)
        self.assertIn("NOT notarised", self.summary.read_text())
        self.assertEqual(self.output.read_text(), "notarised=false\n")
        self.assertEqual(self.calls_made(), [])
        self.assertEqual(self.binary.read_bytes(), before)

    def test_half_a_configuration_fails_and_names_what_is_missing(self):
        secrets = self.all_secrets()
        del secrets["APPLE_NOTARY_KEY_ID"]
        result = self.run_script(**secrets)
        self.assertEqual(result.returncode, 1)
        self.assertIn("APPLE_NOTARY_KEY_ID", result.stderr)
        self.assertEqual(self.calls_made(), [])

    def test_with_secrets_it_signs_then_notarises_then_runs_the_binary(self):
        result = self.run_script(**self.all_secrets())
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls_made()
        tools = [call.split()[0] for call in calls]
        sign = next(index for index, call in enumerate(calls) if call.startswith("codesign --force"))
        submit = next(index for index, call in enumerate(calls) if call.startswith("xcrun notarytool submit"))
        self.assertLess(sign, submit)
        self.assertLess(submit, calls.index("relish 0.2.7"), "the notarised binary must still run")
        signing = calls[sign]
        for flag in ("--options runtime", "--timestamp", "--sign ABC123", "--identifier com.reliaburger.relish"):
            self.assertIn(flag, signing)
        self.assertIn("--wait", calls[submit])
        self.assertIn("--key-id KEY1234567", calls[submit])
        self.assertIn("ditto", tools)
        self.assertIn("security delete-keychain", "\n".join(calls), "the throwaway keychain is removed")
        self.assertEqual(self.output.read_text(), "notarised=true\n")
        self.assertIn("1111-2222", self.summary.read_text())

    def test_a_rejected_submission_fails_and_fetches_apples_log(self):
        result = self.run_script(NOTARY_STATUS="Invalid", **self.all_secrets())
        self.assertEqual(result.returncode, 1)
        self.assertIn("Invalid", result.stderr)
        self.assertTrue(any(call.startswith("xcrun notarytool log 1111-2222") for call in self.calls_made()))
        self.assertFalse(self.output.exists())

    def test_it_refuses_a_p12_with_more_than_one_identity(self):
        result = self.run_script(SECOND_IDENTITY="1", **self.all_secrets())
        self.assertEqual(result.returncode, 1)
        self.assertIn("exactly one Developer ID Application identity", result.stderr)
        self.assertFalse(any(call.startswith("codesign") for call in self.calls_made()))

    @unittest.skipUnless(Path("/bin/bash").exists() and shutil.which("bash"), "needs bash")
    def test_macos_bash_3_runs_the_skip(self):
        # macOS's own /bin/bash is 3.2, which calls an empty array unbound.
        result = subprocess.run(["/bin/bash", str(SCRIPT), str(self.binary)], env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
