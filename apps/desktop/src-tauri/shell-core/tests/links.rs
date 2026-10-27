//! 外链校验（desktop-shell.md §12"IPC 白名单与参数"：`open_external` / `open_github`）。

use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::links::{check_external, GithubTarget};

#[test]
fn external_accepts_https_and_flags_non_github_hosts() {
    let gh = check_external("https://github.com/acme/web/pull/12").unwrap();
    assert!(!gh.needs_confirm);
    assert_eq!(gh.host_display, "github.com");

    for (url, host) in [
        ("https://docs.rs/tauri", "docs.rs"),
        ("https://gist.github.com/x", "gist.github.com"),
        (
            "https://github.com.evil.example/x",
            "github.com.evil.example",
        ),
        ("https://github.com:8443/x", "github.com"),
        ("https://93.184.216.34/", "93.184.216.34"),
    ] {
        let link = check_external(url).unwrap();
        assert!(link.needs_confirm, "{url}");
        assert_eq!(link.host_display, host, "{url}");
    }
}

#[test]
fn external_shows_unicode_and_punycode_for_idn() {
    let link = check_external("https://例子.测试/路径").unwrap();
    assert!(link.needs_confirm);
    assert_eq!(link.host_display, "例子.测试（xn--fsqu00a.xn--0zwm56d）");
    assert!(link.url.starts_with("https://xn--fsqu00a.xn--0zwm56d/"));

    // 同形异义：西里尔字母的 "gіthub.com" 不是 github.com，必须确认且显示 punycode。
    let spoof = check_external("https://gіthub.com/acme").unwrap();
    assert!(spoof.needs_confirm);
    assert!(
        spoof.host_display.contains("xn--"),
        "{}",
        spoof.host_display
    );
}

#[test]
fn external_rejects_other_schemes_and_userinfo() {
    for url in [
        "http://github.com/",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "JAVASCRIPT:alert(1)",
        "data:text/html,<b>x</b>",
        "gantry+pair://h:1/?t=x",
        "vscode://file/x",
        "https://user:pass@github.com/",
        "https://user@github.com/",
        "https:///nohost",
        "https://github.com/a b",
        "https://github.com/\n",
        "not a url",
        "",
    ] {
        let err = check_external(url).unwrap_err();
        assert_eq!(
            (err.code, err.field.as_deref()),
            (ShellErrorCode::BadArgument, Some("url")),
            "{url:?}"
        );
    }
    let long = format!("https://github.com/{}", "a".repeat(5000));
    assert!(check_external(&long).is_err());
}

#[test]
fn github_links_are_built_by_the_shell() {
    let url = |kind, repo, number, sha| GithubTarget::from_args(kind, repo, number, sha)?.url();
    assert_eq!(
        url("pr", "acme/web", Some(12), None).unwrap(),
        "https://github.com/acme/web/pull/12"
    );
    assert_eq!(
        url("issue", "acme/web.js", Some(3), None).unwrap(),
        "https://github.com/acme/web.js/issues/3"
    );
    let sha = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
    assert_eq!(
        url("commit", "acme/web", None, Some(sha)).unwrap(),
        format!("https://github.com/acme/web/commit/{sha}")
    );
}

#[test]
fn github_rejects_bad_repo_number_sha_and_kind() {
    let field = |kind, repo, number, sha| {
        GithubTarget::from_args(kind, repo, number, sha)
            .and_then(|t| t.url())
            .unwrap_err()
            .field
    };
    for repo in [
        "acme",
        "acme/web/extra",
        "../web",
        "acme/..",
        "acme/web?x=1",
        "acme/web#frag",
        "acme/we b",
        "evil.com/@x",
        "",
    ] {
        assert_eq!(
            field("pr", repo, Some(1), None).as_deref(),
            Some("repo"),
            "{repo}"
        );
    }
    assert_eq!(
        field("pr", "acme/web", Some(0), None).as_deref(),
        Some("number")
    );
    assert_eq!(
        field("pr", "acme/web", None, None).as_deref(),
        Some("number")
    );
    assert_eq!(
        field("issue", "acme/web", Some(1), Some("abc")).as_deref(),
        Some("number")
    );
    for sha in ["abc", "9F2C4E7A1B3D5F60718293A4B5C6D7E8F9012345", "../../x"] {
        assert_eq!(
            field("commit", "acme/web", None, Some(sha)).as_deref(),
            Some("sha"),
            "{sha}"
        );
    }
    assert_eq!(
        field("commit", "acme/web", Some(1), None).as_deref(),
        Some("sha")
    );
    assert_eq!(
        field("tree", "acme/web", Some(1), None).as_deref(),
        Some("kind")
    );
}
