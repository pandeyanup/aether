use super::*;

#[test]
fn parses_and_orders_prerelease_and_stable_versions() {
    let v010_alpha = Version::parse("0.1.0-alpha").unwrap();
    let tag_010_alpha = Version::parse("alpha-0.1.0").unwrap();
    assert_eq!(v010_alpha, tag_010_alpha);
    assert_eq!(v010_alpha.display(), "0.1.0-alpha");

    let tag_011_alpha = Version::parse("alpha-0.1.1").unwrap();
    assert_eq!(tag_011_alpha.display(), "0.1.1-alpha");
    assert!(tag_011_alpha > v010_alpha);

    let alpha_2 = Version::parse("0.1.1-alpha.2").unwrap();
    let beta_0 = Version::parse("beta-0.1.1").unwrap();
    let rc_1 = Version::parse("v0.1.1-rc.1").unwrap();
    let stable = Version::parse("v0.1.1").unwrap();
    let next = Version::parse("0.2.0+build123").unwrap();

    assert!(tag_011_alpha < alpha_2);
    assert!(alpha_2 < beta_0);
    assert!(beta_0 < rc_1);
    assert!(rc_1 < stable);
    assert!(stable < next);

    assert!(Version::parse("").is_none());
    assert!(Version::parse("nightly").is_none());
    assert!(Version::parse("1.2").is_none());
}

#[test]
fn parses_sha256_digests_and_sidecar_text() {
    let hex = "a".repeat(64);
    assert_eq!(parse_sha256_text(&format!("sha256:{hex}")).as_deref(), Some(hex.as_str()));
    assert_eq!(
        parse_sha256_text(&format!(
            "SHA256:{}  Aether-0.1.1-macos-aarch64.dmg\n",
            hex.to_uppercase()
        ))
        .as_deref(),
        Some(hex.as_str())
    );
    assert_eq!(
        parse_sha256_text(&format!("{hex}  Aether-0.1.1-linux-x86_64.tar.gz\n")).as_deref(),
        Some(hex.as_str())
    );
    assert_eq!(parse_sha256_text("not-a-hash"), None);
    assert_eq!(parse_sha256_text("sha256:abcd"), None);
}

fn asset(name: &str, size: u64, digest: Option<&str>) -> GithubAsset {
    GithubAsset {
        name: name.to_string(),
        browser_download_url: format!(
            "https://github.com/PrashnaLabs/aether/releases/download/alpha-0.1.1/{name}"
        ),
        size,
        digest: digest.map(str::to_string),
    }
}

#[test]
fn selects_platform_asset_and_checksum_sidecar() {
    let mac_hash = "0".repeat(64);
    let linux_hash = "1".repeat(64);
    let win_hash = "2".repeat(64);
    let assets = vec![
        asset(
            "Aether-0.1.1-alpha-macos-aarch64.dmg",
            20_000_000,
            Some(&format!("sha256:{mac_hash}")),
        ),
        asset("Aether-0.1.1-alpha-macos-aarch64.dmg.sha256", 90, None),
        asset("Aether-0.1.1-alpha-linux-x86_64.deb", 18_000_000, None),
        asset("Aether-0.1.1-alpha-linux-x86_64.tar.gz", 19_000_000, None),
        asset(
            "Aether-0.1.1-alpha-linux-x86_64.tar.gz.sha256",
            90,
            Some(&format!("sha256:{linux_hash}")),
        ),
        asset(
            "Aether-0.1.1-alpha-windows-x86_64.zip",
            21_000_000,
            Some(&format!("sha256:{win_hash}")),
        ),
    ];

    let mac = select_asset(&assets, "macos", "aarch64").unwrap();
    assert_eq!(mac.name, "Aether-0.1.1-alpha-macos-aarch64.dmg");
    assert_eq!(mac.sha256.as_deref(), Some(mac_hash.as_str()));
    assert!(mac.sha256_url.as_deref().is_some_and(|u| u.ends_with(".dmg.sha256")));

    let linux = select_asset(&assets, "linux", "x86_64").unwrap();
    assert_eq!(linux.name, "Aether-0.1.1-alpha-linux-x86_64.tar.gz");
    assert_eq!(linux.sha256, None);
    assert!(linux.sha256_url.as_deref().is_some_and(|u| u.ends_with(".tar.gz.sha256")));

    let win = select_asset(&assets, "windows", "x86_64").unwrap();
    assert_eq!(win.name, "Aether-0.1.1-alpha-windows-x86_64.zip");
    assert_eq!(win.sha256.as_deref(), Some(win_hash.as_str()));
}

#[test]
fn check_outcome_handles_prereleases_and_ignores_drafts() {
    let releases = vec![
        GithubRelease {
            tag_name: "alpha-0.1.0".into(),
            name: "alpha-0.1.0".into(),
            html_url: "https://github.com/PrashnaLabs/aether/releases/tag/alpha-0.1.0".into(),
            body: None,
            draft: false,
            assets: vec![asset("Aether-0.1.0-alpha-macos-aarch64.dmg", 10, None)],
        },
        GithubRelease {
            tag_name: "alpha-0.1.1".into(),
            name: "alpha-0.1.1".into(),
            html_url: "https://github.com/PrashnaLabs/aether/releases/tag/alpha-0.1.1".into(),
            body: Some("Bug fixes".into()),
            draft: false,
            assets: vec![asset("Aether-0.1.1-alpha-macos-aarch64.dmg", 20, None)],
        },
        GithubRelease {
            tag_name: "0.9.0".into(),
            name: "0.9.0".into(),
            html_url: "https://github.com/PrashnaLabs/aether/releases/tag/0.9.0".into(),
            body: None,
            draft: true,
            assets: vec![],
        },
    ];

    let available = check_outcome("0.1.0-alpha", &releases, "macos", "aarch64").unwrap().unwrap();
    assert_eq!(available.tag, "alpha-0.1.1");
    assert_eq!(available.display_version, "0.1.1-alpha");
    assert_eq!(available.notes, "Bug fixes");
    assert_eq!(available.asset.unwrap().name, "Aether-0.1.1-alpha-macos-aarch64.dmg");

    assert_eq!(check_outcome("0.1.1-alpha", &releases, "macos", "aarch64").unwrap(), None);
    assert_eq!(check_outcome("0.2.0", &releases, "macos", "aarch64").unwrap(), None);
}
