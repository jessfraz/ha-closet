//! Fictional source files shared by real CLI and TLS sync consumers.
use base64::Engine;
use std::path::{Path, PathBuf};

pub fn write(directory: &Path) -> PathBuf {
    for (name, text) in [
        (
            "wardrobe.toml",
            include_str!("../fixtures/toml/wardrobe.toml"),
        ),
        ("brands.toml", include_str!("../fixtures/toml/brands.toml")),
        (
            "categories.toml",
            include_str!("../fixtures/toml/categories.toml"),
        ),
        (
            "fittings.toml",
            include_str!("../fixtures/toml/fittings.toml"),
        ),
    ] {
        std::fs::write(directory.join(name), text).unwrap();
    }
    let assets = directory.join("wardrobe-images");
    std::fs::create_dir_all(&assets).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(concat!(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk",
            "+A8AAQUBAScY42YAAAAASUVORK5CYII=",
        ))
        .unwrap();
    let sha = closet::images::ImageSha::digest(&bytes);
    std::fs::write(assets.join(format!("{sha}.png")), bytes).unwrap();
    directory.join("wardrobe.toml")
}
