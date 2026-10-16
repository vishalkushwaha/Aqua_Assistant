"""Record one exchange with a model service as a cassette (card OA9-05, task PR-09 of the OpenCode audit). A MANUAL tool: it makes a real request.

    RECORD=1 python scripts/record_provider_cassette.py ollama_sse --url http://127.0.0.1:11434/v1/chat/completions --body-file prompt.json --provider ollama --stream
    RECORD=1 python scripts/record_provider_cassette.py openai_tool_call --url https://api.example.com/v1/chat/completions --body-file prompt.json --provider openai-compat --stream --remote --key-name openai/key

(In PowerShell: `$env:RECORD = "1"` first, and remove it afterwards.)

It sends the JSON in `--body-file` (a prompt about a SYNTHETIC workspace: write one, do not send a real project's text) to `--url` and writes the whole exchange to
`core/tests/fixtures/provider/<name>.json`, the file `core/src/model/cassette.rs` reads and `core/tests/provider_cassette_test.rs` replays with no network. It is run by a person, on
purpose: nothing in the test suite, the gate or the build runs it, and it needs no key for a model on this machine.

What it refuses, and why:

* It does nothing unless the environment says `RECORD=1` (an accidental run makes no request).
* It never overwrites a cassette: a fixture is data, and a recording that replaces one hides what changed. Record under a new name, and delete the old one by hand if it is to go.
* A service on another machine needs `--remote`: the prompt leaves this machine, and a service may bill for it. A loopback address (`127.0.0.1`, `::1`, `localhost`) does not.
* A key is read by NAME from the credential store (`--key-name openai/key` is the Windows Credential Manager entry `Aqua/openai/key`, written by `aqua-cli secret-set`), put in the
  `Authorization` header (or the header `--key-header` names) of the one request, and never printed, never put in a file. A key goes only to https, or to a loopback address.
* The file is checked before it is written, and not written when it fails: a credential header (any name with authorization, cookie, key, token, secret, organization, project,
  account or session in it) is written as `[REDACTED]`; the key and its base64 and URL forms are replaced wherever they appear; this person's home folder and e-mail addresses are
  replaced by made-up ones (`C:\\work`, `user@example.com`); then the rules of the secret scan before a commit (`scripts/hooks/secret_patterns.txt`) are run over all of it, and a
  folder of a person's own that is still in the text refuses the recording whole. It says what kind of thing it found and never what it was.

What it prints: the name of the file, the status, the number of pieces and bytes. Nothing of the request or the answer.

The pieces are what `http.client` hands back one read at a time (`read1`), with the chunked framing already taken off, so a piece that ends inside a character is written as base64
(`body_encoding`), and any other body as text. A connection that ends before the service has finished is written with `"truncated": true`.

This script has not been run (card OA9-05 had no network, no key and no model); the format it writes is the one `cassette.rs` reads, and `Cassette::problems()` holds the file to the
same rules this script does, so run `cargo test -p aqua-core --features test-support --test provider_cassette_test` after recording one (a new cassette also needs its name in `CASSETTES`
there, and a test).
"""
import argparse
import base64
import http.client
import json
import os
import re
import sys
import urllib.parse
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
FIXTURES = REPO / "core" / "tests" / "fixtures" / "provider"
SECRET_PATTERNS = HERE / "hooks" / "secret_patterns.txt"
FORMAT = 1
MASK = "[REDACTED]"
CREDENTIAL_WORDS = ("authorization", "cookie", "key", "token", "secret", "organization", "project", "account", "session")
LOOPBACK = ("127.0.0.1", "::1", "localhost")
NAME_RE = re.compile(r"^[a-z0-9_]{1,60}$")
SECRET_NAME_RE = re.compile(r"^[A-Za-z0-9_./-]{1,100}$")
PERSONAL_PATH = re.compile(r"(?i)(?:\b[a-z]:[\\/]{1,4}users[\\/]{1,4}|/users/|/home/)[^\\/\"'\s]+")
EMAIL = re.compile(r"(?i)\b[a-z0-9._%+-]+@((?:[a-z0-9-]+\.)+[a-z]{2,})\b")


class RecorderError(Exception):
    """Something this command refuses or cannot do: said in a sentence, exit code 2."""


# ---- the credential store ------------------------------------------------------------------------------------------
def read_credential(name: str) -> str:
    """The value of the Windows Credential Manager entry `Aqua/<name>` (a generic credential whose blob is the UTF-8 text). Never printed."""
    if not SECRET_NAME_RE.match(name) or name.startswith("/") or ".." in name:
        raise RecorderError("the key name has characters a secret name does not (letters, digits, _ - / . only)")
    if sys.platform != "win32":
        raise RecorderError("the credential store is Windows Credential Manager, and this is not Windows")
    import ctypes
    from ctypes import wintypes

    class FILETIME(ctypes.Structure):
        _fields_ = [("dwLowDateTime", wintypes.DWORD), ("dwHighDateTime", wintypes.DWORD)]

    class CREDENTIAL(ctypes.Structure):
        _fields_ = [
            ("Flags", wintypes.DWORD),
            ("Type", wintypes.DWORD),
            ("TargetName", wintypes.LPWSTR),
            ("Comment", wintypes.LPWSTR),
            ("LastWritten", FILETIME),
            ("CredentialBlobSize", wintypes.DWORD),
            ("CredentialBlob", ctypes.POINTER(ctypes.c_ubyte)),
            ("Persist", wintypes.DWORD),
            ("AttributeCount", wintypes.DWORD),
            ("Attributes", ctypes.c_void_p),
            ("TargetAlias", wintypes.LPWSTR),
            ("UserName", wintypes.LPWSTR),
        ]

    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    cred_read = advapi32.CredReadW
    cred_read.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(ctypes.POINTER(CREDENTIAL))]
    cred_read.restype = wintypes.BOOL
    cred_free = advapi32.CredFree
    cred_free.argtypes = [ctypes.c_void_p]
    found = ctypes.POINTER(CREDENTIAL)()
    if not cred_read("Aqua/" + name, 1, 0, ctypes.byref(found)):
        code = ctypes.get_last_error()
        if code == 1168:
            raise RecorderError("the credential store has no entry called Aqua/<the key name>")
        raise RecorderError("the credential store gave error %d" % code)
    try:
        entry = found.contents
        raw = bytes(bytearray(entry.CredentialBlob[: entry.CredentialBlobSize]))
    finally:
        cred_free(found)
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        raise RecorderError("the stored key is not UTF-8 text")


# ---- masking -------------------------------------------------------------------------------------------------------
def is_credential_header(name: str) -> bool:
    lower = name.lower()
    return any(word in lower for word in CREDENTIAL_WORDS)


def secret_forms(secret: str) -> List[str]:
    """The key as it may appear in a text: itself, base64 (both alphabets, with and without padding) and percent-encoded. Short values are not masked (they would damage the text)."""
    if len(secret) < 6:
        return []
    raw = secret.encode("utf-8")
    forms = {secret, urllib.parse.quote(secret, safe=""), base64.b64encode(raw).decode("ascii"), base64.urlsafe_b64encode(raw).decode("ascii")}
    forms |= {f.rstrip("=") for f in list(forms)}
    return sorted((f for f in forms if len(f) >= 6), key=len, reverse=True)


def home_variants(homes: List[str]) -> List[Tuple["re.Pattern[str]", str]]:
    """Each home folder as it is written in plain text, in JSON (backslashes doubled) and with forward slashes, with the made-up folder to put in its place."""
    out: List[Tuple["re.Pattern[str]", str]] = []
    for home in homes:
        home = home.rstrip("\\/")
        if len(home) < 4:
            continue
        plain, doubled, forward = home, home.replace("\\", "\\\\"), home.replace("\\", "/")
        for variant, replacement in ((doubled, "C:\\\\work"), (plain, "C:\\work"), (forward, "C:/work")):
            out.append((re.compile(re.escape(variant), re.IGNORECASE), replacement))
    return out


def load_scan_rules() -> Tuple[List[Tuple[str, "re.Pattern[str]"]], int]:
    """The rules of the secret scan before a commit (kind, pattern); and how many could not be compiled by Python's `re`."""
    rules: List[Tuple[str, "re.Pattern[str]"]] = []
    skipped = 0
    if not SECRET_PATTERNS.is_file():
        raise RecorderError("scripts/hooks/secret_patterns.txt is missing: the recording could not be checked, so it is not made")
    for line in SECRET_PATTERNS.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#") or "\t" not in line:
            continue
        kind, pattern = line.split("\t", 1)
        try:
            rules.append((kind, re.compile(pattern)))
        except re.error:
            skipped += 1
    return rules, skipped


class Masker:
    def __init__(self, secrets: List[str], homes: List[str]) -> None:
        self.secret_forms = [f for s in secrets for f in secret_forms(s)]
        self.homes = home_variants(homes)

    def text(self, value: str) -> str:
        """A text (a header, a url, a string of a JSON body, a stream of events): the key, this person's folders and addresses replaced. A folder is replaced in the three forms it can
        be written in (plain, with its backslashes doubled as JSON writes them, with forward slashes), so a stream of JSON stays JSON."""
        for form in self.secret_forms:
            value = value.replace(form, MASK)
        for pattern, replacement in self.homes:
            value = pattern.sub(lambda _m, r=replacement: r, value)
        return EMAIL.sub(lambda m: m.group(0) if m.group(1).lower().startswith("example.") else "user@example.com", value)

    def json(self, value: Any) -> Any:
        if isinstance(value, str):
            return self.text(value)
        if isinstance(value, list):
            return [self.json(v) for v in value]
        if isinstance(value, dict):
            return {k: self.json(v) for k, v in value.items()}
        return value


def check_text(what: str, text: str, rules: List[Tuple[str, "re.Pattern[str]"]]) -> List[str]:
    """What is wrong with a text that is to be written: the kinds the secret scan finds, a folder of a person's own that is left. Never the text."""
    problems = []
    kinds = sorted({kind for kind, rule in rules if rule.search(text)})
    if kinds:
        problems.append("%s looks like it holds a secret (%s)" % (what, ", ".join(kinds)))
    if PERSONAL_PATH.search(text):
        problems.append("%s names a folder of a person's own that could not be replaced" % what)
    return problems


# ---- the exchange --------------------------------------------------------------------------------------------------
def decode_pieces(pieces: List[bytes]) -> Tuple[str, List[str]]:
    """The body as the file holds it: `utf8` with the pieces as text, or `base64` when any piece ends inside a character."""
    try:
        return "utf8", [p.decode("utf-8") for p in pieces]
    except UnicodeDecodeError:
        return "base64", [base64.b64encode(p).decode("ascii") for p in pieces]


def exchange(url: str, headers: Dict[str, str], body: bytes, timeout: float) -> Tuple[int, List[Tuple[str, str]], List[bytes], bool]:
    """One POST: the status, the headers, the body in the pieces it was read in, and whether the connection ended before the service had finished."""
    parts = urllib.parse.urlsplit(url)
    connection_class = http.client.HTTPSConnection if parts.scheme == "https" else http.client.HTTPConnection
    connection = connection_class(parts.hostname, parts.port, timeout=timeout)
    target = parts.path or "/"
    if parts.query:
        target += "?" + parts.query
    try:
        connection.request("POST", target, body=body, headers=headers)
        response = connection.getresponse()
        got_headers = [(name, value) for name, value in response.getheaders()]
        pieces: List[bytes] = []
        truncated = False
        while True:
            try:
                piece = response.read1(65536)
            except http.client.IncompleteRead as e:
                if e.partial:
                    pieces.append(bytes(e.partial))
                truncated = True
                break
            if not piece:
                break
            pieces.append(piece)
        return response.status, got_headers, pieces, truncated
    finally:
        connection.close()


def build_cassette(args: argparse.Namespace, request_body: Any, request_headers: Dict[str, str], status: int, response_headers: List[Tuple[str, str]], pieces: List[bytes],
                   truncated: bool, secrets: List[str], homes: List[str], key_header: Optional[str]) -> Dict[str, Any]:
    masker = Masker(secrets, homes)

    def headers_of(pairs: List[Tuple[str, str]]) -> Dict[str, str]:
        merged: Dict[str, str] = {}
        for header, value in pairs:
            key = header.lower()
            # The header the key was sent in is masked whatever it is called.
            value = MASK if (is_credential_header(header) or key == (key_header or "").lower()) else masker.text(value)
            merged[key] = (merged[key] + ", " + value) if key in merged else value
        return dict(sorted(merged.items()))

    try:
        text = b"".join(pieces).decode("utf-8")
    except UnicodeDecodeError:
        raise RecorderError("the response body is not text, and a cassette holds text")
    cleaned = masker.text(text)
    if cleaned != text:
        encoding, body = "utf8", [cleaned]
    else:
        encoding, body = decode_pieces(pieces)
    response: Dict[str, Any] = {"status": status, "headers": headers_of(response_headers), "body_encoding": encoding, "body": body}
    if truncated:
        response["truncated"] = True
    note = args.note or "Recorded by scripts/record_provider_cassette.py from a real exchange (the credential headers, the key, this person's folders and addresses masked)."
    return {
        "format": FORMAT,
        "provider": args.provider,
        "protocol": args.protocol,
        "note": masker.text(note),
        "interactions": [
            {
                "request": {"method": "POST", "url": masker.text(args.url), "headers": headers_of(sorted(request_headers.items())), "body": masker.json(request_body)},
                "response": response,
            }
        ],
    }


def problems_of(cassette: Dict[str, Any], rules: List[Tuple[str, "re.Pattern[str]"]]) -> List[str]:
    """The checks of `Cassette::problems()` in `cassette.rs`, over the text that would be written."""
    problems: List[str] = []
    for i, interaction in enumerate(cassette["interactions"], 1):
        at = "interaction %d" % i
        for side in ("request", "response"):
            for header, value in interaction[side]["headers"].items():
                if is_credential_header(header) and value != MASK:
                    problems.append("%s: the %s header %s holds a credential that is not masked" % (at, side, header))
                problems += check_text("%s: the %s header %s" % (at, side, header), "%s: %s" % (header, value), rules)
        problems += check_text(at + ": the url", interaction["request"]["url"], rules)
        problems += check_text(at + ": the request body", json.dumps(interaction["request"]["body"]), rules)
        response = interaction["response"]
        pieces = response["body"]
        joined = "".join(pieces if response["body_encoding"] == "utf8" else [base64.b64decode(p).decode("utf-8", "replace") for p in pieces])
        problems += check_text(at + ": the response body", joined, rules)
    problems += check_text("the note", cassette.get("note", ""), rules)
    return problems


# ---- the command ---------------------------------------------------------------------------------------------------
def parse_args(argv: Optional[List[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Record one exchange with a model service as a cassette (needs RECORD=1; makes a real request).")
    parser.add_argument("name", help="the name of the cassette: lower-case letters, digits and _ (the file is core/tests/fixtures/provider/<name>.json)")
    parser.add_argument("--url", required=True, help="the full url, for example http://127.0.0.1:11434/v1/chat/completions")
    parser.add_argument("--body-file", required=True, help="the JSON request body (a prompt about a synthetic workspace)")
    parser.add_argument("--provider", required=True, help="who answers: llama.cpp, ollama, openai-compat ...")
    parser.add_argument("--protocol", default="openai-chat", help="how it is spoken to (only openai-chat is read by the tests today)")
    parser.add_argument("--stream", action="store_true", help='ask for a stream: sets "stream": true and the Accept header')
    parser.add_argument("--remote", action="store_true", help="allow a service that is not on this machine (the prompt leaves it)")
    parser.add_argument("--key-name", help="the name of the key in the credential store (Aqua/<name>); the key is never printed or written")
    parser.add_argument("--key-header", default="Authorization", help="the header the key goes in (Authorization takes it as a Bearer token; any other gets it as it is)")
    parser.add_argument("--home", action="append", default=[], help="another folder to replace by C:\\work (this person's home folder always is); may be repeated")
    parser.add_argument("--timeout", type=float, default=120.0, help="seconds to wait for the service")
    parser.add_argument("--note", help="what the file shows (default: a line that says it is a recording)")
    return parser.parse_args(argv)


def record(args: argparse.Namespace, env: Optional[Dict[str, str]] = None) -> Path:
    env = os.environ if env is None else env
    if env.get("RECORD") != "1":
        raise RecorderError("this makes a real request: set RECORD=1 to say that you mean it")
    if not NAME_RE.match(args.name):
        raise RecorderError("the name is lower-case letters, digits and _ only (at most 60)")
    target = FIXTURES / (args.name + ".json")
    if target.exists():
        raise RecorderError("the cassette %s exists, and a recording never replaces one: record under another name" % args.name)
    parts = urllib.parse.urlsplit(args.url)
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise RecorderError("the url is not an http or https url")
    loopback = parts.hostname.lower() in LOOPBACK
    if not loopback and not args.remote:
        raise RecorderError("the service is not on this machine: the prompt would leave it, so say --remote")
    if args.key_name and not loopback and parts.scheme != "https":
        raise RecorderError("a key is sent only over https, or to this machine")
    body_path = Path(args.body_file)
    try:
        request_body = json.loads(body_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        raise RecorderError("the body file cannot be read as JSON")
    if args.stream and isinstance(request_body, dict):
        request_body["stream"] = True
    homes = [os.path.expanduser("~")] + list(args.home)
    rules, skipped = load_scan_rules()

    key: Optional[str] = read_credential(args.key_name) if args.key_name else None
    wire_headers = {"Content-Type": "application/json", "Accept": "text/event-stream" if args.stream else "application/json"}
    if key is not None:
        wire_headers[args.key_header] = ("Bearer " + key) if args.key_header.lower() == "authorization" else key
    status, response_headers, pieces, truncated = exchange(args.url, wire_headers, json.dumps(request_body).encode("utf-8"), args.timeout)

    # What is written has the credential header values masked whatever they were, and the key replaced wherever it shows.
    secrets = [key] if key else []
    cassette = build_cassette(args, request_body, wire_headers, status, response_headers, pieces, truncated, secrets, homes, args.key_header if key else None)
    text = json.dumps(cassette, indent=2, ensure_ascii=False) + "\n"
    found = problems_of(cassette, rules)
    if key and any(form in text for form in secret_forms(key)):
        found.append("the key is still in the file")
    if found:
        raise RecorderError("the recording is refused whole, and nothing was written:\n  " + "\n  ".join(found))
    FIXTURES.mkdir(parents=True, exist_ok=True)
    with open(target, "x", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    print("wrote %s: status %d, %d pieces, %d bytes%s" % (target.relative_to(REPO).as_posix(), status, len(pieces), sum(len(p) for p in pieces), ", truncated" if truncated else ""))
    if skipped:
        print("note: %d rules of the secret scan could not be run by Python's re; the commit hook still has them" % skipped)
    return target


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv)
    try:
        record(args)
    except RecorderError as e:
        print("refused: %s" % e, file=sys.stderr)
        return 2
    except (OSError, http.client.HTTPException) as e:
        print("the request failed (%s)" % type(e).__name__, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
