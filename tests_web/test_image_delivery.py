"""Serve small catalog previews without sacrificing the original detail photo."""

from io import BytesIO
from urllib.parse import urlsplit

import pytest
from PIL import Image, ImageDraw
from playwright.async_api import Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_app import login


def product_png(size: tuple[int, int]) -> bytes:
    """A transparent fictional product, not a household photograph."""
    image = Image.new("RGBA", size)
    draw = ImageDraw.Draw(image)
    width, height = size
    draw.rectangle(
        (width // 4, height // 8, width * 3 // 4, height * 7 // 8),
        fill="#30475f",
    )
    output = BytesIO()
    image.save(output, format="PNG")
    return output.getvalue()


async def test_grid_defers_distant_previews_and_gallery_loads_originals(
    rust_backend: RustBackend,
) -> None:
    records = []
    for index in range(40):
        record = catalog_item(
            f"example-preview-{index:02}", name=f"Example garment {index:02}"
        )
        record["images"] = [
            {
                "url": f"https://images.example.invalid/api/image/serve/{index:032x}/original",
                "kind": "source_photo",
                "confidence": "high",
            }
        ]
        records.append(record)
    first_images = records[0]["images"]
    assert isinstance(first_images, list)
    first_images.append(
        {
            "url": f"https://images.example.invalid/api/image/serve/{40:032x}/original",
            "kind": "source_photo",
            "confidence": "high",
        }
    )
    await rust_backend.sync(records)
    bodies = {
        "original": product_png((2048, 2560)),
        "512x512": product_png((410, 512)),
        "256x256": product_png((205, 256)),
    }
    requested: list[str] = []

    async def serve(route: Route) -> None:
        path = urlsplit(route.request.url).path
        requested.append(path)
        body = bodies.get(path.rsplit("/", 1)[-1])
        assert body is not None
        await route.fulfill(content_type="image/png", body=body)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": 1100, "height": 900}, device_scale_factor=2
            )
            await page.route("https://images.example.invalid/**", serve)
            await login(page, rust_backend)
            cards = page.locator(".item-card")
            await expect(cards).to_have_count(40)
            first = cards.first.locator(".photo img")
            await expect(first).to_have_js_property("naturalHeight", 512)
            await expect(first).to_have_js_property("loading", "eager")
            await expect(first).to_have_js_property("fetchPriority", "high")
            assert not any(path.endswith("/original") for path in requested)
            assert not any(f"/{39:032x}/" in path for path in requested)

            await cards.first.get_by_role(
                "button", name="View Example garment 00"
            ).click()
            dialog = page.get_by_role("dialog")
            main = dialog.locator(".gallery-main img")
            await expect(main).to_have_js_property("naturalHeight", 2560)
            for image in await dialog.locator(".photo-thumbnail img").all():
                await expect(image).to_have_js_property("naturalHeight", 256)
            assert not any(path.endswith(f"/{40:032x}/original") for path in requested)
            await dialog.get_by_role("button", name="Next photo", exact=True).click()
            await expect(dialog.locator(".gallery-main img")).to_have_js_property(
                "naturalHeight", 2560
            )
            await expect(
                dialog.get_by_role("link", name="Open photo")
            ).to_have_attribute(
                "href",
                f"https://images.example.invalid/api/image/serve/{40:032x}/original",
            )
            await dialog.get_by_role("button", name="Close item details").click()
            await cards.last.scroll_into_view_if_needed()
            await expect(cards.last.locator(".photo img")).to_have_js_property(
                "naturalHeight", 512
            )
        finally:
            await browser.close()


@pytest.mark.parametrize("original_fails", [False, True])
async def test_failed_native_preview_falls_back_once_to_original(
    rust_backend: RustBackend, original_fails: bool
) -> None:
    record = catalog_item(name="Example fallback garment")
    record["images"] = [
        {
            "url": "https://images.example.invalid/api/image/serve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/original",
            "kind": "source_photo",
            "confidence": "high",
        }
    ]
    await rust_backend.sync([record])
    original = product_png((2048, 2560))
    requested: list[str] = []

    async def serve(route: Route) -> None:
        filename = urlsplit(route.request.url).path.rsplit("/", 1)[-1]
        requested.append(filename)
        if filename != "original" or original_fails:
            await route.abort()
        else:
            await route.fulfill(content_type="image/png", body=original)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://images.example.invalid/**", serve)
            await login(page, rust_backend)
            photo = page.locator(".item-card .photo")
            if original_fails:
                await expect(photo.get_by_text("No photo", exact=True)).to_be_visible()
            else:
                await expect(photo.locator("img")).to_have_js_property(
                    "naturalHeight", 2560
                )
            assert requested == ["512x512", "original"]
        finally:
            await browser.close()


@pytest.mark.parametrize("cropped", [False, True])
async def test_signed_or_cropped_photos_keep_original_resolution(
    rust_backend: RustBackend, cropped: bool
) -> None:
    record = catalog_item(name="Example original-resolution garment")
    url = "https://images.example.invalid/api/image/serve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/original"
    record["images"] = [
        {
            "url": url if cropped else url + "?version=example",
            "kind": "source_photo",
            "confidence": "high",
        }
    ]
    if cropped:
        images = record["images"]
        assert isinstance(images, list)
        reference = images[0]
        assert isinstance(reference, dict)
        reference["crop"] = {
            "x": 2500,
            "y": 0,
            "width": 5000,
            "height": 5000,
        }
    await rust_backend.sync([record])
    original = product_png((2048, 2560))
    requested: list[str] = []

    async def serve(route: Route) -> None:
        requested.append(route.request.url)
        await route.fulfill(content_type="image/png", body=original)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://images.example.invalid/**", serve)
            await login(page, rust_backend)
            await expect(page.locator(".item-card .photo img")).to_have_js_property(
                "naturalHeight", 2560
            )
            assert requested == [url if cropped else url + "?version=example"]
        finally:
            await browser.close()
