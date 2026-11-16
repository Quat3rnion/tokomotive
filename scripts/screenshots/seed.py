#!/usr/bin/env python3
"""Seed demo data through the real dashboard forms, never a live instance."""
import json
import re

import requests
from env import BASE as B, MOCK, PASSWORD as PW, KEYS, WORKDIR

s = requests.Session()
s.headers["Origin"] = B


def post(path, data, ok=(200, 303)):
    r = s.post(B + path, data=data, allow_redirects=False, timeout=30)
    if r.status_code not in ok or re.search(r'class="(?:form-error|error)"[^>]*>\s*[^<\s]', r.text):
        raise RuntimeError(f"Seed failed: {path} HTTP {r.status_code}: {r.text[:500]}")
    return r


post("/setup", {"mode": "full", "password": PW, "password_confirm": PW})
post("/admin/login", {"password": PW})
assert "tokomotive_session" in s.cookies.get_dict(), "no session"

# Single sign-on screen: leave disabled; no real identity provider is needed.
post("/admin/settings/oidc", {"issuer": "https://auth.example.com/application/o/tokomotive/",
    "public_base_url": B, "client_id": "tokomotive-demo", "client_secret": "",
    "scopes": "openid profile email groups", "button_label": "Authentik",
    "groups_claim": "groups", "group_0": "toko-admins", "role_0": "admin",
    "group_1": "toko-readers", "role_1": "viewer", "default_role": "deny",
    "token_auth": "client_secret_basic", "allow_private": "on", "dashboard_https": ""})

# Secret stores screen: no real credential or remote service is needed.
post("/admin/settings/secrets", {"name": "team-vault", "kind": "vault", "address": MOCK,
    "typed": "true", "vault_auth": "token", "vault_token": "env:DEMO_VAULT_TOKEN",
    "vault_namespace": "research", "vault_kv_version": "2", "vault_browse_root": "kv",
    "allow_private": "on", "is_default": "on", "refresh_secs": "900"})

post("/admin/providers", {"name": "openai", "auth": "api_key", "kind": "openai", "base_url": MOCK, "api_key": "sk-demo-openai-primary-7f3a"})
post("/admin/providers/openai/keys", {"api_key": "sk-demo-openai-backup-91c2"})
post("/admin/providers", {"name": "anthropic", "auth": "api_key", "kind": "anthropic", "base_url": MOCK, "api_key": "sk-demo-ant-main-4b8e"})
post("/admin/providers/anthropic/keys", {"api_key": "env:ANTHROPIC_API_KEY_2"})
post("/admin/providers", {"name": "chatgpt-plus", "auth": "chatgpt", "kind": "openai", "base_url": "https://api.openai.com", "api_key": ""})
post("/admin/providers", {"name": "claude-max", "auth": "claude", "kind": "anthropic", "base_url": "https://api.anthropic.com", "api_key": ""})
post("/admin/providers", {"name": "codex-cli", "auth": "codex", "kind": "openai", "base_url": "https://chatgpt.com/backend-api/codex", "api_key": ""})
post("/admin/providers", {"name": "copilot", "auth": "github_copilot", "kind": "openai", "base_url": "https://api.githubcopilot.com", "api_key": ""})
post("/admin/settings/markup", {"default_markup_percent": "20"})

Z = dict(input="", output="", cache_read="", cache_write_5m="", cache_write_1h="", image_input="", image_output="", tiers="")
MODELS = [
    ("gpt-6.1-sol", "openai", "", dict(input="2.00", output="10.00", cache_read="0.10")),
    ("gpt-6-luna", "openai", "luna", dict(input="0.10", output="0.50", cache_read="0.01")),
    ("gpt-4o-mini", "openai", "", dict(input="0.15", output="0.60", cache_read="0.075")),
    ("text-embedding-3-small", "openai", "", dict(input="0.02", output="0")),
    ("claude-opus-5-5", "anthropic", "opus", dict(input="4.00", output="20.00", cache_read="0.20", cache_write_5m="5.00", cache_write_1h="8.00")),
    ("claude-sonnet-5-5", "anthropic", "sonnet", dict(input="2.00", output="10.00", cache_read="0.10", cache_write_5m="2.50", cache_write_1h="4.00")),
    ("claude-haiku-5-5", "anthropic", "haiku", dict(input="0.10", output="0.50", cache_read="0.01", cache_write_5m="0.125", cache_write_1h="0.20", tiers="100000 0.50 2.50 0.05 0.625 1.00")),
]
for name, provider, aliases, rates in MODELS:
    post("/admin/models", dict(Z, name=name, provider=provider, aliases=aliases, **rates))
html = s.get(B + "/admin/models", timeout=30).text
ids = {m.group(2): m.group(1) for m in re.finditer(r'href="/admin/models/(\d+)"[^>]*>([^<]+)<', html)}
post("/admin/teams", {"name": "Research"})
post("/admin/teams", {"name": "Product"})
post(f"/admin/models/{ids['claude-sonnet-5-5']}/price", dict(Z, mode="markup", markup_percent="25"))
post(f"/admin/models/{ids['gpt-6.1-sol']}/team-price", dict(Z, team_name="Research", mode="markup", markup_percent="10"))
post(f"/admin/models/{ids['claude-sonnet-5-5']}/team-price", dict(Z, team_name="Product", mode="markup", markup_percent="15"))
post(f"/admin/models/{ids['gpt-6-luna']}/price", dict(Z, mode="rates", input="0.20", output="1.00", cache_read="0.02"))
post("/admin/routes", {"name": "smart", "strategy": "weighted", "fallbacks": "claude-haiku-5-5, gpt-4o-mini", "deployment_provider": ["openai", "anthropic", "anthropic"], "deployment_model": ["gpt-6.1-sol", "claude-sonnet-5-5", "claude-opus-5-5"], "deployment_weight": ["5", "3", "1"]})
post("/admin/routes", {"name": "fast", "strategy": "priority", "fallbacks": "gpt-4o-mini", "deployment_provider": ["openai", "anthropic"], "deployment_model": ["gpt-6-luna", "claude-haiku-5-5"], "deployment_weight": ["1", "1"]})
# A request rule on "smart": tool requests use Opus, with a 10% Canary on Sonnet.
post("/admin/routes/smart/rules", {
    "rule[0][name]": "tools-to-opus", "rule[0][enabled]": "on", "rule[0][has_tools]": "true",
    "rule[0][action]": "deployments", "rule[0][deployments]": json.dumps(["anthropic", "claude-opus-5-5"]),
    "rule[0][canary_percent]": "10", "rule[0][canary_action]": "deployments",
    "rule[0][canary_deployments]": json.dumps(["anthropic", "claude-sonnet-5-5"]),
})
# The rule editor form: one detector, one regex and one keyword rule.
post("/admin/guardrails", {
    "name": "Contact protection", "max_text_bytes": "1048576", "hook_url": "",
    "rule[0][name]": "EMAIL", "rule[0][kind]": "detector", "rule[0][detector]": "email",
    "rule[0][action]": "redact", "rule[0][direction]": "both",
    "rule[1][name]": "TICKET", "rule[1][kind]": "regex", "rule[1][pattern]": r"(?i)\bPRJ-[0-9]{4}\b",
    "rule[1][action]": "redact", "rule[1][direction]": "request",
    "rule[2][name]": "CODENAMES", "rule[2][kind]": "keywords", "rule[2][words]": "Bluebird\nNightjar",
    "rule[2][whole_word]": "on", "rule[2][action]": "block", "rule[2][direction]": "both",
})
guard_html = s.get(B + "/admin/guardrails", timeout=30).text
guard_id = re.search(r'action="/admin/guardrails/(\d+)/attach"', guard_html).group(1)
post(f"/admin/guardrails/{guard_id}/attach", {"route_name": "smart", "team_name": "", "key_id": ""})

USERS = [
    ("maria.keller", "Research", "250", "600", "4000000", True),
    ("jonas.berg", "Research", "100", "300", "", True),
    ("priya.nair", "Product", "150", "", "", True),
    ("lena.fischer", "Product", "80", "120", "1000000", True),
    ("ci-pipeline", "", "40", "600", "", False),
    ("a-very-long-service-account-name-for-the-nightly-data-pipeline@example.com", "Product", "5", "", "", False),
]
for name, team, budget, rpm, tpm, monthly in USERS:
    data = dict(name=name, team_name=team, budget_usd=budget, rpm_limit=rpm, tpm_limit=tpm)
    if monthly:
        data["monthly"] = "on"
    post("/admin/users", data)

keys = {}


def mkkey(user, label, allowed="", rpm="", providers=()):
    data = [("user_name", user), ("label", label), ("allowed_models", allowed), ("rpm_limit", rpm), ("tpm_limit", "")]
    data += [("allowed_providers", p) for p in providers]
    response = post("/admin/keys", data)
    match = re.search(r"(sk-toko-[0-9a-f]{32,})", response.text)
    assert match, response.text[:800]
    keys.setdefault(user, []).append(match.group(1))


mkkey("maria.keller", "notebook", "", "240")
mkkey("maria.keller", "eval-harness", "smart, claude-opus-5-5", "", ["anthropic", "openai"])
mkkey("jonas.berg", "laptop", "", "", ["openai", "anthropic"])
mkkey("priya.nair", "support-bot", "smart, fast, text-embedding-3-small", "400")
mkkey("lena.fischer", "prototype", "fast, sonnet, claude-sonnet-5-5", "", ["anthropic"])
mkkey("ci-pipeline", "nightly-evals", "fast, gpt-6-luna", "", ["openai"])
for i in range(1, 61):
    user = f"eval-worker-{i:02d}"
    post("/admin/users", {"name": user, "team_name": "Research" if i % 2 else "Product", "budget_usd": "10", "rpm_limit": "60", "tpm_limit": ""})
    mkkey(user, "batch", "fast")
WORKDIR.mkdir(parents=True, exist_ok=True)
KEYS.write_text(json.dumps(keys))
KEYS.chmod(0o600)

# Exercise real MCP forms and requests. All endpoints are local demo fixtures.
post("/admin/mcp", {"name": "research", "url": MOCK.rstrip("/") + "/mcp", "auth": "bearer", "secret": "demo-mcp-credential", "allow_private": "on", "enabled": "on"})
html = s.get(B + "/admin/mcp", timeout=30).text
mcp_id = re.search(r'action="/admin/mcp/(\d+)/test"', html).group(1)
post(f"/admin/mcp/{mcp_id}/test", {})
html = s.get(B + "/admin/keys?user=ci-pipeline", timeout=30).text
key_id = re.search(r'action="/admin/keys/(\d+)/mcp"', html).group(1)
post(f"/admin/keys/{key_id}/mcp", {"mcp_allow": "research/*"})
mcp = requests.Session()
mcp.headers.update({"Authorization": "Bearer " + keys["ci-pipeline"][0], "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-06-18"})
r = mcp.post(B + "/mcp", json={"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "demo", "version": "1"}}}, timeout=30)
r.raise_for_status()
mcp.headers["Mcp-Session-Id"] = r.headers["Mcp-Session-Id"]
mcp.post(B + "/mcp", json={"jsonrpc": "2.0", "method": "notifications/initialized"}, timeout=30).raise_for_status()
# The gateway dispatches only names from a tools/list of this session.
r = mcp.post(B + "/mcp", json={"jsonrpc": "2.0", "id": 100, "method": "tools/list", "params": {}}, timeout=30)
r.raise_for_status()
assert any(t["name"] == "research__search_docs" for t in r.json()["result"]["tools"]), r.text
for i in range(16):
    r = mcp.post(B + "/mcp", json={"jsonrpc": "2.0", "id": i + 2, "method": "tools/call", "params": {"name": "research__search_docs", "arguments": {"query": "demo"}}}, timeout=30)
    r.raise_for_status()
    assert "result" in r.json(), r.text
print("keys", sum(map(len, keys.values())))
