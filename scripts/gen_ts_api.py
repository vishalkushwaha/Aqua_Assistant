#!/usr/bin/env python3
"""Writes the TypeScript client of the Aqua API from documentation/openapi.json (card OA10-02, task UX-04).

    python scripts/gen_ts_api.py            writes apps/aqua-desktop/src/api/generated.ts
    python scripts/gen_ts_api.py --check    exits 1 when that file is not what the generator would write now
    python scripts/gen_ts_api.py --stdout   prints it

The document is itself generated from the router's source and from core/src/api_gateway/contract_schemas.rs by a test of the core
(`AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib openapi`), so the order is: change the Rust, regenerate the document, run this. The test
apps/aqua-desktop/tests/generatedApi.test.ts runs `--check`, so a document that changed without the client does not pass `npm test`.

What is written: one type for every schema of the document, the error codes, a class AquaClient with one method for every operation
(named by its operationId; the ones that have schemas are typed, the others take and give `unknown`), a typed reader of server-sent
streams and AquaApiError, which carries the `code` of an error. Nothing in the app reads it yet: the app's own files in src/api/ are not
changed by this script.

Only the standard library is used, and the file it writes is plain ASCII.
"""

import argparse
import json
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
SPEC = os.path.join(ROOT, "documentation", "openapi.json")
OUT = os.path.join(ROOT, "apps", "aqua-desktop", "src", "api", "generated.ts")

# A schema called like a name TypeScript (or the client) already has is written under another name.
TS_NAMES = {"Error": "ApiErrorBody"}

PRELUDE = r"""export interface ClientOptions {
  /** Where the core listens, without a trailing slash. */
  baseUrl?: string;
  /** The API token (or, for the app, the UI token): sent as `Authorization: Bearer <token>`. */
  token?: string;
  /** The function that makes the request (`fetch` by default). */
  fetch?: typeof fetch;
}

/** An answer that was not a success: its status and, when the core said, its `code` (see ERROR_CODES.md). */
export class AquaApiError extends Error {
  readonly status: number;
  readonly code: ErrorCode | "unknown";
  readonly hint: string | undefined;
  readonly retryable: boolean;
  readonly body: unknown;

  constructor(status: number, body: unknown) {
    const said = isErrorBody(body) ? body : undefined;
    super(said ? said.error : "the core answered " + status);
    this.name = "AquaApiError";
    this.status = status;
    this.code = said ? said.code : "unknown";
    this.hint = said ? said.hint : undefined;
    this.retryable = said ? said.retryable === true : false;
    this.body = body;
  }
}

/** Whether a body is an error of the contract: `{ error, code }`. */
export function isErrorBody(body: unknown): body is ApiErrorBody {
  if (typeof body !== "object" || body === null) return false;
  const found = body as { error?: unknown; code?: unknown };
  return typeof found.error === "string" && typeof found.code === "string" && (ERROR_CODES as ReadonlyArray<string>).includes(found.code);
}

/** One event of a server-sent stream: its name, its `id:` when it has one, and its data (JSON, read). */
export interface SseMessage<T> {
  event: string;
  id: string | undefined;
  data: T;
}

interface CallParts {
  path?: Record<string, string | number>;
  query?: Record<string, string | number | boolean | undefined>;
  body?: unknown;
}

export class AquaClient {
  private readonly baseUrl: string;
  private readonly token: string | undefined;
  private readonly fetcher: typeof fetch;

  constructor(options: ClientOptions = {}) {
    this.baseUrl = (options.baseUrl ?? "http://127.0.0.1:8767").replace(/\/+$/, "");
    this.token = options.token;
    this.fetcher = options.fetch ?? ((input, init) => fetch(input, init));
  }

  private address(template: string, parts: CallParts): string {
    let path = template;
    for (const [name, value] of Object.entries(parts.path ?? {})) {
      path = path.replace("{" + name + "}", encodeURIComponent(String(value)));
    }
    const query = Object.entries(parts.query ?? {})
      .filter((entry): entry is [string, string | number | boolean] => entry[1] !== undefined)
      .map(([name, value]) => encodeURIComponent(name) + "=" + encodeURIComponent(String(value)));
    return this.baseUrl + path + (query.length > 0 ? "?" + query.join("&") : "");
  }

  private async send(method: string, template: string, parts: CallParts, accept: string): Promise<Response> {
    const headers: Record<string, string> = { Accept: accept };
    if (this.token !== undefined) headers["Authorization"] = "Bearer " + this.token;
    const init: RequestInit = { method: method.toUpperCase(), headers };
    if (parts.body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(parts.body);
    }
    const response = await this.fetcher(this.address(template, parts), init);
    if (!response.ok) {
      const text = await response.text();
      let body: unknown = text;
      try {
        body = JSON.parse(text);
      } catch {
        // an answer that is not JSON (the framework's own refusal): the text is kept as it came
      }
      throw new AquaApiError(response.status, body);
    }
    return response;
  }

  /** One request with a JSON answer. */
  async request<T>(method: string, template: string, parts: CallParts = {}): Promise<T> {
    const response = await this.send(method, template, parts, "application/json");
    const text = await response.text();
    return (text === "" ? undefined : JSON.parse(text)) as T;
  }

  /** One request whose answer is a stream of server-sent events, read as they come. `M` is the kind of message: SseMessage of the data, or a union of them by event name. */
  async *stream<M>(method: string, template: string, parts: CallParts = {}): AsyncGenerator<M> {
    const response = await this.send(method, template, parts, "text/event-stream");
    if (response.body === null) return;
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let pending = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      pending += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
      for (let end = pending.indexOf("\n\n"); end >= 0; end = pending.indexOf("\n\n")) {
        const frame = pending.slice(0, end);
        pending = pending.slice(end + 2);
        let event = "message";
        let id: string | undefined;
        const data: string[] = [];
        for (const line of frame.split("\n")) {
          if (line.startsWith(":")) continue;
          const at = line.indexOf(":");
          const field = at < 0 ? line : line.slice(0, at);
          const text = at < 0 ? "" : line.slice(at + 1).replace(/^ /, "");
          if (field === "event") event = text;
          else if (field === "id") id = text;
          else if (field === "data") data.push(text);
        }
        if (data.length > 0) yield { event, id, data: JSON.parse(data.join("\n")) } as unknown as M;
      }
    }
  }
"""


def ts_name(name):
    return TS_NAMES.get(name, name)


def ascii_only(text):
    return text.encode("ascii", "replace").decode("ascii")


def doc_comment(text, indent):
    text = ascii_only(" ".join(str(text).split())).replace("*/", "* /")
    return "%s/** %s */" % (indent, text) if text else ""


def camel(word):
    parts = [p for p in re.split(r"[^A-Za-z0-9]+", word) if p]
    return parts[0] + "".join(p[:1].upper() + p[1:] for p in parts[1:]) if parts else word


def pascal(word):
    text = camel(word)
    return text[:1].upper() + text[1:]


def ref_name(schema):
    return ts_name(schema["$ref"].rsplit("/", 1)[-1])


def ts_type(schema, indent=""):
    """The TypeScript type of a schema of the vocabulary contract_schemas.rs uses."""
    if "$ref" in schema:
        return ref_name(schema)
    nullable = schema.get("nullable") is True
    if "allOf" in schema:
        text = ts_type(schema["allOf"][0], indent)
    elif "oneOf" in schema:
        text = "".join("\n%s  | %s" % (indent, ts_type(s, indent + "  ")) for s in schema["oneOf"])
        return "(%s) | null" % text.strip() if nullable else text
    elif "enum" in schema:
        text = " | ".join(json.dumps(w) for w in schema["enum"])
    else:
        kind = schema.get("type")
        if kind == "string":
            text = "string"
        elif kind in ("integer", "number"):
            text = "number"
        elif kind == "boolean":
            text = "boolean"
        elif kind == "array":
            text = "Array<%s>" % ts_type(schema.get("items", {}), indent)
        elif kind == "object":
            text = ts_object(schema, indent)
        else:
            return "unknown"
    return "%s | null" % text if nullable else text


def ts_object(schema, indent):
    props = schema.get("properties")
    if not props:
        return "Record<string, unknown>"
    required = set(schema.get("required", []))
    lines = ["{"]
    for name, sub in props.items():
        key = name if re.match(r"^[A-Za-z_][A-Za-z0-9_]*$", name) else json.dumps(name)
        lines.append("%s  %s%s: %s;" % (indent, key, "" if name in required else "?", ts_type(sub, indent + "  ")))
    lines.append("%s}" % indent)
    return "\n".join(lines)


def success_types(op):
    """What an operation answers (its 2xx answers) as a TypeScript type, and the schema of its stream when it is one."""
    seen = []
    stream = None
    for status, answer in op["responses"].items():
        if not status.startswith("2"):
            continue
        content = answer.get("content", {})
        if "text/event-stream" in content:
            stream = ref_name(content["text/event-stream"]["schema"])
        elif "application/json" in content:
            name = ref_name(content["application/json"]["schema"])
            if name not in seen:
                seen.append(name)
    return (" | ".join(seen) if seen else "unknown"), stream


def operation(path, method, op, types):
    """The method of the client for one operation. A stream with named events adds a type to `types`."""
    params = op.get("parameters", [])
    path_params = [p for p in params if p["in"] == "path"]
    query_params = [p for p in params if p["in"] == "query"]
    required, optional = [], []
    for p in path_params:
        required.append("%s: %s" % (camel(p["name"]), "number" if p["schema"]["type"] == "integer" else "string"))
    body = op.get("requestBody")
    if body:
        kind = ref_name(body["content"]["application/json"]["schema"])
        kind = "Record<string, unknown>" if kind == "Untyped" else kind
        (required if body.get("required") else optional).append("body%s: %s" % ("" if body.get("required") else "?", kind))
    if query_params:
        fields = "; ".join("%s?: %s" % (q["name"], "number" if q["schema"]["type"] == "integer" else "string") for q in query_params)
        optional.append("query: { %s } = {}" % fields)
    args = ", ".join(required + optional)
    parts = []
    if path_params:
        parts.append("path: { %s }" % ", ".join("%s: %s" % (json.dumps(p["name"]), camel(p["name"])) for p in path_params))
    if query_params:
        parts.append("query")
    if body:
        parts.append("body")
    call = "%s, %s, { %s }" % (json.dumps(method), json.dumps(path), ", ".join(parts))
    answer, stream = success_types(op)
    note = "%s %s. %s (token: %s%s)" % (method.upper(), path, op.get("summary", ""), op.get("x-aqua-token", "api"), "" if op.get("x-aqua-typed") else "; the body is not described yet")
    operation_id = op["operationId"]
    out = [doc_comment(note, "  ")]
    if stream:
        events = op.get("x-aqua-events")
        if events:
            message = "%sMessage" % pascal(operation_id)
            kinds = "".join("\n  | { event: %s; id: string | undefined; data: %s }" % (json.dumps(n), ref_name(r)) for n, r in events.items())
            types.append("/** One message of the stream of %s %s, by the name of its event. */\nexport type %s =%s;" % (method.upper(), path, message, kinds))
        else:
            message = "SseMessage<%s>" % stream
        out.append("  %s(%s): AsyncGenerator<%s> {" % (operation_id, args, message))
        out.append("    return this.stream<%s>(%s);" % (message, call))
    else:
        out.append("  %s(%s): Promise<%s> {" % (operation_id, args, answer))
        out.append("    return this.request<%s>(%s);" % (answer, call))
    out.append("  }")
    return "\n".join(out)


def render(spec):
    schemas = spec["components"]["schemas"]
    head = [
        "// GENERATED by scripts/gen_ts_api.py from documentation/openapi.json. Do not edit by hand:",
        "// change the Rust (core/src/api_gateway/contract_schemas.rs), regenerate the document",
        "// (AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib openapi) and run the script again.",
        "// apps/aqua-desktop/tests/generatedApi.test.ts fails when this file is not what the script writes.",
        "",
        "/** The contract this file was written for: every answer of the core carries it as the header x-aqua-contract, and GET /api/v1/version says it. */",
        "export const CONTRACT_VERSION = %d;" % spec["info"]["x-aqua-contract"],
        "",
        "/** Every code of an error body (documentation/ERROR_CODES.md). */",
        "export const ERROR_CODES = [%s] as const;" % ", ".join(json.dumps(c) for c in schemas["ErrorCode"]["enum"]),
        "export type ErrorCode = (typeof ERROR_CODES)[number];",
        "",
        "// ---- the schemas of the bodies " + "-" * 60,
        "",
    ]
    blocks = []
    for name, schema in schemas.items():
        if name == "ErrorCode":
            continue
        body = ts_type(schema)
        block = "export type %s =%s;" % (ts_name(name), body if body.startswith("\n") else " " + body)
        if schema.get("description"):
            block = doc_comment(schema["description"], "") + "\n" + block
        blocks.append(block)
    seen = {}
    methods = []
    types = []
    for path, operations in spec["paths"].items():
        for method, op in operations.items():
            if op["operationId"] in seen:
                sys.exit("two operations are called %s: %s and %s %s" % (op["operationId"], seen[op["operationId"]], method, path))
            seen[op["operationId"]] = "%s %s" % (method, path)
            methods.append(operation(path, method, op, types))
    text = "\n".join(head) + "\n\n".join(blocks + types) + "\n\n"
    text += "// ---- the client " + "-" * 75 + "\n\n"
    text += PRELUDE
    text += "\n" + "\n\n".join(methods) + "\n}\n"
    return ascii_only(text)


def build():
    with open(SPEC, "r", encoding="utf-8") as handle:
        return render(json.load(handle))


def main():
    parser = argparse.ArgumentParser(description="Write apps/aqua-desktop/src/api/generated.ts from documentation/openapi.json")
    parser.add_argument("--check", action="store_true", help="exit 1 when the file on disk is not what would be written")
    parser.add_argument("--stdout", action="store_true", help="print the text instead of writing it")
    parser.add_argument("--out", default=OUT, help="where to write (default: %(default)s)")
    args = parser.parse_args()
    text = build()
    if args.stdout:
        # bytes, so that Windows does not turn each newline into two characters
        sys.stdout.buffer.write(text.encode("ascii"))
        return 0
    if args.check:
        try:
            with open(args.out, "r", encoding="utf-8", newline="") as handle:
                have = handle.read().replace("\r\n", "\n")
        except OSError:
            have = ""
        if have != text:
            sys.stderr.write("%s is not what scripts/gen_ts_api.py writes now: run python scripts/gen_ts_api.py\n" % os.path.relpath(args.out, ROOT))
            return 1
        return 0
    with open(args.out, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    sys.stdout.write("wrote %s\n" % os.path.relpath(args.out, ROOT))
    return 0


if __name__ == "__main__":
    sys.exit(main())
