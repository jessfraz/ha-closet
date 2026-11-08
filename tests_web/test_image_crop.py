"""Render non-destructive viewports through the real catalog and gallery."""

from pathlib import Path

import pytest
from playwright.async_api import Locator, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_navigation_gallery import connect_at


async def crop_photo(route: Route, width: int, height: int) -> None:
    """A blue rectangle surrounded by red, with known original coordinates."""
    await route.fulfill(
        content_type="image/svg+xml",
        body=(
            '<svg xmlns="http://www.w3.org/2000/svg" '
            f'width="{width}" height="{height}">'
            f'<rect width="{width}" height="{height}" fill="#f02020"/>'
            f'<rect x="{width / 4}" y="{height / 8}" '
            f'width="{width / 2}" height="{height * 3 / 4}" fill="#2050f0"/>'
            "</svg>"
        ),
    )


async def assert_crop_geometry(
    photo: Locator, crop: tuple[int, int, int, int], original_size: tuple[int, int]
) -> None:
    """Check the decoded original's scale, offset, clipping and responsive fit."""
    viewport = photo.locator(".photo-crop")
    await expect(viewport).to_have_count(1)
    image = viewport.locator("img")
    await expect(image).to_have_attribute(
        "src", "https://images.example.invalid/jacket.svg"
    )
    assert await image.evaluate(
        "image => image.complete && [image.naturalWidth, image.naturalHeight]"
    ) == list(original_size)
    outer = await photo.bounding_box()
    frame = await viewport.bounding_box()
    pixels = await image.bounding_box()
    assert outer is not None and frame is not None and pixels is not None
    x, y, width, height = crop
    original_aspect = original_size[0] / original_size[1]
    aspect = original_aspect * width / height
    expected_width = min(outer["width"], outer["height"] * aspect)
    assert frame["width"] == pytest.approx(expected_width, abs=0.1)
    assert frame["height"] == pytest.approx(expected_width / aspect, abs=0.1)
    assert frame["x"] == pytest.approx(
        outer["x"] + (outer["width"] - frame["width"]) / 2, abs=0.1
    )
    assert frame["y"] == pytest.approx(
        outer["y"] + (outer["height"] - frame["height"]) / 2, abs=0.1
    )
    assert pixels["width"] == pytest.approx(frame["width"] * 10000 / width, abs=0.1)
    assert pixels["height"] == pytest.approx(frame["height"] * 10000 / height, abs=0.1)
    assert pixels["width"] == pytest.approx(pixels["height"] * original_aspect, abs=0.1)
    assert pixels["x"] == pytest.approx(
        frame["x"] - pixels["width"] * x / 10000, abs=0.1
    )
    assert pixels["y"] == pytest.approx(
        frame["y"] - pixels["height"] * y / 10000, abs=0.1
    )
    assert await viewport.evaluate(
        "frame => [getComputedStyle(frame).overflowX, "
        "getComputedStyle(frame).overflowY]"
    ) == ["hidden", "hidden"]


@pytest.mark.parametrize("width", [390, 1100])
async def test_same_crop_renders_in_inventory_gallery_and_thumbnails(
    rust_backend: RustBackend, tmp_path: Path, width: int
) -> None:
    item = catalog_item(name="Example crop jacket")
    item["images"] = [
        {
            "url": "https://images.example.invalid/jacket.svg",
            "kind": "product_reference",
            "confidence": "high",
            "crop": {"x": 2500, "y": 1250, "width": 5000, "height": 7500},
        },
        {
            "url": "https://images.example.invalid/jacket.svg",
            "kind": "product_reference",
            "confidence": "high",
            "crop": {"x": 0, "y": 0, "width": 2500, "height": 1250},
        },
        {
            "url": "https://images.example.invalid/jacket.svg",
            "kind": "product_reference",
            "confidence": "high",
        },
        {
            "url": "https://images.example.invalid/broken.svg",
            "kind": "product_reference",
            "confidence": "high",
            "crop": {"x": 2500, "y": 1250, "width": 5000, "height": 7500},
        },
    ]
    await rust_backend.sync([item])
    original_size = (400, 800) if width == 390 else (800, 400)

    async def original_photo(route: Route) -> None:
        await crop_photo(route, *original_size)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.route("https://images.example.invalid/**", original_photo)
            await page.route(
                "https://images.example.invalid/broken.svg", lambda route: route.abort()
            )
            await connect_at(page, rust_backend, "?panel=keep")
            card = page.locator(".item-card .photo")
            await assert_crop_geometry(card, (2500, 1250, 5000, 7500), original_size)
            await card.screenshot(path=str(tmp_path / f"crop-inventory-{width}.png"))
            await page.get_by_role("button", name="View Example crop jacket").click()
            dialog = page.get_by_role("dialog", name="Example crop jacket")
            main = dialog.locator(".gallery-main .photo")
            await assert_crop_geometry(main, (2500, 1250, 5000, 7500), original_size)
            await assert_crop_geometry(
                dialog.get_by_role("button", name="Show photo 1").locator(".photo"),
                (2500, 1250, 5000, 7500),
                original_size,
            )
            await assert_crop_geometry(
                dialog.get_by_role("button", name="Show photo 2").locator(".photo"),
                (0, 0, 2500, 1250),
                original_size,
            )
            await expect(
                dialog.get_by_role("link", name="Open photo")
            ).to_have_attribute("href", "https://images.example.invalid/jacket.svg")
            await main.screenshot(path=str(tmp_path / f"crop-gallery-{width}.png"))
            await dialog.get_by_role("button", name="Next photo", exact=True).click()
            await assert_crop_geometry(main, (0, 0, 2500, 1250), original_size)
            second = dialog.get_by_role("button", name="Show photo 2", exact=True)
            await second.focus()
            await page.keyboard.press("ArrowRight")
            await expect(dialog.locator(".gallery-counter")).to_have_text("3 of 4")
            await expect(main.locator(".photo-crop")).to_have_count(0)
            await expect(main.locator(":scope > img")).to_have_count(1)
            await page.keyboard.press("End")
            await expect(main.get_by_text("No photo", exact=True)).to_be_visible()
            await expect(dialog.get_by_role("link", name="Open photo")).to_have_count(0)
            await expect(
                dialog.get_by_role("button", name="Show photo 4", exact=True)
            ).to_be_focused()
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            await dialog.get_by_role("button", name="Close item details").click()
            assert page.url.endswith("?panel=keep")
            await page.set_viewport_size({"width": 620, "height": 900})
            await assert_crop_geometry(card, (2500, 1250, 5000, 7500), original_size)
        finally:
            await browser.close()
