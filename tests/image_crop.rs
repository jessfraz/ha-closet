use closet::models::{ImageCrop, parse_catalog, validate_catalog};

#[test]
fn source_with_a_non_destructive_crop_is_valid_catalog_input() {
    let catalog = parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .expect("a bounded image viewport is valid catalog data");
    assert_eq!(catalog.report().count, 1);
    let image = &catalog.items()[0].images[0];
    assert_eq!(image.url, "https://images.example.invalid/jacket.svg");
    assert_eq!(
        image.source_url.as_deref(),
        Some("https://shop.example.invalid/jacket")
    );
    let encoded = serde_json::to_vec(catalog.items()).unwrap();
    assert_eq!(parse_catalog(&encoded).unwrap().report(), catalog.report());
}

#[test]
fn crops_round_trip_and_absent_crops_keep_the_current_canonical_shape() {
    let catalog = parse_catalog(include_bytes!("fixtures/catalog-crop.json")).unwrap();
    let mut items = catalog.into_items();
    let crop = ImageCrop {
        x: 2500,
        y: 1250,
        width: 5000,
        height: 7500,
    };
    assert_eq!(items[0].images[0].crop, Some(crop));
    let encoded = serde_json::to_vec(&items).unwrap();
    assert_eq!(parse_catalog(&encoded).unwrap().items(), items);
    items[0].images[0].crop = None;
    // The explicit count belongs to the current import fixture. Omitting a
    // crop must not add a serialized crop field or change anything else.
    assert_eq!(
        validate_catalog(items.clone())
            .unwrap()
            .report()
            .source_hash,
        "0b9d06c69d66b7fdb9bc73e0b56a913039830f471aa6d5b9bcec9ca8701e4619"
    );
    let legacy = serde_json::to_vec(&items).unwrap();
    let decoded = parse_catalog(&legacy).unwrap();
    assert_eq!(decoded.items()[0].images[0].crop, None);
    assert_eq!(decoded.items(), items);
}

#[test]
fn crop_rectangles_require_positive_size_and_fit_both_original_axes() {
    let item = parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .unwrap()
        .into_items()
        .remove(0);
    for crop in [
        ImageCrop {
            x: 0,
            y: 0,
            width: 10_000,
            height: 10_000,
        },
        ImageCrop {
            x: 9999,
            y: 9999,
            width: 1,
            height: 1,
        },
    ] {
        let mut candidate = item.clone();
        candidate.images[0].crop = Some(crop);
        assert!(validate_catalog(vec![candidate]).is_ok());
    }
    for crop in [
        ImageCrop {
            x: 0,
            y: 0,
            width: 0,
            height: 10_000,
        },
        ImageCrop {
            x: 0,
            y: 0,
            width: 10_000,
            height: 0,
        },
        ImageCrop {
            x: 1,
            y: 0,
            width: 10_000,
            height: 1,
        },
        ImageCrop {
            x: 0,
            y: 1,
            width: 1,
            height: 10_000,
        },
        ImageCrop {
            x: 10_000,
            y: 0,
            width: 1,
            height: 1,
        },
        ImageCrop {
            x: 0,
            y: 10_000,
            width: 1,
            height: 1,
        },
        ImageCrop {
            x: u16::MAX,
            y: 0,
            width: u16::MAX,
            height: 1,
        },
    ] {
        let mut candidate = item.clone();
        candidate.images[0].crop = Some(crop);
        let error = validate_catalog(vec![candidate]).unwrap_err();
        assert_eq!(error.code, "invalid_catalog");
        assert_eq!(error.errors.len(), 1);
        assert_eq!(error.errors[0].field, "images.crop");
        assert_eq!(error.errors[0].code, "invalid_crop");
    }
    for invalid in [
        r#"{"x":-1,"y":0,"width":1,"height":1}"#,
        r#"{"x":0.5,"y":0,"width":1,"height":1}"#,
        r#"{"x":0,"y":0,"width":65536,"height":1}"#,
        r#"{"x":0,"y":0,"width":1}"#,
        r#"{"x":0,"y":0,"width":1,"height":1,"rotate":90}"#,
    ] {
        assert!(serde_json::from_str::<ImageCrop>(invalid).is_err());
    }
}
