"""Photo canvases stay white without changing the original garment colors."""

from io import BytesIO
from typing import Literal

import pytest
from homeassistant.util.json import JsonObjectType
from PIL import Image, ImageDraw
from playwright.async_api import Locator, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_inventory_ui import luminance
from tests_web.test_navigation_gallery import connect_at

BLUE = (37, 87, 163)
RUST = (195, 77, 43)
WHITE = (255, 255, 255)


async def transparent_photo(route: Route) -> None:
    """Opaque and translucent color blocks inside a transparent original PNG."""
    image = Image.new("RGBA", (640, 400))
    draw = ImageDraw.Draw(image)
    draw.rectangle((160, 100, 480, 300), fill=(*BLUE, 255))
    draw.rectangle((320, 140, 440, 260), fill=(*RUST, 255))
    draw.rectangle((190, 130, 260, 270), fill=(20, 160, 60, 128))
    stream = BytesIO()
    image.save(stream, format="PNG")
    await route.fulfill(content_type="image/png", body=stream.getvalue())


async def assert_white_canvas(photo: Locator, *, cropped: bool) -> None:
    image = photo.locator("img")
    await image.evaluate("image => image.decode()")
    await expect(photo).to_have_css("background-color", "rgb(255, 255, 255)")
    await expect(image).to_have_css("filter", "none")
    await expect(image).to_have_css("mix-blend-mode", "normal")
    assert await image.evaluate(
        "image => [image.naturalWidth, image.naturalHeight]"
    ) == [640, 400]
    viewport = photo.locator(".photo-crop") if cropped else image
    bounds = await viewport.bounding_box()
    frame = await photo.bounding_box()
    assert bounds is not None and frame is not None
    assert bounds["x"] >= frame["x"] - 0.5
    assert bounds["y"] >= frame["y"] - 0.5
    assert bounds["x"] + bounds["width"] <= frame["x"] + frame["width"] + 0.5
    assert bounds["y"] + bounds["height"] <= frame["y"] + frame["height"] + 0.5
    with Image.open(BytesIO(await photo.screenshot())) as screenshot:
        colors = list(screenshot.convert("RGB").get_flattened_data())
        assert colors.count(WHITE) > len(colors) * 0.3
        assert BLUE in colors and RUST in colors, "source RGB was altered"
        assert (137, 207, 157) in colors, "transparency did not composite on white"


@pytest.mark.parametrize("theme", ["light", "dark"])
@pytest.mark.parametrize("width", [390, 1100])
async def test_white_photo_canvas_preserves_pixels_in_every_view(
    rust_backend: RustBackend, width: int, theme: Literal["light", "dark"]
) -> None:
    original: JsonObjectType = {
        "url": "https://images.example.invalid/canvas.png",
        "kind": "product_reference",
        "confidence": "high",
    }
    cropped: JsonObjectType = {
        **original,
        "crop": {"x": 1250, "y": 0, "width": 7500, "height": 10000},
    }
    whole_item = catalog_item("canvas-whole", name="Example canvas coat")
    whole_item["images"] = [original, cropped]
    crop_item = catalog_item("canvas-cropped", name="Example cropped coat")
    crop_item["images"] = [cropped, original]
    empty_item = catalog_item("canvas-empty", name="Example no photo")
    await rust_backend.sync([whole_item, crop_item, empty_item])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900}, color_scheme=theme
            )
            await page.route("https://images.example.invalid/**", transparent_photo)
            await connect_at(page, rust_backend)
            for name, is_crop in [
                ("Example canvas coat", False),
                ("Example cropped coat", True),
            ]:
                card = page.locator(".item-card").filter(
                    has=page.get_by_role("button", name=f"View {name}", exact=True)
                )
                await assert_white_canvas(card.locator(".photo"), cropped=is_crop)
            placeholder = page.locator(".photo").filter(
                has=page.get_by_text("No photo", exact=True)
            )
            await expect(placeholder).to_have_css(
                "background-color",
                "rgb(240, 240, 238)" if theme == "light" else "rgb(44, 47, 49)",
            )
            ink = await placeholder.locator(".photo-placeholder").evaluate(
                "element => getComputedStyle(element).color"
            )
            background = await placeholder.evaluate(
                "element => getComputedStyle(element).backgroundColor"
            )
            light, dark = sorted([luminance(ink), luminance(background)], reverse=True)
            assert (light + 0.05) / (dark + 0.05) >= 4.5
            await page.get_by_role(
                "button", name="View Example canvas coat", exact=True
            ).click()
            dialog = page.get_by_role("dialog", name="Example canvas coat")
            main = dialog.locator(".gallery-main .photo")
            for number, is_crop in [(1, False), (2, True)]:
                thumbnail = dialog.get_by_role(
                    "button", name=f"Show photo {number}", exact=True
                )
                await thumbnail.click()
                await assert_white_canvas(main, cropped=is_crop)
                # Hovering the control must not dim its white canvas or pixels.
                await thumbnail.hover()
                await assert_white_canvas(thumbnail.locator(".photo"), cropped=is_crop)
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()
