//! The real TOML compiler and CLI preserve an unrecorded purchase price.
use closet::{api, source::CompiledCatalog, store::Store};
use std::{collections::BTreeMap, fs, process::Command, sync::Arc};

#[tokio::test(flavor = "multi_thread")]
async fn unknown_price_compiles_syncs_and_reads_back_without_a_zero_substitute() {
    let temp = tempfile::tempdir().unwrap();
    for file in [
        "wardrobe.toml",
        "brands.toml",
        "categories.toml",
        "fittings.toml",
    ] {
        fs::copy(
            format!("config/wardrobe.example/{file}"),
            temp.path().join(file),
        )
        .unwrap();
    }
    let path = temp.path().join("wardrobe.toml");
    let text = fs::read_to_string(&path).unwrap();
    let mut removed = false;
    let text = text
        .lines()
        .filter(|line| {
            if !removed && line.starts_with("price =") {
                removed = true;
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(removed);
    fs::write(&path, text).unwrap();
    let source = CompiledCatalog::load(&path).unwrap();
    let projected = source.materialize(&BTreeMap::new()).unwrap();
    assert_eq!(projected.items()[0].price, None);
    assert_eq!(projected.items()[1].price.as_ref().unwrap().amount, 50.0);
    let validated = Command::new(env!("CARGO_BIN_EXE_ha-closet"))
        .args(["inventory", "validate", "--input"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        validated.status.success(),
        "{}",
        String::from_utf8_lossy(&validated.stdout)
    );
    let store = Store::open(temp.path().join("closet.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(
            store.clone(),
            "fictional-unknown-price-owner-credential",
        )),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let cli_path = path.clone();
    let state = temp.path().join("sync-state");
    let applied = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_ha-closet"))
            .env(
                "CLOSET_API_TOKEN",
                "fictional-unknown-price-owner-credential",
            )
            .args([
                "--transport",
                "native",
                "--url",
                &origin,
                "inventory",
                "sync",
                "--input",
            ])
            .arg(cli_path)
            .arg("--state-dir")
            .arg(state)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        applied.status.success(),
        "{} {}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        store
            .item("example-purchase-a")
            .await
            .unwrap()
            .catalog
            .price,
        None
    );
    assert_eq!(
        store
            .item("example-purchase-b")
            .await
            .unwrap()
            .catalog
            .price
            .as_ref()
            .unwrap()
            .amount,
        50.0
    );
    server.close().await.unwrap();
    store.close().await;
}
