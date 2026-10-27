//! 规则层测试：`docs/modules/egress.md` §12.D（硬性拒绝、IP 字面量、网段、通配）与
//! §12.F（非法视图）中不依赖网络的部分；令牌只存哈希（§12.A）。

use std::net::IpAddr;

use gantry_core::egress::ConfigView;
use gantry_core::ids::Entropy;
use gantry_egress::{
    mint_job_token, mint_svc_token, parse_connect_authority, review, AllowPattern, Cidr, DenyNets,
    Host, HostDenial, OsEntropy, TokenHash,
};
use serde_json::{json, Value};

fn connect(authority: &str) -> Result<Host, HostDenial> {
    parse_connect_authority(authority)
}

#[test]
fn github_family_is_hard_denied_in_every_spelling() {
    for authority in [
        "github.com:443",
        "GitHub.COM:443",
        "github.com.:443",
        "api.github.com:443",
        "x.y.github.com:443",
        "raw.githubusercontent.com:443",
        "objects.githubusercontent.com:443",
        "ghcr.io:443",
        "foo.github.io:443",
        "codeload.github.com:443",
        "git.io:443",
        "a.githubcopilot.com:443",
        // IDN 混淆：全角字母、全角句点、大写全角。
        "ｇｉｔｈｕｂ.com:443",
        "github。com:443",
        "ＧＩＴＨＵＢ.ＣＯＭ:443",
        "api.ｇｉｔｈｕｂ.com:443",
        // userinfo。
        "a@github.com:443",
        "job:secret@api.github.com:443",
        // 端口不对也先报 GitHub。
        "github.com:22",
    ] {
        assert_eq!(
            connect(authority),
            Err(HostDenial::Github),
            "{authority} must be hard.github"
        );
        assert_eq!(HostDenial::Github.rule_id(), "hard.github");
    }
}

#[test]
fn lookalikes_that_are_not_github_are_not_hard_github() {
    for authority in [
        "evilgithub.com:443",
        "github.com.evil.example:443",
        "notgit.io:443",
    ] {
        assert!(
            connect(authority).is_ok(),
            "{authority} is not part of the GitHub family"
        );
    }
}

#[test]
fn ip_literals_are_denied_in_every_notation() {
    for authority in [
        "127.0.0.1:443",
        "8.8.8.8:443",
        "2130706433:443",
        "017700000001:443",
        "0x7f000001:443",
        "0x7f.0.0.1:443",
        "127.1:443",
        "0177.0.0.1:443",
        "[::1]:443",
        "[::ffff:127.0.0.1]:443",
        "[2001:db8::1]:443",
        "::1:443",
    ] {
        assert_eq!(
            connect(authority),
            Err(HostDenial::IpLiteral),
            "{authority} must be hard.ip_literal"
        );
    }
}

#[test]
fn only_port_443_is_allowed() {
    for authority in [
        "registry.npmjs.org:22",
        "registry.npmjs.org:80",
        "registry.npmjs.org:8443",
    ] {
        assert_eq!(connect(authority), Err(HostDenial::Port), "{authority}");
    }
    assert_eq!(
        connect("registry.npmjs.org:443").map(|h| h.to_string()),
        Ok("registry.npmjs.org".into())
    );
}

#[test]
fn malformed_authorities_are_denied() {
    for authority in [
        "registry.npmjs.org",
        "registry.npmjs.org:",
        "registry.npmjs.org:+443",
        "registry.npmjs.org:99999",
        "user@registry.npmjs.org:443",
        "reg_istry.npmjs.org:443",
        "-bad.example:443",
        "*.example.com:443",
        ":443",
    ] {
        assert_eq!(
            connect(authority),
            Err(HostDenial::Authority),
            "{authority} must be hard.authority"
        );
    }
}

#[test]
fn host_normalization_is_lowercase_punycode_without_trailing_dot() {
    assert_eq!(
        Host::parse("Registry.NPMJS.org.").unwrap().as_str(),
        "registry.npmjs.org"
    );
    assert_eq!(
        Host::parse("bücher.example").unwrap().as_str(),
        "xn--bcher-kva.example"
    );
}

#[test]
fn wildcard_matches_exactly_one_label() {
    let pattern = AllowPattern::parse("*.example.com").unwrap();
    let matches = |host: &str| pattern.matches(&Host::parse(host).unwrap());
    assert!(matches("a.example.com"));
    assert!(matches("A.Example.COM."));
    assert!(!matches("example.com"));
    assert!(!matches("evilexample.com"));
    assert!(!matches("example.com.evil.com"));
    // OQ-E8 未决：取更严的解释，不匹配多层子域。
    assert!(!matches("a.b.example.com"));

    let exact = AllowPattern::parse("registry.npmjs.org").unwrap();
    assert!(exact.matches(&Host::parse("registry.npmjs.org").unwrap()));
    assert!(!exact.matches(&Host::parse("x.registry.npmjs.org").unwrap()));
}

#[test]
fn allow_patterns_reject_forbidden_entries() {
    assert_eq!(AllowPattern::parse("*"), Err(HostDenial::Authority));
    assert_eq!(
        AllowPattern::parse("*.*.example.com"),
        Err(HostDenial::Authority)
    );
    assert_eq!(
        AllowPattern::parse("a.*.example.com"),
        Err(HostDenial::Authority)
    );
    assert_eq!(
        AllowPattern::parse("api.github.com"),
        Err(HostDenial::Github)
    );
    assert_eq!(AllowPattern::parse("*.github.com"), Err(HostDenial::Github));
    assert_eq!(AllowPattern::parse("10.0.0.1"), Err(HostDenial::IpLiteral));
}

#[test]
fn private_and_special_addresses_are_denied() {
    let nets = DenyNets::default();
    for ip in [
        "0.0.0.0",
        "10.1.2.3",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.8",
        "192.168.1.1",
        "198.18.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "fc00::1",
        "fd12::1",
        "fe80::1",
        "ff02::1",
        // IPv4 映射与 NAT64 按内嵌 IPv4 再判。
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "64:ff9b::a00:1",
        "64:ff9b::7f00:1",
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(nets.is_denied(ip), "{ip} must be denied");
    }
    for ip in [
        "8.8.8.8",
        "160.79.104.10",
        "172.32.0.1",
        "2606:4700::1",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(!nets.is_denied(ip), "{ip} must be allowed");
    }
}

#[test]
fn extra_nets_cover_egress_listeners_and_sandbox_ranges() {
    let nets = DenyNets::new(vec![
        "203.0.113.0/24".parse::<Cidr>().unwrap(),
        "2001:db8::/32".parse::<Cidr>().unwrap(),
    ]);
    assert!(nets.is_denied("203.0.113.9".parse().unwrap()));
    assert!(nets.is_denied("2001:db8::5".parse().unwrap()));
    assert!(nets.is_denied("::ffff:203.0.113.9".parse().unwrap()));
    assert!(!nets.is_denied("203.0.114.1".parse().unwrap()));
    assert!("10.0.0.0/33".parse::<Cidr>().is_err());
    assert!("nonsense".parse::<Cidr>().is_err());
}

fn base_view() -> Value {
    json!({
        "rev": 7,
        "providers": {
            "anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "secret_ref": "model.anthropic.main"
            }
        },
        "groups": {
            "model_api": { "hosts": ["api.anthropic.com"] },
            "npm": { "hosts": ["registry.npmjs.org"] },
            "crates": { "hosts": ["crates.io", "*.crates.io"] }
        },
        "profiles": { "default": { "groups": ["model_api", "npm", "crates"] } },
        "default_profile": "default",
        "injection": {},
        "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 }
    })
}

fn view(value: Value) -> ConfigView {
    serde_json::from_value(value).expect("shape-valid config view")
}

fn rejected_pointers(value: Value) -> Vec<String> {
    review(view(value))
        .expect_err("view must be rejected")
        .into_iter()
        .map(|issue| issue.pointer)
        .collect()
}

#[test]
fn valid_view_passes_review_and_resolves_upstreams() {
    let reviewed = review(view(base_view())).expect("valid");
    let upstream = reviewed.provider("anthropic").unwrap();
    assert_eq!(upstream.host.as_str(), "api.anthropic.com");
    assert_eq!(upstream.base_path, "");
    assert_eq!(reviewed.group("crates").unwrap().len(), 2);
}

#[test]
fn groups_with_forbidden_hosts_are_rejected() {
    for (host, _why) in [
        ("api.github.com", "V10 GitHub"),
        ("10.0.0.1", "IP literal"),
        ("*", "bare wildcard"),
        ("*.*.example.com", "multi-level wildcard"),
    ] {
        let mut value = base_view();
        value["groups"]["npm"]["hosts"] = json!(["registry.npmjs.org", host]);
        assert_eq!(
            rejected_pointers(value),
            vec!["/groups/npm/hosts/1"],
            "{host}"
        );
    }
}

#[test]
fn provider_host_outside_model_api_is_rejected() {
    let mut value = base_view();
    value["providers"]["anthropic"]["base_url"] = json!("https://registry.npmjs.org");
    assert_eq!(
        rejected_pointers(value),
        vec!["/providers/anthropic/base_url"]
    );

    for url in [
        "http://api.anthropic.com",
        "https://api.anthropic.com:8443",
        "https://user@api.anthropic.com",
        "https://api.anthropic.com/v1?x=1",
        "https://api.anthropic.com/../x",
    ] {
        let mut value = base_view();
        value["providers"]["anthropic"]["base_url"] = json!(url);
        assert_eq!(
            rejected_pointers(value),
            vec!["/providers/anthropic/base_url"],
            "{url}"
        );
    }

    let mut value = base_view();
    value["providers"]["anthropic"]["base_url"] = json!("https://api.anthropic.com:443/proxy/");
    let reviewed = review(view(value)).expect("explicit 443 and a base path are fine");
    assert_eq!(reviewed.provider("anthropic").unwrap().base_path, "/proxy");
}

#[test]
fn unknown_provider_kind_profile_references_and_weights_are_rejected() {
    let mut value = base_view();
    value["providers"]["anthropic"]["kind"] = json!("openai");
    assert_eq!(rejected_pointers(value), vec!["/providers/anthropic/kind"]);

    let mut value = base_view();
    value["profiles"]["default"]["groups"] = json!(["model_api", "pypi"]);
    assert_eq!(rejected_pointers(value), vec!["/profiles/default/groups/1"]);

    let mut value = base_view();
    value["default_profile"] = json!("strict");
    assert_eq!(rejected_pointers(value), vec!["/default_profile"]);

    let mut value = base_view();
    value["token_weights"]["output"] = json!(0.0);
    assert_eq!(rejected_pointers(value), vec!["/token_weights"]);

    let mut value = base_view();
    value["providers"]["a/b"] = value["providers"]["anthropic"].clone();
    assert_eq!(rejected_pointers(value), vec!["/providers/a~1b"]);
}

#[test]
fn non_empty_injection_is_rejected_in_m1() {
    let mut value = base_view();
    value["groups"]["npm"]["hosts"] = json!(["registry.npmjs.org", "npm.corp.example"]);
    value["injection"] = json!({
        "npm_private": {
            "hosts": ["npm.corp.example"],
            "kind": "bearer",
            "credential": { "secret_ref": "registry.npm.corp" }
        }
    });
    assert_eq!(rejected_pointers(value), vec!["/injection"]);
}

#[test]
fn every_issue_is_reported_together() {
    let mut value = base_view();
    value["groups"]["npm"]["hosts"] = json!(["github.com"]);
    value["default_profile"] = json!("missing");
    assert_eq!(
        rejected_pointers(value),
        vec!["/groups/npm/hosts/0", "/default_profile"]
    );
}

#[test]
fn minted_tokens_are_well_formed_unique_and_only_hashed() {
    let entropy = OsEntropy;
    let (job, job_hash) = mint_job_token(&entropy);
    let (other, other_hash) = mint_job_token(&entropy);
    assert!(job.expose_secret().starts_with("gjt1_"));
    assert_eq!(job.expose_secret().len(), 5 + 43);
    assert_ne!(job, other);
    assert_ne!(job_hash, other_hash);
    assert_eq!(TokenHash::of(job.expose_secret()), job_hash);
    // 哈希里不含令牌原文。
    assert!(!job_hash
        .digest()
        .to_hex()
        .contains(&job.expose_secret()[5..15]));
    assert_eq!(format!("{job:?}"), "JobToken(***)");

    let (svc, svc_hash) = mint_svc_token(&entropy);
    assert!(svc.expose_secret().starts_with("gst1_"));
    assert_eq!(TokenHash::of(svc.expose_secret()), svc_hash);
}

#[test]
fn minting_uses_the_injected_entropy() {
    struct Zero;
    impl Entropy for Zero {
        fn fill(&self, buf: &mut [u8]) {
            buf.fill(0);
        }
    }
    let (token, _) = mint_job_token(&Zero);
    assert_eq!(token.expose_secret(), format!("gjt1_{}", "A".repeat(43)));
}
