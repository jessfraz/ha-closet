//! Fictional TOML producer -> checked unresolved source -> existing v1 projection.
use closet::{
    images::ImageSha,
    models::SyncRequest,
    source::{CompiledCatalog, SourceError, types::*},
    store::Store,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path, process::Command};
#[path = "support/toml.rs"]
mod fixture;
#[derive(Serialize, Deserialize)]
struct Document {
    schema: u8,
    styles: Vec<Style>,
    variants: Vec<Variant>,
    inventory: Vec<InventoryEntry>,
}
fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from));
    fs::write(path, text.replace(from, to)).unwrap();
}
fn document(path: &Path) -> Document {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}
fn produce(path: &Path, value: &Document) {
    fs::write(path, toml::to_string_pretty(value).unwrap()).unwrap();
}
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = fixture::write(directory.path());
    (directory, path)
}
fn urls(source: &CompiledCatalog) -> BTreeMap<ImageSha, url::Url> {
    source
        .assets()
        .iter()
        .map(|asset| {
            (
                asset.sha256.clone(),
                format!(
                    "https://ha.example.invalid/api/image/serve/{}/original",
                    &asset.sha256.as_str()[..32]
                )
                .parse()
                .unwrap(),
            )
        })
        .collect()
}
fn code(error: &SourceError, expected: &str) {
    assert!(
        error.errors.iter().any(|error| error.code == expected),
        "{error:?}"
    );
}
#[tokio::test]
async fn real_producer_preserves_purchases_gallery_facts_and_service_retry_history() {
    let (directory, path) = fixture();
    let mut authored = document(&path);
    authored.inventory[0].sku = Some("FICTIONAL-SKU".to_owned().try_into().unwrap());
    authored.inventory[0].notes = vec!["Fictional purchase note".to_owned().try_into().unwrap()];
    authored.inventory[0].price = SourcePrice {
        amount: 42.5.try_into().unwrap(),
        currency: "USD".to_owned().try_into().unwrap(),
        basis: "receipt".to_owned().try_into().unwrap(),
        quantity_basis: "per_item".to_owned().try_into().unwrap(),
    };
    produce(&path, &authored);
    fs::write(
        directory.path().join("fittings.toml"),
        "[fittings.example_spring]\n\
        label='Example spring fitting'\n\
        season='Spring/Summer'\n\
        years=[2026]\n\
        legacy_api_purchase_source='fictional_private_source_token'\n\
        stylist='FITTING_EVIDENCE_SENTINEL'\n\
        deck_url='https://documents.example.invalid/PRIVATE_EVIDENCE_SENTINEL'\n\
        fitting_date='2026-01-02'\n",
    )
    .unwrap();
    let source = CompiledCatalog::load(&path).unwrap();
    assert_eq!(source.item_count(), 2);
    assert_eq!(source.assets().len(), 1);
    assert_eq!(source.assets()[0].slots.len(), 2);
    assert_eq!(source.assets()[0].slots[0].index, 0);
    assert_eq!(source.assets()[0].slots[1].index, 1);
    assert!(source.materialize(&BTreeMap::new()).is_err()); // No fake URL is invented for a new asset.
    let projected = source.materialize(&urls(&source)).unwrap();
    let items = projected.items();
    assert_eq!(items[0].id, "example-purchase-a");
    assert_eq!(items[0].quantity, Some(2));
    assert_eq!(items[1].quantity, Some(1));
    assert_eq!(items[0].size.as_deref(), Some("00"));
    assert_eq!(items[1].size.as_deref(), Some("0"));
    assert_eq!(items[0].color.as_deref(), Some("Navy"));
    assert_eq!(items[0].sku.as_deref(), Some("FICTIONAL-SKU"));
    assert_eq!(items[0].price.as_ref().unwrap().amount, 42.5);
    assert_eq!(items[0].purchase_source.as_deref(), Some("self_bought"));
    assert_eq!(
        items[1].purchase_source.as_deref(),
        Some("fictional_private_source_token")
    );
    assert_eq!(
        items[1].shop.as_ref().unwrap().label,
        "Example spring fitting"
    );
    assert_eq!(items[1].images.len(), 2);
    assert_eq!(items[1].images[1].crop.as_ref().unwrap().x, 1000);
    assert_eq!(
        items[1].images[1].source_url.as_deref(),
        Some("https://maker.example/crew-tee/detail")
    );
    let wire = serde_json::to_string(items).unwrap();
    assert!(!wire.contains("EVIDENCE_SENTINEL"));
    let store = Store::open(directory.path().join("projection.sqlite3"))
        .await
        .unwrap();
    let request = || SyncRequest {
        items: projected.items().to_vec(),
        dry_run: false,
        allow_empty: false,
    };
    let report = store
        .sync(request(), Some(0), "example-owner", Some("original"))
        .await
        .unwrap();
    assert_eq!(report.created, 2);
    assert_eq!(
        store
            .sync(request(), Some(0), "example-owner", Some("original"))
            .await
            .unwrap(),
        report
    );
    let again = store
        .sync(request(), Some(1), "example-owner", Some("same-source"))
        .await
        .unwrap();
    assert_eq!(again.catalog_revision, 1);
    assert_eq!(again.unchanged, 2);
    store.close().await;
}
#[test]
fn source_identity_is_format_independent_but_every_input_byte_is_frozen() {
    let (directory, path) = fixture();
    let before = CompiledCatalog::load(&path).unwrap();
    let before_projection = before.materialize(&urls(&before)).unwrap();
    let mut authored = document(&path);
    authored.inventory.reverse();
    produce(&path, &authored);
    fs::write(
        directory.path().join("brands.toml"),
        fs::read_to_string(directory.path().join("brands.toml")).unwrap()
            + "\n# Comment-only source edit\n",
    )
    .unwrap();
    assert_eq!(
        before.verify_unchanged().unwrap_err().code,
        "catalog_source_changed"
    );
    let after = CompiledCatalog::load(&path).unwrap();
    assert_eq!(before.source_hash(), after.source_hash());
    assert_eq!(
        before_projection.report().source_hash,
        after
            .materialize(&urls(&after))
            .unwrap()
            .report()
            .source_hash
    );
    let asset = &after.assets()[0];
    fs::write(&asset.path, b"changed").unwrap();
    assert_eq!(
        after.verify_unchanged().unwrap_err().code,
        "catalog_source_changed"
    );
    assert!(after.materialize(&urls(&after)).is_err());
}
#[test]
fn unknown_fields_refs_and_conflicts_are_offline_and_safe() {
    let (directory, path) = fixture();
    let mut authored = document(&path);
    authored.inventory[1].variant = "missing_variant".to_owned().try_into().unwrap();
    authored.styles[0].brand = "missing_brand".to_owned().try_into().unwrap();
    authored.styles[0].category = "missing.category".to_owned().try_into().unwrap();
    authored.inventory.push(authored.inventory[0].clone());
    produce(&path, &authored);
    let error = CompiledCatalog::load(&path).unwrap_err();
    for expected in [
        "unknown_variant",
        "unknown_brand",
        "unknown_category",
        "duplicate_inventory_id",
    ] {
        code(&error, expected);
    }
    let safe = serde_json::to_string(&error).unwrap();
    assert!(
        !safe.contains("missing_variant")
            && !safe.contains("example-purchase")
            && !safe.contains(directory.path().to_str().unwrap())
    );
    let (_directory, path) = fixture();
    edit(&path, "quantity = 2", "quantity = 0");
    assert!(CompiledCatalog::load(&path).is_err());
    let (_directory, path) = fixture();
    edit(
        &path,
        "quantity = 2",
        "quantity = 2\nUNMAPPED_PRIVATE_FIELD = 'DO_NOT_ECHO_SENTINEL'",
    );
    let error = CompiledCatalog::load(&path).unwrap_err();
    assert!(
        !serde_json::to_string(&error)
            .unwrap()
            .contains("DO_NOT_ECHO_SENTINEL")
    );
}
#[test]
fn registry_ids_are_not_aliases_and_unicode_collisions_do_not_last_win() {
    let (directory, path) = fixture();
    let source = CompiledCatalog::load(&path).unwrap();
    assert_eq!(
        source.brand_by_name("NORTHSTUDIO"),
        source.brand_by_name("North Studio")
    );
    assert!(source.brand_by_name("north_studio").is_none());
    let brands = directory.path().join("brands.toml");
    fs::write(
        &brands,
        fs::read_to_string(&brands).unwrap()
            + "\n[brands.second]\nname='ＮＯＲＴＨ\u{fe0f} STUDIO'\n",
    )
    .unwrap();
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "brand_name_collision",
    );
    let (directory, path) = fixture();
    edit(
        &directory.path().join("brands.toml"),
        "aliases = [\"NorthStudio\"]",
        "aliases = [\"NORTH STUDIO\"]",
    );
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "brand_name_collision",
    );
    let (_directory, path) = fixture();
    edit(&path, "brand = \"north_studio\"", "brand = \"northstudio\"");
    code(&CompiledCatalog::load(&path).unwrap_err(), "unknown_brand");
}
#[test]
fn category_labels_are_parent_scoped_and_global_aliases_are_unambiguous() {
    let (directory, path) = fixture();
    let categories = directory.path().join("categories.toml");
    fs::write(&categories,"schema=1\n[categories.'clothing.top.tshirt']\nlabel='T-shirts'\n[categories.'clothing.upper']\nlabel='Upper'\n[categories.'clothing.lower']\nlabel='Lower'\n[categories.'clothing.upper.other']\nparent='clothing.upper'\nlabel='Other'\naliases=['Upper alternatives']\n[categories.'clothing.lower.other']\nparent='clothing.lower'\nlabel='Other'\naliases=['Lower alternatives']\n").unwrap();
    CompiledCatalog::load(&path).unwrap();
    edit(&categories, "Lower alternatives", "UPPER ALTERNATIVES");
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "category_alias_collision",
    );
    let (_directory, path) = fixture();
    edit(
        &path,
        "name = \"Crew tee\"",
        "name = \"Crew tee\"\nname = \"Duplicate TOML key\"",
    );
    assert!(CompiledCatalog::load(&path).is_err());
}
#[test]
fn sizes_keep_zeroes_units_scopes_and_reported_labels_without_conversion() {
    let (directory, path) = fixture();
    let brands = directory.path().join("brands.toml");
    fs::write(&brands,fs::read_to_string(&brands).unwrap()+"\n[size_profiles.uk]\nsystem='uk'\nkind='clothing'\n[size_profiles.us]\nsystem='us'\nkind='clothing'\n").unwrap();
    let mut authored = document(&path);
    authored.inventory[0].size = ReportedSize {
        primary: SizedLabel {
            profile: "uk".to_owned().try_into().unwrap(),
            value: SizeValue::Numeric {
                label: "6".to_owned().try_into().unwrap(),
            },
        },
        also_reported: vec![SizedLabel {
            profile: "us".to_owned().try_into().unwrap(),
            value: SizeValue::Numeric {
                label: "2".to_owned().try_into().unwrap(),
            },
        }],
    };
    produce(&path, &authored);
    let source = CompiledCatalog::load(&path).unwrap();
    assert_eq!(source.items()[0].size.as_str(), "UK 6 / US 2");
    for label in ["01", "1.0", "+1", "1e1", " 0"] {
        assert!(NativeNumber::try_from(label.to_owned()).is_err());
    }
    assert_ne!(
        NativeNumber::try_from("00".to_owned()).unwrap(),
        NativeNumber::try_from("0".to_owned()).unwrap()
    );
    assert!(matches!(
        NativeNumber::try_from("37.5".to_owned()).unwrap(),
        NativeNumber::Half(37)
    ));
    edit(&brands, "kind='clothing'", "kind='eyewear'");
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "size_profile_category_mismatch",
    );
    let (directory, path) = fixture();
    edit(
        &directory.path().join("categories.toml"),
        "label = \"T-shirts\"",
        "label = \"T-shirts\"\nsizing_kind = 'eyewear'",
    );
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "category_sizing_kind_mismatch",
    );
}
#[test]
fn color_display_preserves_literal_shade_without_replacing_the_typed_families() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    let shade = "Literal maker wash".to_owned().try_into().unwrap();
    authored.variants[0].color = ColorSpec {
        families: vec![ColorFamily::Blue].try_into().unwrap(),
        maker_shade: Some(shade),
        api_v1_display: ColorDisplay::MakerShade,
    };
    produce(&path, &authored);
    let source = CompiledCatalog::load(&path).unwrap();
    assert_eq!(source.items()[0].color.as_str(), "Literal maker wash");
    authored.variants[0].color.api_v1_display = ColorDisplay::Families;
    produce(&path, &authored);
    assert_eq!(
        CompiledCatalog::load(&path).unwrap().items()[0]
            .color
            .as_str(),
        "Blue"
    );
    authored.variants[0].color = ColorSpec {
        families: vec![ColorFamily::Multicolor].try_into().unwrap(),
        maker_shade: None,
        api_v1_display: ColorDisplay::Families,
    };
    produce(&path, &authored);
    assert_eq!(
        CompiledCatalog::load(&path).unwrap().items()[0]
            .color
            .as_str(),
        "Multicolor"
    );
    authored.variants[0].color.api_v1_display = ColorDisplay::MakerShade;
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "maker_shade_required",
    );
}

fn fiber(fiber: Fiber, qualifiers: &[FiberQualifier]) -> Constituent {
    Constituent::Fiber {
        fiber,
        qualifiers: qualifiers.to_vec().try_into().unwrap(),
    }
}
fn component(composition: Composition) -> MaterialComponent {
    MaterialComponent {
        id: None,
        color: None,
        part: MaterialPart::Main,
        construction: None,
        composition: Some(composition),
        properties: Vec::new(),
        weight: None,
        maker_wording: None,
    }
}
fn listed(parts: Vec<Constituent>) -> Composition {
    Composition::Listed {
        parts: parts.try_into().unwrap(),
        listed_as_blend: false,
    }
}
fn percent(parts: Vec<(Constituent, u16)>, complete: bool) -> Composition {
    Composition::Percent {
        parts: parts
            .into_iter()
            .map(|(constituent, n)| PercentPart {
                constituent,
                basis_points: n.try_into().unwrap(),
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
        complete,
    }
}

fn projected_material(path: &Path) -> String {
    let source = CompiledCatalog::load(path).unwrap();
    source.materialize(&urls(&source)).unwrap().items()[0]
        .material
        .clone()
        .unwrap()
}

#[test]
fn leather_finishes_retain_material_nouns_in_real_projection() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    for (finish, expected) in [
        (None, "Leather"),
        (Some(Finish::Smooth), "Smooth leather"),
        (Some(Finish::Patent), "Patent leather"),
        (Some(Finish::Grained), "Grained leather"),
        (Some(Finish::Pebbled), "Pebbled leather"),
        (Some(Finish::Embossed), "Embossed leather"),
        (Some(Finish::Nappa), "Nappa leather"),
        (Some(Finish::Box), "Box leather"),
        (Some(Finish::Saffiano), "Saffiano leather"),
        (Some(Finish::HairOn), "Hair-on leather"),
        (Some(Finish::Suede), "Suede"),
        (Some(Finish::WaxedSuede), "Waxed suede"),
        (Some(Finish::Nubuck), "Nubuck"),
        (Some(Finish::Shearling), "Shearling"),
    ] {
        authored.variants[0].materials = vec![component(listed(vec![Constituent::Leather {
            animal: None,
            finish,
        }]))]
        .try_into()
        .unwrap();
        produce(&path, &authored);
        assert_eq!(projected_material(&path), expected, "{finish:?}");
    }
}

#[test]
fn leather_animal_labels_retain_nouns_without_duplicate_skin_or_hide() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    for (animal, expected) in [
        (LeatherAnimal::Bovine, "Bovine leather"),
        (LeatherAnimal::Calf, "Calfskin"),
        (LeatherAnimal::Lamb, "Lambskin"),
        (LeatherAnimal::Sheep, "Sheepskin"),
        (LeatherAnimal::Goat, "Goatskin"),
        (LeatherAnimal::Pig, "Pigskin"),
        (LeatherAnimal::Deer, "Deerskin"),
        (LeatherAnimal::Horse, "Horsehide"),
        (LeatherAnimal::Buffalo, "Buffalo leather"),
        (LeatherAnimal::Snake, "Snakeskin"),
        (LeatherAnimal::Crocodile, "Crocodile leather"),
        (LeatherAnimal::Alligator, "Alligator leather"),
        (LeatherAnimal::Ostrich, "Ostrich leather"),
        (LeatherAnimal::Lizard, "Lizard leather"),
    ] {
        authored.variants[0].materials = vec![component(listed(vec![Constituent::Leather {
            animal: Some(animal),
            finish: None,
        }]))]
        .try_into()
        .unwrap();
        produce(&path, &authored);
        assert_eq!(projected_material(&path), expected, "{animal:?}");
    }
}

#[test]
fn leather_nouns_survive_percentages_and_separate_components() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    let main = component(percent(
        vec![(
            Constituent::Leather {
                animal: None,
                finish: Some(Finish::Pebbled),
            },
            5000,
        )],
        false,
    ));
    let mut lining = component(listed(vec![Constituent::Leather {
        animal: None,
        finish: Some(Finish::Suede),
    }]));
    lining.part = MaterialPart::Lining;
    let mut trim = component(listed(vec![Constituent::Leather {
        animal: Some(LeatherAnimal::Calf),
        finish: Some(Finish::Smooth),
    }]));
    trim.part = MaterialPart::Trim;
    let mut sole = component(percent(
        vec![(
            Constituent::Leather {
                animal: Some(LeatherAnimal::Bovine),
                finish: None,
            },
            10000,
        )],
        true,
    ));
    sole.part = MaterialPart::Sole;
    authored.variants[0].materials = vec![main, lining, trim, sole].try_into().unwrap();
    produce(&path, &authored);
    assert_eq!(
        projected_material(&path),
        "50% Pebbled leather; Lining: Suede; Trim: Smooth Calfskin; Sole: Bovine leather"
    );
}
#[test]
fn compositions_preserve_unknown_amounts_component_totals_and_real_qualifiers() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    authored.variants[0].materials = vec![component(listed(vec![fiber(Fiber::Cashmere, &[])]))]
        .try_into()
        .unwrap();
    produce(&path, &authored);
    let listed_source = CompiledCatalog::load(&path).unwrap();
    assert_eq!(listed_source.items()[0].material.as_str(), "Cashmere");
    authored.variants[0].materials[0].composition =
        Some(percent(vec![(fiber(Fiber::Cashmere, &[]), 10000)], true));
    produce(&path, &authored);
    let complete = CompiledCatalog::load(&path).unwrap();
    assert_eq!(
        complete.items()[0].material,
        listed_source.items()[0].material
    );
    assert_ne!(complete.source_hash(), listed_source.source_hash());
    authored.variants[0].materials[0].composition =
        Some(percent(vec![(fiber(Fiber::Cashmere, &[]), 5000)], false));
    produce(&path, &authored);
    assert_eq!(
        CompiledCatalog::load(&path).unwrap().items()[0]
            .material
            .as_str(),
        "50% Cashmere"
    );
    for (parts, complete) in [
        (vec![(fiber(Fiber::Cotton, &[]), 9900)], true),
        (vec![(fiber(Fiber::Cotton, &[]), 10000)], false),
    ] {
        authored.variants[0].materials[0].composition = Some(percent(parts, complete));
        produce(&path, &authored);
        code(
            &CompiledCatalog::load(&path).unwrap_err(),
            "invalid_composition_total",
        );
    }
    authored.variants[0].materials[0].composition = Some(percent(
        vec![
            (fiber(Fiber::Polyester, &[FiberQualifier::Recycled]), 6000),
            (fiber(Fiber::Polyester, &[]), 4000),
        ],
        true,
    ));
    produce(&path, &authored);
    CompiledCatalog::load(&path).unwrap();
    authored.variants[0].materials[0].composition = Some(listed(vec![
        fiber(Fiber::from_legacy("spandex").unwrap(), &[]),
        fiber(Fiber::Elastane, &[]),
    ]));
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "duplicate_constituent",
    );
    authored.variants[0].materials[0].composition = Some(listed(vec![fiber(
        Fiber::Polyester,
        &[FiberQualifier::Merino],
    )]));
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "incompatible_fiber_qualifier",
    );
}
#[test]
fn repeatable_lace_trade_claims_polymers_weight_and_color_remain_structured() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    let mut first = component(percent(vec![(fiber(Fiber::Cotton, &[]), 10000)], true));
    first.id = Some("panel_a".to_owned().try_into().unwrap());
    first.part = MaterialPart::Lace;
    let mut second = component(percent(vec![(fiber(Fiber::Polyester, &[]), 10000)], true));
    second.id = Some("panel_b".to_owned().try_into().unwrap());
    second.part = MaterialPart::Lace;
    let mut main = component(listed(vec![Constituent::TradeMaterial {
        claim: TradeMaterialClaim::PyratexCirculose,
    }]));
    main.construction = Some(Construction::Canvas);
    main.weight = Some(FabricWeight {
        amount: 24.0.try_into().unwrap(),
        unit: WeightUnit::Ounce,
    });
    main.properties = vec![MaterialProperty::HeavyDuty];
    main.color = Some(ColorSpec {
        families: vec![ColorFamily::Brown].try_into().unwrap(),
        maker_shade: None,
        api_v1_display: ColorDisplay::Families,
    });
    let mut frame = component(listed(vec![Constituent::Polymer {
        polymer: Polymer::Acetate,
        recycled: true,
    }]));
    frame.part = MaterialPart::Frame;
    authored.variants[0].materials = vec![first, second, main, frame].try_into().unwrap();
    produce(&path, &authored);
    let source = CompiledCatalog::load(&path).unwrap();
    let rendered = source.items()[0].material.as_str();
    for fact in [
        "PYRATEX® Circulose",
        "24 oz",
        "Heavy-duty",
        "Brown",
        "Recycled Acetate",
        "Lace: Cotton",
        "Lace: Polyester",
    ] {
        assert!(rendered.contains(fact), "{rendered}");
    }
    assert!(!rendered.contains("Viscose"));
    authored.variants[0].materials[1].id = None;
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "component_identity_required",
    );
}
#[test]
fn empty_override_is_rejected_and_valid_override_provenance_is_preserved() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    authored.inventory[0].images = Some(Vec::new());
    let SourceImage::Asset { reference, .. } =
        &mut authored.inventory[1].images.as_mut().unwrap()[1]
    else {
        panic!("asset fixture")
    };
    reference.kind = ImageKind::MerchantEmailProductReference;
    reference.notes = Some("Fictional provenance".to_owned().try_into().unwrap());
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "images_required",
    );
    authored.inventory[0].images = None;
    produce(&path, &authored);
    let source = CompiledCatalog::load(&path).unwrap();
    assert!(!source.items()[0].images.is_empty());
    assert_eq!(source.assets()[0].slots.len(), 2);
    let projected = source.materialize(&urls(&source)).unwrap();
    assert_eq!(
        projected.items()[1].images[1].kind,
        "merchant_email_product_reference"
    );
    assert_eq!(
        projected.items()[1].images[1].notes.as_deref(),
        Some("Fictional provenance")
    );
}
#[test]
fn asset_directory_is_bounded_and_never_follows_symlinks_or_accepts_hidden_additions() {
    let (directory, path) = fixture();
    let source = CompiledCatalog::load(&path).unwrap();
    fs::write(
        directory.path().join("wardrobe-images/unlisted.png"),
        b"unexpected",
    )
    .unwrap();
    assert_eq!(
        source.verify_unchanged().unwrap_err().code,
        "catalog_source_changed"
    );
    assert!(CompiledCatalog::load(&path).is_err());
    let (directory, path) = fixture();
    #[cfg(unix)]
    {
        let source = CompiledCatalog::load(&path).unwrap();
        let asset = &source.assets()[0].path;
        fs::remove_file(asset).unwrap();
        std::os::unix::fs::symlink(directory.path().join("brands.toml"), asset).unwrap();
        assert!(CompiledCatalog::load(&path).is_err());
    }
}
#[test]
fn real_cli_requires_toml_support_before_credentials_or_any_network() {
    #[derive(Deserialize)]
    struct Envelope {
        data: Report,
    }
    #[derive(Deserialize)]
    struct Report {
        valid: bool,
        count: usize,
    }
    let (_directory, path) = fixture();
    let binary = std::env::var_os("CLOSET_TEST_CLI_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_ha-closet").into());
    let output = Command::new(binary)
        .args([
            "--token-file",
            "/does-not-exist",
            "inventory",
            "validate",
            "--input",
            path.to_str().unwrap(),
        ])
        .env_remove("HOME_ASSISTANT_TOKEN")
        .env_remove("CLOSET_API_TOKEN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Envelope = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.data.valid);
    assert_eq!(report.data.count, 2);
}

#[test]
fn global_category_alias_cannot_claim_another_scoped_display_label() {
    let (directory, path) = fixture();
    let categories = directory.path().join("categories.toml");
    fs::write(
        &categories,
        fs::read_to_string(&categories).unwrap()
            + "\n[categories.\"clothing.bottom\"]\nlabel='Bottoms'\naliases=['T-shirts']\n",
    )
    .unwrap();
    code(
        &CompiledCatalog::load(&path)
            .expect_err("global alias must not shadow another canonical category label"),
        "category_alias_label_collision",
    );
}

#[test]
fn every_toml_purchase_requires_explicit_positive_quantity() {
    let (_directory, path) = fixture();
    edit(&path, "quantity = 2", "");
    assert!(CompiledCatalog::load(&path).is_err());
    let (_directory, path) = fixture();
    edit(&path, "quantity = 2", "quantity = 0");
    assert!(CompiledCatalog::load(&path).is_err());
}

#[test]
fn strict_authoring_requires_every_complete_core_fact_without_fallbacks() {
    for field in [
        "brand",
        "name",
        "category",
        "color",
        "materials",
        "size",
        "price",
        "quantity",
        "source",
        "images",
    ] {
        let (_directory, path) = fixture();
        let original = fs::read_to_string(&path).unwrap();
        let mut removed = false;
        let changed = original
            .lines()
            .filter(|line| {
                if !removed && line.starts_with(&format!("{field} =")) {
                    removed = true;
                    false
                } else {
                    true
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(removed);
        fs::write(&path, changed).unwrap();
        assert!(
            CompiledCatalog::load(&path).is_err(),
            "missing required {field}"
        );
    }
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    authored.inventory[0].images = authored.inventory[1].images.clone();
    authored.variants[0].images.clear();
    produce(&path, &authored);
    assert_eq!(CompiledCatalog::load(&path).unwrap().item_count(), 2);
}

#[test]
fn material_reordering_is_a_noop_and_distinct_styles_may_share_display_names() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    authored.variants[0].materials = vec![component(percent(
        vec![
            (fiber(Fiber::Cotton, &[]), 6000),
            (fiber(Fiber::Silk, &[]), 4000),
        ],
        true,
    ))]
    .try_into()
    .unwrap();
    produce(&path, &authored);
    let before = CompiledCatalog::load(&path).unwrap();
    let projected = before.materialize(&urls(&before)).unwrap();
    let Some(Composition::Percent { parts, .. }) =
        &mut authored.variants[0].materials[0].composition
    else {
        panic!("percent fixture")
    };
    parts.reverse();
    produce(&path, &authored);
    let after = CompiledCatalog::load(&path).unwrap();
    assert_eq!(before.source_hash(), after.source_hash());
    assert_eq!(
        projected.report().source_hash,
        after
            .materialize(&urls(&after))
            .unwrap()
            .report()
            .source_hash
    );
    let mut style = authored.styles[0].clone();
    style.id = "another_style".to_owned().try_into().unwrap();
    authored.styles.push(style);
    let mut variant = authored.variants[0].clone();
    variant.id = "another_variant".to_owned().try_into().unwrap();
    variant.style = authored.styles[1].id.clone();
    authored.variants.push(variant);
    authored.inventory[1].variant = authored.variants[1].id.clone();
    produce(&path, &authored);
    assert_eq!(CompiledCatalog::load(&path).unwrap().item_count(), 2);
}
#[test]
fn aggregate_asset_sizes_are_preflighted_before_reading_even_one_untrusted_png() {
    let (directory, path) = fixture();
    let mut authored = document(&path);
    for index in 1..=60 {
        let mut variant = authored.variants[0].clone();
        variant.id = format!("unused_variant_{index}").try_into().unwrap();
        variant.images.truncate(1);
        let SourceImage::Asset { sha256, .. } = &mut variant.images[0] else {
            panic!("asset fixture")
        };
        *sha256 = format!("{index:064x}").try_into().unwrap();
        let file = fs::File::create(
            directory
                .path()
                .join("wardrobe-images")
                .join(format!("{sha256}.png")),
        )
        .unwrap();
        file.set_len(closet::images::MAX_IMAGE_BYTES).unwrap();
        authored.variants.push(variant);
    }
    produce(&path, &authored);
    assert_eq!(
        CompiledCatalog::load(&path).unwrap_err().code,
        "image_total_too_large"
    );
}
#[test]
fn supplier_urls_are_exact_and_duplicate_or_native_references_are_rejected() {
    let (_directory, path) = fixture();
    let mut authored = document(&path);
    let original = "https://IMAGES.example.invalid/Exact%2FCase.png?label=Keep%20Case#Photo";
    let SourceImage::Remote { url, .. } = &mut authored.inventory[1].images.as_mut().unwrap()[0]
    else {
        panic!("remote fixture")
    };
    *url = original.to_owned().try_into().unwrap();
    produce(&path, &authored);
    let source = CompiledCatalog::load(&path).unwrap();
    assert_eq!(
        source.materialize(&urls(&source)).unwrap().items()[1].images[0].url,
        original
    );
    let duplicate = authored.variants[0].images[0].clone();
    authored.variants[0].images.push(duplicate);
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "duplicate_image_reference",
    );
    authored.variants[0].images.pop();
    let SourceImage::Remote { url, .. } = &mut authored.inventory[1].images.as_mut().unwrap()[0]
    else {
        panic!("remote fixture")
    };
    *url="https://ha.example.invalid/api/image/serve/11111111111111111111111111111111/256x256-photo.png".to_owned().try_into().unwrap();
    produce(&path, &authored);
    code(
        &CompiledCatalog::load(&path).unwrap_err(),
        "native_url_not_source_identity",
    );
}

#[test]
fn every_color_requires_a_closed_family_even_when_maker_shade_is_displayed() {
    let (_directory, path) = fixture();
    edit(&path, "families = [\"navy\"], ", "");
    edit(
        &path,
        "api_v1_display = \"families\"",
        "api_v1_display = \"maker_shade\"",
    );
    assert!(
        CompiledCatalog::load(&path).is_err(),
        "literal maker shade must not bypass the typed color family"
    );
    let (_directory, path) = fixture();
    edit(&path, "families = [\"navy\"]", "families = []");
    assert!(CompiledCatalog::load(&path).is_err());
    let (_directory, path) = fixture();
    edit(
        &path,
        "families = [\"navy\"]",
        "families = [\"a literal unknown family\"]",
    );
    assert!(CompiledCatalog::load(&path).is_err());
}

#[test]
fn guaranteed_oversized_flattening_fails_offline_before_credentials() {
    let (directory, path) = fixture();
    edit(
        &directory.path().join("brands.toml"),
        "name = \"North Studio\"",
        &format!("name = {:?}", "B".repeat(1024)),
    );
    let mut authored = document(&path);
    authored.styles[0].name = "N".repeat(512).try_into().unwrap();
    let remote = authored.inventory[1].images.as_ref().unwrap()[0].clone();
    authored.variants[0].images = vec![remote];
    let template = authored.inventory[0].clone();
    authored.inventory = (0..1000)
        .map(|index| {
            let mut entry = template.clone();
            entry.id = format!("fictional-purchase-{index}").try_into().unwrap();
            entry.images = None;
            entry
        })
        .collect();
    produce(&path, &authored);
    fs::remove_dir_all(directory.path().join("wardrobe-images")).unwrap();
    let error = CompiledCatalog::load(&path)
        .expect_err("deduplicated authoring must not admit an impossible flat projection");
    assert_eq!(error.code, "catalog_too_large");
    code(&error, "catalog_too_large");
    let output = Command::new(env!("CARGO_BIN_EXE_ha-closet"))
        .args([
            "--token-file",
            "/does-not-exist",
            "inventory",
            "validate",
            "--input",
            path.to_str().unwrap(),
        ])
        .env_remove("HOME_ASSISTANT_TOKEN")
        .env_remove("CLOSET_API_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("token"));
    #[derive(Deserialize)]
    struct Failure {
        error_code: String,
    }
    let failure: Failure = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(failure.error_code, "catalog_too_large");
}
