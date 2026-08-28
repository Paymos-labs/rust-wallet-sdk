use paymos_wallet::VaultSecret;

fn full() -> String {
    VaultSecret::pack("vk_live_abc123", 42, Some("{\"share\":\"x\"}"))
}

fn read_only() -> String {
    VaultSecret::pack("vk_live_abc123", 42, None)
}

#[test]
fn packs_then_parses_full_and_read_only() {
    let full = VaultSecret::parse(&full()).unwrap();
    assert_eq!(full.api_key, "vk_live_abc123");
    assert_eq!(full.vault_id, 42);
    assert!(full.has_share());

    let read = VaultSecret::parse(&read_only()).unwrap();
    assert!(!read.has_share());
    assert_eq!(read.share, None);
}

#[test]
fn rejects_anything_that_is_not_a_vault_secret() {
    for bad in ["sk_prod_not_ours", "vk_test_dev_key", "", "vs_live_@@@"] {
        assert!(VaultSecret::parse(bad).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn rejects_a_secret_whose_api_key_is_not_one_of_ours() {
    assert!(VaultSecret::parse(&VaultSecret::pack("nope", 1, None)).is_err());
}

#[test]
fn never_renders_the_share_and_masks_the_key() {
    let s = VaultSecret::parse(&full()).unwrap();
    for rendered in [format!("{s}"), format!("{s:?}")] {
        assert!(!rendered.contains("share\":\"x"), "the share must never be rendered");
        assert!(!rendered.contains("abc123"), "the key must be masked");
        assert!(rendered.contains("vk_live_"), "the non-secret prefix is fine to show");
    }
}
