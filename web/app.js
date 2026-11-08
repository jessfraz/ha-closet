"use strict";

// The token belongs to this tab only. The JSON file, not this UI, owns the catalog.
const byId = (id) => document.getElementById(id);
let token = null;
let system = null;
let view = "inventory";
let nextPage = null;
let listAbort = null;
let listRevision = 0;
let listLoading = false;
let pageObserver = null;
let autoPagingPaused = false;
let listAutomatic = false;
let todayRevision = 0;
let pollTimer = null;
let pollDeadline = 0;
let todayDraft = false;
let lastToday = null;
let pendingGeneration = null;
const items = new Map();
const sourceLabels = new Map();
const filterFields = [["q", "search"], ["brand", "brand"], ["category", "category"], ["source", "source"]];
let appliedFilters = { q: "", brand: "", category: "", source: "" };
let selectedItemId = null;
let detailAbort = null;
let detailRevision = 0;
let detailClosePending = false;
let locationInitialized = false;
let navigationListening = false;
const pendingLocations = [];
// HA owns the outer route. Its documented navigate message updates that route
// without exposing the ingress session path or writing its history directly.
const navigationHost = (() => {
  if (window.parent === window || !/^\/api\/hassio_ingress\/[^/]+\//.test(window.location.pathname)) return window;
  try { if (window.parent.location.origin === window.location.origin) return window.parent; }
  catch { /* A cross-origin embedding can still use its own document history. */ }
  return window;
})();

class APIError extends Error {
  constructor(status, code) {
    super(code);
    this.status = status;
    this.code = code;
  }
}

function notice(message = "") {
  byId("notice").textContent = message;
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function object(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function navigationURL() {
  return new URL(navigationHost.location.href);
}

function restoreFilters(filters) {
  for (const [key, id] of filterFields) {
    const control = byId(id);
    if (id !== "search" && filters[key] && ![...control.options].some((option) => option.value === filters[key])) {
      control.add(new Option(sourceLabels.get(filters[key]) || filters[key], filters[key]));
    }
    control.value = filters[key];
  }
}

function applyLocation(url = navigationURL(), { refresh = false } = {}) {
  const filters = Object.fromEntries(filterFields.map(([key]) => [key, url.searchParams.get(`closet_${key}`) || ""]));
  const itemId = url.searchParams.get("closet_item") || null;
  const filtersChanged = filterFields.some(([key]) => filters[key] !== appliedFilters[key]);
  const first = !locationInitialized;
  if (first || filtersChanged) {
    appliedFilters = filters;
    restoreFilters(filters);
  }
  locationInitialized = true;
  detailClosePending = false;
  const itemChanged = itemId !== selectedItemId;
  selectedItemId = itemId;
  if (!system) return;
  if (filtersChanged && view !== "inventory") selectView("inventory");
  if (first || filtersChanged || refresh) void loadInventory();
  if (first || itemChanged || refresh) void loadDetail(itemId);
}

function navigate(filters, itemId, { replace = false } = {}) {
  const url = navigationURL();
  for (const [key] of filterFields) {
    url.searchParams.delete(`closet_${key}`);
    if (filters[key]) url.searchParams.set(`closet_${key}`, filters[key]);
  }
  url.searchParams.delete("closet_item");
  if (itemId) url.searchParams.set("closet_item", itemId);
  const queued = navigationHost !== window && pendingLocations.length > 0;
  if (url.href !== navigationURL().href || queued) {
    if (navigationHost === window) {
      const previous = object(history.state) ? history.state : {};
      history[replace ? "replaceState" : "pushState"]({ ...previous, closetItemEntry: Boolean(itemId) && !replace }, "", url);
    } else if (pendingLocations.at(-1) !== url.href) {
      pendingLocations.push(url.href);
      navigationHost.postMessage({ type: "home-assistant/navigate", path: url.pathname + url.search + url.hash, options: { replace } }, url.origin);
    }
  }
  applyLocation(url);
}

function applyFilters() {
  const filters = Object.fromEntries(filterFields.map(([key, id]) => [key, key === "q" ? byId(id).value.trim() : byId(id).value]));
  const unchanged = filterFields.every(([key]) => filters[key] === appliedFilters[key]);
  navigate(filters, null);
  if (unchanged) void loadInventory();
}

function closeDetail() {
  if (!selectedItemId || detailClosePending) return;
  detailClosePending = true;
  if (navigationHost === window && history.state?.closetItemEntry === true) history.back();
  else navigate(appliedFilters, null, { replace: true });
}

function locationChanged(event) {
  const url = navigationURL();
  if (event.type === "popstate") pendingLocations.length = 0;
  else if (pendingLocations.length) {
    // HA processes these messages in order. An earlier identical destination
    // cannot acknowledge the latest request or admit an intermediate close.
    if (pendingLocations[0] !== url.href) return;
    pendingLocations.shift();
    if (pendingLocations.length) return;
  }
  if (system) applyLocation(url);
}

function startNavigation() {
  if (navigationListening) return;
  navigationListening = true;
  navigationHost.addEventListener("popstate", locationChanged);
  if (navigationHost !== window) navigationHost.addEventListener("location-changed", locationChanged);
}

function stopNavigation() {
  navigationListening = false;
  pendingLocations.length = 0;
  navigationHost.removeEventListener("popstate", locationChanged);
  if (navigationHost !== window) navigationHost.removeEventListener("location-changed", locationChanged);
}

async function request(path, options = {}) {
  const headers = new Headers(options.headers);
  headers.set("X-Closet-Request", "1");
  headers.set("Accept", "application/json");
  if (token) headers.set("Authorization", `Bearer ${token}`);
  if (options.body !== undefined) headers.set("Content-Type", "application/json");
  let response;
  let text;
  try {
    response = await fetch(new URL(path, document.baseURI), {
      ...options,
      headers,
      redirect: "error",
      credentials: "same-origin",
    });
    text = await response.text();
  } catch (error) {
    if (error.name !== "AbortError" && system) byId("service-status").textContent = "Connection lost";
    throw error;
  }
  if (system) byId("service-status").textContent = response.status === 401 ? "Authorization expired" : "Connected";
  if (text.length > 8 * 1024 * 1024) throw new APIError(response.status, "response_too_large");
  let data = null;
  if (text) {
    try { data = JSON.parse(text); }
    catch { throw new APIError(response.status, "invalid_response"); }
  }
  if (!response.ok) {
    throw new APIError(response.status, object(data) && typeof data.code === "string" ? data.code : "request_failed");
  }
  return data;
}

function errorMessage(error) {
  if (error instanceof APIError) {
    if (error.status === 401) return "The connection is no longer authorized. Reload Closet to reconnect.";
    if (error.status === 403) return "This action needs owner or Home Assistant administrator access.";
    if (error.code === "page_invalidated") return "The inventory changed while paging. Refresh to start from the current catalog.";
    if (error.status === 409) return "Another request is in progress or the inventory changed. Read the current state before trying again.";
    if (error.status === 503) return "Closet or one of its configured services is unavailable.";
  }
  return "The request did not complete. Refresh to read the current state.";
}

function safeLink(value) {
  if (typeof value !== "string") return null;
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || url.username || url.password) return null;
    return url.href;
  } catch { return null; }
}

function photo(item, index = 0) {
  const wrapper = element("div", "photo");
  const reference = Array.isArray(item.images) ? item.images[index] : null;
  const url = object(reference) ? safeLink(reference.url) : null;
  const placeholder = () => wrapper.replaceChildren(element("span", "photo-placeholder", "No photo"));
  if (!url) { placeholder(); return wrapper; }
  const image = document.createElement("img");
  image.alt = item.name || "Item";
  image.referrerPolicy = "no-referrer";
  image.loading = "lazy";
  image.addEventListener("error", placeholder, { once: true });
  const crop = reference.crop;
  if (object(crop)) {
    image.addEventListener("load", () => {
      const units = 10000;
      const viewport = element("div", "photo-crop");
      viewport.style.setProperty("--crop-aspect", String(image.naturalWidth * crop.width / (image.naturalHeight * crop.height)));
      image.style.width = `${100 * units / crop.width}%`;
      image.style.height = `${100 * units / crop.height}%`;
      image.style.left = `${-100 * crop.x / crop.width}%`;
      image.style.top = `${-100 * crop.y / crop.height}%`;
      viewport.append(image);
      wrapper.replaceChildren(viewport);
    }, { once: true });
  }
  image.src = url;
  wrapper.append(image);
  return wrapper;
}

function sourceOf(item) {
  if (object(item.shop) && typeof item.shop.id === "string") return `shop:${item.shop.id}`;
  if (typeof item.purchase_source === "string") return `purchase:${item.purchase_source}`;
  return null;
}

function sourceLabel(item) {
  const source = sourceOf(item);
  if (source === null) return null;
  if (sourceLabels.has(source)) return sourceLabels.get(source);
  if (object(item.shop) && item.shop.label) return item.shop.label;
  if (item.purchase_source === "self_bought") return "Self-bought";
  return item.purchase_source || null;
}

function filterFromBadge(field, value, label) {
  selectView("inventory");
  for (const id of ["search", "brand", "category", "source"]) byId(id).value = "";
  const select = byId(field);
  if (![...select.options].some((option) => option.value === value)) select.add(new Option(label, value));
  select.value = value;
  applyFilters();
}

function sourceTone(source) {
  let hash = 0;
  for (const character of source) hash = (Math.imul(hash, 31) + character.codePointAt(0)) >>> 0;
  return hash % 6;
}

function itemCard(item, compact = false) {
  const card = element("article", compact ? "item-piece" : "item-card");
  const badge = (field, value, label, accessibleLabel) => {
    const button = element("button", `item-badge ${field}-badge`, label);
    button.type = "button";
    button.setAttribute("aria-label", accessibleLabel);
    button.title = accessibleLabel;
    if (field === "source") button.classList.add(`source-tone-${sourceTone(value)}`);
    button.addEventListener("click", () => filterFromBadge(field, value, label));
    return button;
  };
  card.append(photo(item));
  if (item.brand) card.append(badge("brand", item.brand, item.brand, `Show all items by ${item.brand}`));
  const button = element("button", "item-open item-name", item.name);
  button.type = "button";
  button.setAttribute("aria-label", `View ${item.name}`);
  const metadata = [item.size, item.color].filter((value) => value !== null && value !== undefined && value !== "");
  const reference = Array.isArray(item.images) ? item.images[0] : null;
  const description = [item.brand];
  if (!compact) description.push(...metadata);
  if (object(reference) && reference.notes) description.push(reference.notes);
  button.setAttribute("aria-description", description.filter(Boolean).join(" · "));
  button.addEventListener("click", () => openDetail(item));
  card.append(button);
  if (!compact && metadata.length) card.append(element("span", "item-meta", metadata.join(" · ")));
  if (object(reference) && reference.notes) card.append(element("span", "image-note", reference.notes));
  const source = sourceOf(item);
  const label = sourceLabel(item);
  if (source !== null && label !== null) {
    sourceLabels.set(source, label);
    const badges = element("div", "item-badges");
    badges.append(badge("source", source, label, `Show all items from ${label}`));
    card.append(badges);
  }
  return card;
}

function fillOptions(select, values, allLabel) {
  const selected = select.value;
  const options = [new Option(allLabel, "")];
  for (const value of values) if (typeof value === "string") options.push(new Option(value, value));
  if (selected && !values.includes(selected)) options.push(new Option(selected, selected));
  select.replaceChildren(...options);
  select.value = selected;
}

function fillSources(facets) {
  const select = byId("source");
  let selected = select.value;
  const options = [];
  for (const facet of facets) {
    if (!object(facet) || typeof facet.source !== "string" || typeof facet.label !== "string" || !Number.isInteger(facet.count) || facet.count < 0) throw new APIError(200, "invalid_response");
    sourceLabels.set(facet.source, facet.label);
    options.push(new Option(`${facet.label} (${facet.count})`, facet.source));
  }
  if (selected && !facets.some((facet) => facet.source === selected)) {
    const label = sourceLabels.get(selected) || selected;
    options.push(new Option(`${label} (0)`, selected));
  }
  options.sort((left, right) => left.label.localeCompare(right.label));
  select.replaceChildren(new Option("All sources", ""), ...options);
  select.value = selected;
}

function stopAutoPaging() {
  pageObserver?.disconnect();
  pageObserver = null;
}

function observeNextPage() {
  stopAutoPaging();
  if (view !== "inventory" || !system || document.hidden || listLoading || autoPagingPaused || !nextPage || typeof IntersectionObserver !== "function") return;
  const revision = listRevision;
  const observer = new IntersectionObserver((entries) => {
    if (pageObserver !== observer || revision !== listRevision || !entries.some((entry) => entry.isIntersecting)) return;
    stopAutoPaging();
    void loadInventory(true, { automatic: true });
  }, { rootMargin: "240px 0px" });
  pageObserver = observer;
  observer.observe(byId("page-sentinel"));
}

async function loadInventory(append = false, { automatic = false } = {}) {
  if (append && (!nextPage || listLoading)) return;
  const more = byId("load-more");
  const moveFocus = !automatic && append && document.activeElement === more;
  stopAutoPaging();
  listAbort?.abort();
  const controller = new AbortController();
  listAbort = controller;
  const revision = ++listRevision;
  listLoading = true;
  listAutomatic = automatic;
  if (!append) autoPagingPaused = false;
  const query = new URLSearchParams({ limit: "50" });
  if (append) {
    // The cursor carries the applied filters, not any unsubmitted search text.
    query.set("page_token", nextPage);
  } else {
    for (const [key] of filterFields) {
      const value = appliedFilters[key];
      if (value) query.set(key, value);
    }
  }
  if (!append) { nextPage = null; more.hidden = true; byId("page-sentinel").hidden = true; }
  more.setAttribute("aria-disabled", "true");
  more.setAttribute("aria-busy", "true");
  more.textContent = "Loading more…";
  try {
    const data = await request(`v1/items?${query}`, { signal: controller.signal });
    if (revision !== listRevision) return;
    if (!object(data) || !Array.isArray(data.items) || !object(data.facets) || !Array.isArray(data.facets.sources) || !Number.isInteger(data.total)) throw new APIError(200, "invalid_response");
    fillSources(data.facets.sources);
    if (!append) byId("item-grid").replaceChildren();
    let firstAddedButton = null;
    for (const item of data.items) {
      if (!object(item) || typeof item.id !== "string" || typeof item.name !== "string") throw new APIError(200, "invalid_response");
      items.set(item.id, item);
      const card = itemCard(item);
      firstAddedButton ||= card.querySelector(".item-open");
      byId("item-grid").append(card);
    }
    nextPage = typeof data.next_page === "string" ? data.next_page : null;
    autoPagingPaused = false;
    more.hidden = nextPage === null;
    byId("page-sentinel").hidden = nextPage === null;
    if (moveFocus && nextPage === null && document.activeElement === document.body) firstAddedButton?.focus();
    byId("inventory-empty").hidden = data.total !== 0;
    byId("inventory-empty").querySelector("h2").textContent = data.scope_total === 0 ? "No items yet" : "No matching items";
    byId("inventory-empty").querySelector("p").textContent = data.scope_total === 0 ? "Validate and sync a JSON catalog to fill the closet." : "Try another search or clear the filters.";
    byId("inventory-count").textContent = `${data.total} ${data.total === 1 ? "item" : "items"}${data.total !== data.scope_total ? ` · ${data.scope_total} in inventory` : ""}`;
    fillOptions(byId("brand"), data.facets.brands || [], "All brands");
    fillOptions(byId("category"), data.facets.categories || [], "All categories");
    notice();
  } catch (error) {
    if (error.name !== "AbortError" && revision === listRevision) {
      const previous = byId("item-grid").childElementCount ? " The previous inventory result is still shown." : "";
      autoPagingPaused = true;
      notice(errorMessage(error) + previous + (automatic ? " Use Load more to try again." : ""));
    }
  } finally {
    if (revision === listRevision) {
      listLoading = false;
      listAutomatic = false;
      more.removeAttribute("aria-disabled");
      more.removeAttribute("aria-busy");
      more.textContent = "Load more";
      if (moveFocus && nextPage !== null && document.activeElement === document.body) more.focus();
      observeNextPage();
    }
  }
}

function openDetail(item) {
  navigate(appliedFilters, item.id);
}

async function loadDetail(itemId) {
  detailAbort?.abort();
  const revision = ++detailRevision;
  const dialog = byId("detail");
  if (!itemId) { if (dialog.open) dialog.close(); return; }
  const controller = new AbortController();
  detailAbort = controller;
  byId("detail-brand").textContent = "";
  byId("detail-title").textContent = "Loading item…";
  if (dialog.open && byId("detail-content").contains(document.activeElement)) byId("close-detail").focus();
  byId("detail-content").replaceChildren(element("p", "muted", "Loading item…"));
  if (!dialog.open) dialog.showModal();
  try {
    const item = await request(`v1/items/${encodeURIComponent(itemId)}`, { signal: controller.signal });
    if (revision !== detailRevision || selectedItemId !== itemId) return;
    if (!object(item) || item.id !== itemId) throw new APIError(200, "invalid_response");
    if (item.archived) throw new APIError(404, "item_not_found");
    items.set(itemId, item);
    renderDetail(item);
  } catch (error) {
    if (error.name === "AbortError" || revision !== detailRevision || selectedItemId !== itemId) return;
    byId("detail-title").textContent = error instanceof APIError && error.status === 404 ? "Item unavailable" : "Unable to load item";
    byId("detail-content").replaceChildren(element("p", "muted", error instanceof APIError && error.status === 404 ? "This item is not in the active inventory." : errorMessage(error)));
  }
}

function gallery(item) {
  const panel = element("div", "detail-gallery");
  const main = element("div", "gallery-main");
  const information = element("div");
  const references = Array.isArray(item.images) ? item.images : [];
  const thumbnails = [];
  const counter = element("span", "gallery-counter");
  let selected = 0;
  const select = (index, focus = false) => {
    selected = (index + references.length) % references.length || 0;
    const picture = photo(item, selected);
    main.replaceChildren(picture);
    information.replaceChildren();
    const reference = references[selected];
    if (object(reference)) {
      if (reference.notes) information.append(element("p", "image-note", reference.notes));
      const source = safeLink(reference.source_url || reference.url);
      if (source) {
        const link = element("a", "reference-link", reference.source_url ? "View product ↗" : "Open photo ↗");
        link.href = source; link.target = "_blank"; link.rel = "noopener noreferrer";
        information.append(link);
        if (!reference.source_url) picture.querySelector("img")?.addEventListener("error", () => link.remove(), { once: true });
      }
    }
    counter.textContent = `${selected + 1} of ${references.length}`;
    thumbnails.forEach((button, index) => {
      button.setAttribute("aria-pressed", String(index === selected));
      button.tabIndex = index === selected ? 0 : -1;
    });
    if (focus) thumbnails[selected]?.focus();
  };
  panel.append(main);
  if (references.length > 1) {
    const controls = element("div", "gallery-controls");
    const previous = element("button", "quiet", "←");
    previous.type = "button"; previous.setAttribute("aria-label", "Previous photo");
    previous.addEventListener("click", () => select(selected - 1));
    const next = element("button", "quiet", "→");
    next.type = "button"; next.setAttribute("aria-label", "Next photo");
    next.addEventListener("click", () => select(selected + 1));
    counter.setAttribute("role", "status");
    counter.setAttribute("aria-live", "polite");
    controls.append(previous, counter, next);
    const strip = element("div", "gallery-thumbnails");
    strip.setAttribute("role", "group"); strip.setAttribute("aria-label", "Item photos");
    references.forEach((reference, index) => {
      const button = element("button", "photo-thumbnail");
      button.type = "button"; button.setAttribute("aria-label", `Show photo ${index + 1}`);
      const preview = photo(item, index);
      const image = preview.querySelector("img");
      if (image) image.alt = "";
      button.append(preview);
      button.addEventListener("click", () => select(index));
      thumbnails.push(button); strip.append(button);
    });
    strip.addEventListener("keydown", (event) => {
      const index = { ArrowLeft: selected - 1, ArrowRight: selected + 1, Home: 0, End: references.length - 1 }[event.key];
      if (index === undefined) return;
      event.preventDefault(); select(index, true);
    });
    panel.append(controls, strip);
  }
  panel.append(information);
  select(0);
  return panel;
}

function renderDetail(item) {
  byId("detail-brand").textContent = item.brand || "";
  byId("detail-title").textContent = item.name;
  const layout = element("div", "detail-layout");
  const imagePanel = gallery(item);
  const values = [
    ["Category", item.category], ["Color", item.color], ["Size", item.size],
    ["Quantity", item.quantity], ["Material", item.material], ["SKU", item.sku],
  ];
  if (item.purchase_source) values.push(["Purchase source", item.purchase_source]);
  if (object(item.shop)) {
    values.push(["Shop", item.shop.label]);
    if (item.shop.season) values.push(["Season", item.shop.season]);
    if (Array.isArray(item.shop.years) && item.shop.years.length) values.push(["Years", item.shop.years.join(", ")]);
  }
  if (object(item.price) && typeof item.price.amount === "number" && typeof item.price.currency === "string") {
    let price = `${item.price.amount} ${item.price.currency}`;
    try { price = new Intl.NumberFormat(undefined, { style: "currency", currency: item.price.currency }).format(item.price.amount); }
    catch { /* Keep the recorded amount/currency if this browser cannot format it. */ }
    values.push(["Recorded price", price]);
    if (item.price.basis) values.push(["Price basis", item.price.basis]);
    if (item.price.quantity_basis) values.push(["Quantity basis", item.price.quantity_basis]);
  }
  const definition = element("dl");
  for (const [label, value] of values) {
    if (value !== null && value !== undefined && value !== "") definition.append(element("dt", "", label), element("dd", "", value));
  }
  const copy = element("div"); copy.append(definition);
  if (Array.isArray(item.notes) && item.notes.length) {
    const notes = element("div", "detail-notes");
    notes.append(element("h3", "", "Notes"));
    for (const note of item.notes) notes.append(element("p", "", note));
    copy.append(notes);
  }
  layout.append(imagePanel, copy);
  byId("detail-content").replaceChildren(layout);
}

function inputValues() {
  return { occasion: byId("occasion").value, weather_mode: byId("weather-mode").value };
}

const occasionLabels = { all: "All occasions", everyday: "Everyday", work: "Work", dinner: "Evening", bedtime: "Bedtime" };

function inputLabel(inputs) {
  const occasion = occasionLabels[inputs.occasion] || "Unknown occasion";
  return `${occasion} · ${inputs.weather_mode === "ignored" ? "Weather ignored" : "Uses forecast"}`;
}

function generationMessage(generation) {
  if (!generation) return "";
  if (generation.status === "running") return "Choosing from the inventory… The previous result stays below until this finishes.";
  if (generation.status === "succeeded") return "";
  const messages = {
    today_unconfigured: "Today isn't configured. Inventory still works.",
    weather_unavailable: "Weather is unavailable. Choose Ignore forecast to explicitly request a non-weather suggestion.",
    model_unavailable: "The configured outfit assistant is unavailable. Inventory still works.",
    timeout: "The request timed out. Upstream processing may still finish; it will not be retried automatically.",
    generation_timeout: "The request timed out. Upstream processing may still finish; it will not be retried automatically.",
    interrupted: "The request was interrupted by a restart. No automatic retry was made.",
    catalog_changed: "Referenced inventory changed while the suggestion was being made. Refresh before requesting another.",
    generation_context_changed: "The day, configuration or inventory changed during the request. No result was published as current.",
    insufficient_inventory: "There are not enough eligible pieces for a complete outfit.",
    invalid_model_output: "The assistant returned a combination that did not pass validation.",
  };
  return messages[generation.error_code] || "The last attempt did not complete. A new request is a deliberate new attempt.";
}

async function getItem(id) {
  if (items.has(id)) return items.get(id);
  const item = await request(`v1/items/${encodeURIComponent(id)}`);
  if (!object(item) || item.id !== id) throw new APIError(200, "invalid_response");
  items.set(id, item);
  return item;
}

function renderWeather(result) {
  const holder = byId("weather"); holder.replaceChildren();
  if (!result) return;
  if (result.inputs.weather_mode === "ignored") { holder.textContent = "Forecast not used for this result."; return; }
  const weather = result.weather;
  if (!object(weather)) return;
  const fields = [];
  const entries = Array.isArray(weather.entries) ? weather.entries : [];
  const condition = entries.find((entry) => object(entry) && typeof entry.condition === "string")?.condition;
  if (condition) fields.push(condition.replaceAll("-", " "));
  const temperatures = entries.flatMap((entry) => object(entry) ? [entry.temperature, entry.templow].filter((value) => typeof value === "number" && Number.isFinite(value)) : []);
  const unit = weather.temperature_unit || "";
  if (temperatures.length) fields.push(`${Math.min(...temperatures)}–${Math.max(...temperatures)} ${unit}`);
  holder.append(element("p", "", fields.join(" · ") || "Forecast context supplied"));
  const metadata = [];
  if (weather.coverage === "partial") metadata.push("Partial forecast coverage");
  if (weather.coverage === "complete") metadata.push("Full forecast coverage");
  if (weather.forecast_type) metadata.push(`${weather.forecast_type} forecast samples`);
  const time = (value) => new Date(value).toLocaleTimeString([], { timeZone: lastToday?.timezone, hour: "2-digit", minute: "2-digit" });
  if (weather.window_start && weather.window_end) metadata.push(`${time(weather.window_start)}–${time(weather.window_end)} window`);
  if (weather.retrieved_at) {
    const checked = new Date(weather.retrieved_at);
    if (!Number.isNaN(checked.getTime())) metadata.push(`Last checked ${time(weather.retrieved_at)}`);
  }
  if (metadata.length) holder.append(element("small", "", metadata.join(" · ")));
}

async function renderToday(data, revision) {
  if (!object(data) || typeof data.date !== "string" || typeof data.timezone !== "string") throw new APIError(200, "invalid_response");
  lastToday = data;
  byId("today-date").textContent = `${data.date} · ${data.timezone}`;
  const result = object(data.result) ? data.result : null;
  const generation = object(data.generation) ? data.generation : null;
  if (!todayDraft && !pendingGeneration) {
    const inputs = result?.inputs || generation?.inputs;
    if (object(inputs)) {
      byId("weather-mode").value = inputs.weather_mode;
    }
  }
  const running = generation?.status === "running";
  for (const id of ["occasion", "weather-mode"]) byId(id).disabled = running || pendingGeneration !== null;
  byId("suggest").disabled = running || system?.role !== "owner";
  byId("suggest").textContent = pendingGeneration ? "Retry same request" : result ? "Suggest again" : "Suggest outfits";
  const message = pendingGeneration ? "The last request was not acknowledged. Retry the same request to recover it before changing the controls." : system?.role !== "owner" ? "Owner access is needed to request suggestions. Reading existing results is available." : generationMessage(generation);
  byId("generation-state").textContent = message;
  byId("generation-state").hidden = !message;
  byId("today-empty").hidden = result !== null || running;
  byId("today-result-label").textContent = result ? `${inputLabel(result.inputs)}${result.freshness === "stale" ? " · Stale result" : ""}` : "";
  byId("today-input-note").hidden = !result || JSON.stringify(inputValues()) === JSON.stringify(result.inputs);
  renderWeather(result);
  const cards = document.createDocumentFragment();
  if (result && Array.isArray(result.suggestions)) {
    const groups = result.inputs.occasion === "all"
      ? ["everyday", "work", "dinner", "bedtime"]
      : [result.inputs.occasion];
    for (const occasion of groups) {
      const suggestions = result.suggestions.filter((suggestion) => object(suggestion) && (suggestion.occasion || result.inputs.occasion) === occasion).slice(0, 3);
      if (!suggestions.length) continue;
      const section = element("section", "today-group");
      const heading = element("h2", "today-group-heading", occasionLabels[occasion] || "Suggestions");
      heading.id = `today-group-${occasion}`;
      section.setAttribute("aria-labelledby", heading.id);
      section.append(heading);
      for (const suggestion of suggestions) {
        if (!object(suggestion) || !Array.isArray(suggestion.item_ids)) throw new APIError(200, "invalid_response");
        const card = element("article", `suggestion${result.freshness === "stale" ? " stale" : ""}`);
        const pieces = element("div", "pieces");
        const records = await Promise.all([...new Set(suggestion.item_ids)].slice(0, 5).map(getItem));
        if (revision !== todayRevision || view !== "today") return;
        for (const item of records) pieces.append(itemCard(item, true));
        const copy = element("div", "suggestion-copy");
        copy.append(element("h3", "", suggestion.label || "Another combination"), element("p", "muted", suggestion.advisory_reason || ""));
        const warnings = Array.isArray(suggestion.warnings) ? suggestion.warnings.filter((warning) => warning !== "unknown_quantity" && warning !== "qualified_image_reference") : [];
        if (warnings.length) copy.append(element("p", "qualifier", warnings.join(" · ").replaceAll("_", " ")));
        card.append(pieces, copy); section.append(card);
      }
      cards.append(section);
    }
  }
  if (revision !== todayRevision || view !== "today") return;
  byId("suggestions").replaceChildren(cards);
  clearTimeout(pollTimer);
  if (running && view === "today" && !document.hidden) {
    if (!pollDeadline) pollDeadline = Date.now() + 90000;
    if (Date.now() < pollDeadline) pollTimer = setTimeout(() => loadToday(), 1800);
    else { byId("generation-state").textContent = "The request is still running. Refresh to read its current status; no new generation will be started."; byId("generation-state").hidden = false; }
  } else pollDeadline = 0;
}

async function loadToday() {
  const revision = ++todayRevision;
  try {
    const data = await request("v1/today");
    if (revision === todayRevision && view === "today") await renderToday(data, revision);
  } catch (error) {
    if (revision === todayRevision && view === "today") { notice(errorMessage(error)); clearTimeout(pollTimer); }
  }
}

async function generate(event) {
  event.preventDefault();
  if (!pendingGeneration) {
    const random = crypto.getRandomValues(new Uint8Array(16));
    const key = Array.from(random, (value) => value.toString(16).padStart(2, "0")).join("");
    pendingGeneration = { key, inputs: inputValues() };
  }
  byId("suggest").disabled = true;
  for (const id of ["occasion", "weather-mode"]) byId(id).disabled = true;
  try {
    await request("v1/today/generate", { method: "POST", headers: { "Idempotency-Key": pendingGeneration.key }, body: JSON.stringify(pendingGeneration.inputs) });
    pendingGeneration = null; todayDraft = false; notice(); pollDeadline = 0;
    await loadToday();
  } catch (error) {
    // A gateway/timeout failure may arrive after Rust has reserved the attempt.
    // Only a definitive client rejection permits a new key and edited inputs.
    if (error instanceof APIError && error.status >= 400 && error.status < 500 && error.status !== 408) pendingGeneration = null;
    notice(pendingGeneration ? "The request was not acknowledged. Retry the same request to recover its result without starting another attempt." : errorMessage(error));
    byId("suggest").disabled = false;
    byId("suggest").textContent = pendingGeneration ? "Retry same request" : "Suggest outfits";
    if (!pendingGeneration) for (const id of ["occasion", "weather-mode"]) byId(id).disabled = false;
  }
}

async function connect() {
  const data = await request("v1/system");
  if (!object(data) || typeof data.version !== "string" || !["owner", "integration"].includes(data.role)) throw new APIError(200, "invalid_response");
  system = data;
  byId("login").hidden = true;
  byId("token").value = "";
  byId("app").hidden = false;
  byId("refresh").hidden = false;
  byId("app-footer").hidden = false;
  byId("app-version").textContent = data.version;
  byId("service-status").textContent = "Connected";
  applyLocation(navigationURL(), { refresh: true });
}

function selectView(next) {
  todayRevision += 1;
  stopAutoPaging();
  view = next;
  if (view !== "inventory" && listAutomatic) listAbort?.abort();
  byId("inventory").hidden = view !== "inventory";
  byId("today").hidden = view !== "today";
  byId("inventory-tab").setAttribute("aria-pressed", String(view === "inventory"));
  byId("today-tab").setAttribute("aria-pressed", String(view === "today"));
  clearTimeout(pollTimer);
  notice();
  if (view === "today") { pollDeadline = 0; void loadToday(); }
  else observeNextPage();
}

byId("login").addEventListener("submit", async (event) => {
  event.preventDefault(); token = byId("token").value;
  try { await connect(); notice(); }
  catch (error) { token = null; notice(errorMessage(error)); }
});
byId("refresh").addEventListener("click", async () => {
  items.clear();
  if (view === "today") { pollDeadline = 0; await loadToday(); }
  else await loadInventory();
  if (selectedItemId) void loadDetail(selectedItemId);
});
byId("filters").addEventListener("submit", (event) => { event.preventDefault(); applyFilters(); });
byId("brand").addEventListener("change", applyFilters);
byId("category").addEventListener("change", applyFilters);
byId("source").addEventListener("change", applyFilters);
byId("load-more").addEventListener("click", () => void loadInventory(true));
byId("inventory-tab").addEventListener("click", () => selectView("inventory"));
byId("today-tab").addEventListener("click", () => selectView("today"));
byId("close-detail").addEventListener("click", closeDetail);
byId("detail").addEventListener("cancel", (event) => { event.preventDefault(); closeDetail(); });
byId("generate").addEventListener("submit", generate);
byId("generate").addEventListener("change", () => { todayDraft = true; byId("today-input-note").hidden = !lastToday?.result || JSON.stringify(inputValues()) === JSON.stringify(lastToday.result.inputs); });
document.addEventListener("visibilitychange", () => {
  clearTimeout(pollTimer);
  stopAutoPaging();
  if (document.hidden && listAutomatic) listAbort?.abort();
  if (!document.hidden && view === "today" && system) void loadToday();
  if (!document.hidden && view === "inventory") observeNextPage();
});
window.addEventListener("pagehide", () => { clearTimeout(pollTimer); stopAutoPaging(); listAbort?.abort(); detailAbort?.abort(); detailRevision += 1; stopNavigation(); });
window.addEventListener("pageshow", () => { startNavigation(); if (system) applyLocation(navigationURL(), { refresh: true }); observeNextPage(); });
startNavigation();
connect().catch((error) => { if (error instanceof APIError && error.status === 401) byId("login").hidden = false; else notice(errorMessage(error)); });
