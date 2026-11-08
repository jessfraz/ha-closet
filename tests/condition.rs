use closet::models::parse_catalog;

#[test]
fn removed_purchase_condition_is_rejected_even_when_null() {
    let source = include_str!("fixtures/catalog-crop.json");
    for condition in ["\"Lightly Worn\"", "null"] {
        let legacy = source.replacen(
            "\"images\":",
            &format!("\"condition_at_purchase\":{condition},\"images\":"),
            1,
        );
        let error = parse_catalog(legacy.as_bytes())
            .expect_err("removed purchase condition must not be accepted");
        assert_eq!(error.code, "invalid_catalog_json");
    }
}
