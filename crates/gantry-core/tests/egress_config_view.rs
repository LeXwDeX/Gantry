//! `ConfigView.injection` 非空视图的契约回归。
//!
//! 权威来源：`docs/interfaces/config-view-injection.md`。本文件只验证 serde 与
//! schema 层，不验证控制面接线或代理行为。

use gantry_core::egress::{ConfigView, InjectionKind};
use serde_json::{json, Map, Value};

const SECRET: &str = "registry.npm.corp";

fn base_view(injection: Value) -> Value {
    json!({
        "rev": 18240,
        "providers": {},
        "groups": {
            "npm": { "hosts": ["npm.corp.example"] },
            "cargo": { "hosts": ["cargo.corp.example"] },
            "wild": { "hosts": ["*.corp.example"] }
        },
        "profiles": {
            "default": { "groups": ["npm", "cargo", "wild"] }
        },
        "default_profile": "default",
        "injection": injection,
        "token_weights": {
            "input": 1.0,
            "output": 5.0,
            "cache_read": 0.1,
            "cache_write": 1.25
        }
    })
}

fn parse(injection: Value) -> Result<ConfigView, serde_json::Error> {
    serde_json::from_value(base_view(injection))
}

fn assert_rejected(injection: Value, pointer: &str) {
    let error = parse(injection).expect_err("injection must be rejected");
    let message = error.to_string();
    assert!(
        message.contains(pointer),
        "expected pointer {pointer} in error: {message}"
    );
}

fn bearer(host: &str, secret: &str) -> Value {
    json!({
        "hosts": [host],
        "kind": "bearer",
        "credential": { "secret_ref": secret }
    })
}

fn header(host: &str, name: &str, secret: &str) -> Value {
    json!({
        "hosts": [host],
        "kind": "header",
        "header": name,
        "credential": { "secret_ref": secret }
    })
}

fn one(id: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(id.to_owned(), value);
    Value::Object(map)
}

#[test]
fn accepts_bearer_and_header_examples() {
    let parsed = parse(json!({
        "npm_private": bearer("npm.corp.example", "registry.npm.corp"),
        "cargo_private": header("cargo.corp.example", "x-api-key", "registry.cargo.corp")
    }))
    .expect("valid bearer and header entries");

    assert_eq!(parsed.injection.len(), 2);
    let npm = &parsed.injection["npm_private"];
    assert_eq!(npm.hosts, ["npm.corp.example"]);
    assert_eq!(npm.kind, InjectionKind::Bearer);
    assert_eq!(npm.header.as_deref(), None);
    assert_eq!(npm.credential.secret_ref.as_str(), "registry.npm.corp");

    let cargo = &parsed.injection["cargo_private"];
    assert_eq!(cargo.kind, InjectionKind::Header);
    assert_eq!(cargo.header.as_deref(), Some("x-api-key"));
    assert_eq!(cargo.credential.secret_ref.as_str(), "registry.cargo.corp");

    let serialized = serde_json::to_value(&parsed).expect("valid view serializes");
    assert_eq!(serialized["injection"]["npm_private"]["kind"], "bearer");
    assert!(serialized["injection"]["npm_private"]
        .get("header")
        .is_none());
    assert_eq!(
        serialized["injection"]["cargo_private"]["header"],
        "x-api-key"
    );
    assert_eq!(
        serialized["injection"]["cargo_private"]["credential"],
        json!({ "secret_ref": "registry.cargo.corp" })
    );
}

#[test]
fn accepts_two_bearer_entries_on_different_hosts() {
    let parsed = parse(json!({
        "npm": bearer("npm.corp.example", "s1"),
        "cargo": bearer("cargo.corp.example", "s2")
    }))
    .expect("two bearers on different hosts");
    assert_eq!(parsed.injection.len(), 2);
}

#[test]
fn accepts_hosts_covered_by_a_wildcard_group() {
    let parsed = parse(json!({
        "npm": bearer("api.corp.example", "s1"),
        "cargo": bearer("*.corp.example", "s2")
    }))
    .expect("wildcard group covers one label and the same wildcard");
    assert_eq!(parsed.injection["cargo"].hosts[0], "*.corp.example");
}

#[test]
fn deduplicates_hosts_case_insensitively_and_preserves_order() {
    let parsed = parse(json!({
        "npm": {
            "hosts": ["npm.corp.example", "NPM.CORP.EXAMPLE", "npm.corp.example"],
            "kind": "bearer",
            "credential": { "secret_ref": SECRET }
        }
    }))
    .expect("duplicate hosts are removed");

    assert_eq!(parsed.injection["npm"].hosts, ["npm.corp.example"]);
    let serialized = serde_json::to_value(&parsed).expect("valid view serializes");
    assert_eq!(
        serialized["injection"]["npm"]["hosts"],
        json!(["npm.corp.example"])
    );
}

#[test]
fn rejects_unknown_injection_and_credential_fields() {
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "credential": { "secret_ref": SECRET },
                "unexpected": true
            }
        }),
        "/injection/npm/unexpected",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "credential": { "secret_ref": SECRET, "value": "leak" }
            }
        }),
        "/injection/npm/credential/value",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "credential": { "secret_ref": SECRET, "present": true }
            }
        }),
        "/injection/npm/credential/present",
    );
}

#[test]
fn rejects_invalid_injection_ids() {
    assert_rejected(
        one("Upper", bearer("npm.corp.example", SECRET)),
        "/injection/Upper",
    );
    assert_rejected(
        one("a/b", bearer("npm.corp.example", SECRET)),
        "/injection/a~1b",
    );
    assert_rejected(one("", bearer("npm.corp.example", SECRET)), "/injection/");
}

#[test]
fn rejects_invalid_hosts() {
    assert_rejected(
        json!({
            "npm": {
                "kind": "bearer",
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/hosts",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": [],
                "kind": "bearer",
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/hosts",
    );

    for host in [
        "api.github.com",
        "raw.githubusercontent.com",
        "127.0.0.1",
        "10.0.0.1",
        "*",
        "**.example.com",
        "https://npm.corp.example",
        "npm.corp.example:443",
        "npm.corp.example/path",
        "a..b",
    ] {
        assert_rejected(
            json!({ "npm": bearer(host, SECRET) }),
            "/injection/npm/hosts/0",
        );
    }
}

#[test]
fn rejects_hosts_not_covered_by_a_group() {
    assert_rejected(
        json!({ "npm": bearer("other.example", SECRET) }),
        "/injection/npm/hosts/0",
    );
    assert_rejected(
        json!({ "npm": bearer("*.elsewhere.example", SECRET) }),
        "/injection/npm/hosts/0",
    );
}

#[test]
fn rejects_invalid_kind() {
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/kind",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "basic",
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/kind",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": 1,
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/kind",
    );
}

#[test]
fn rejects_header_mismatch_and_forbidden_headers() {
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "header": "x-api-key",
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/header",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "header": null,
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/header",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "header",
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/header",
    );

    for name in [
        "",
        "Host",
        "Content-Length",
        "Transfer-Encoding",
        "Connection",
        "Keep-Alive",
        "Proxy-Authenticate",
        "Proxy-Authorization",
        "TE",
        "Trailer",
        "Upgrade",
        "Authorization",
        "bad header",
    ] {
        assert_rejected(
            json!({ "npm": header("npm.corp.example", name, SECRET) }),
            "/injection/npm/header",
        );
    }

    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "header",
                "header": 42,
                "credential": { "secret_ref": SECRET }
            }
        }),
        "/injection/npm/header",
    );
}

#[test]
fn rejects_invalid_credentials() {
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer"
            }
        }),
        "/injection/npm/credential",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "credential": SECRET
            }
        }),
        "/injection/npm/credential",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "credential": {}
            }
        }),
        "/injection/npm/credential/secret_ref",
    );
    assert_rejected(
        json!({
            "npm": {
                "hosts": ["npm.corp.example"],
                "kind": "bearer",
                "credential": { "secret_ref": 42 }
            }
        }),
        "/injection/npm/credential/secret_ref",
    );
    for secret in ["Upper", "-bad", "a/b", ""] {
        assert_rejected(
            json!({
                "npm": {
                    "hosts": ["npm.corp.example"],
                    "kind": "bearer",
                    "credential": { "secret_ref": secret }
                }
            }),
            "/injection/npm/credential/secret_ref",
        );
    }
}

#[test]
fn rejects_duplicate_host_header_pairs() {
    let error = parse(json!({
        "a": bearer("npm.corp.example", "s1"),
        "b": bearer("NPM.CORP.EXAMPLE", "s2")
    }))
    .expect_err("duplicate bearer on the same host");
    assert!(error.to_string().contains("/injection/"), "{}", error);

    let error = parse(json!({
        "a": header("npm.corp.example", "x-api-key", "s1"),
        "b": header("NPM.CORP.EXAMPLE", "X-Api-Key", "s2")
    }))
    .expect_err("duplicate header name on the same host");
    assert!(error.to_string().contains("/injection/"), "{}", error);
}

#[test]
fn accepts_same_header_name_on_different_hosts() {
    let parsed = parse(json!({
        "npm": header("npm.corp.example", "x-api-key", "s1"),
        "cargo": header("cargo.corp.example", "X-Api-Key", "s2")
    }))
    .expect("same header name on different hosts");
    assert_eq!(parsed.injection.len(), 2);
}

#[test]
fn rejects_non_object_injection() {
    assert!(
        serde_json::from_value::<ConfigView>(base_view(json!([]))).is_err(),
        "injection must be an object"
    );
    assert_rejected(one("npm", json!([])), "/injection/npm");
    assert_rejected(one("npm", json!("not an object")), "/injection/npm");
}

#[test]
fn debug_redacts_injection_values() {
    let parsed = parse(json!({
        "npm": bearer("npm.corp.example", "registry.npm.corp")
    }))
    .expect("valid view");
    let debug = format!("{parsed:?}");
    assert!(debug.contains("redacted"));
    assert!(!debug.contains("registry.npm.corp"));
}

#[test]
fn schema_matches_contract() {
    let schema: Value =
        serde_json::to_value(schemars::schema_for!(ConfigView)).expect("schema serializes");

    assert_eq!(
        schema["properties"]["injection"]["propertyNames"]["pattern"],
        "^[a-z0-9][a-z0-9._-]{0,63}$"
    );

    let injection = &schema["$defs"]["InjectionView"];
    assert_eq!(injection["additionalProperties"], false);
    assert_eq!(
        injection["required"],
        json!(["hosts", "kind", "credential"])
    );
    assert_eq!(injection["properties"]["hosts"]["minItems"], 1);
    assert_eq!(
        injection["properties"]["kind"]["enum"],
        json!(["bearer", "header"])
    );

    let credential = &schema["$defs"]["InjectionCredentialView"];
    assert_eq!(credential["additionalProperties"], false);
    assert!(credential["properties"].get("value").is_none());
    assert!(credential["properties"].get("present").is_none());
    assert_eq!(
        schema["$defs"]["SecretName"]["pattern"],
        "^[a-z0-9][a-z0-9._-]{0,63}$"
    );
}
