"""Uncropped references stay fully visible in every shared photo viewport."""

from io import BytesIO
from pathlib import Path

import pytest
from PIL import Image
from playwright.async_api import Locator, Route, async_playwright

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_navigation_gallery import connect_at

MARKERS = [(240, 30, 30), (30, 180, 60), (200, 30, 200), (245, 195, 20)]
SIZES = {"portrait": (1100, 1687), "landscape": (1687, 1100)}


async def reference_photo(route: Route) -> None:
    """Four colored corners make lost source pixels observable after rendering."""
    orientation = Path(route.request.url).stem
    width, height = SIZES[orientation]
    corners = [(0, 0), (0.9, 0), (0, 0.9), (0.9, 0.9)]
    markers = "".join(
        f'<rect x="{x * width}" y="{y * height}" width="{width / 10}" '
        f'height="{height / 10}" fill="rgb({red},{green},{blue})"/>'
        for (x, y), (red, green, blue) in zip(corners, MARKERS, strict=True)
    )
    await route.fulfill(
        content_type="image/svg+xml",
        body=(
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" '
            f'height="{height}"><rect width="100%" height="100%" '
            f'fill="#718096"/>{markers}</svg>'
        ),
    )


async def assert_whole_photo(photo: Locator, size: tuple[int, int]) -> bytes:
    image = photo.locator(":scope > img")
    await image.evaluate("image => image.decode()")
    assert await image.evaluate(
        "image => [image.naturalWidth, image.naturalHeight]"
    ) == list(size)
    frame = await photo.bounding_box()
    rendered = await image.bounding_box()
    assert frame is not None and rendered is not None
    assert rendered["x"] >= frame["x"] - 0.5
    assert rendered["y"] >= frame["y"] - 0.5
    assert rendered["x"] + rendered["width"] <= frame["x"] + frame["width"] + 0.5
    assert rendered["y"] + rendered["height"] <= frame["y"] + frame["height"] + 0.5, (
        "uncropped image extends below its photo viewport"
    )
    screenshot = await photo.screenshot()
    with Image.open(BytesIO(screenshot)) as pixels:
        colors = set(pixels.convert("RGB").get_flattened_data())
    assert set(MARKERS) <= colors, "the viewport clips a source-image corner"
    return screenshot


@pytest.mark.parametrize("width", [320, 390, 1100])
@pytest.mark.parametrize("orientation", ["portrait", "landscape"])
async def test_uncropped_photos_fit_cards_gallery_and_thumbnails(
    rust_backend: RustBackend, tmp_path: Path, width: int, orientation: str
) -> None:
    other = "landscape" if orientation == "portrait" else "portrait"
    orientations = [orientation, other]
    item = catalog_item(name="Example framed reference")
    item["images"] = [
        {
            "url": f"https://images.example.invalid/{name}.svg",
            "kind": "product_reference",
            "confidence": "high",
        }
        for name in orientations
    ]
    await rust_backend.sync([item])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.route("https://images.example.invalid/**", reference_photo)
            await connect_at(page, rust_backend)
            card = page.locator(".item-card .photo")
            screenshot = await assert_whole_photo(card, SIZES[orientation])
            (tmp_path / f"uncropped-card-{orientation}-{width}.png").write_bytes(
                screenshot
            )
            await page.get_by_role(
                "button", name="View Example framed reference", exact=True
            ).click()
            dialog = page.get_by_role("dialog", name="Example framed reference")
            main = dialog.locator(".gallery-main .photo")
            for index, name in enumerate(orientations, 1):
                thumbnail = dialog.get_by_role(
                    "button", name=f"Show photo {index}", exact=True
                )
                await assert_whole_photo(thumbnail.locator(".photo"), SIZES[name])
                await thumbnail.click()
                screenshot = await assert_whole_photo(main, SIZES[name])
                (tmp_path / f"uncropped-gallery-{name}-{width}.png").write_bytes(
                    screenshot
                )
            await dialog.get_by_role("button", name="Close item details").click()
            await page.set_viewport_size({"width": 620, "height": 900})
            await assert_whole_photo(card, SIZES[orientation])
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()
