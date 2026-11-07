"use strict";

// The token belongs to this tab only. The JSON file, not this UI, owns the catalog.
const byId = (id) => document.getElementById(id);
let token = null;
let system = null;
let view = "inventory";
let nextPage = null;
let listAbort = null;
let listRevision = 0;
let todayRevision = 0;
let pollTimer = null;
let pollDeadline = 0;
let todayDraft = false;
let lastToday = null;
let pendingGeneration = null;
const items = new Map();

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

function photo(item) {
  const wrapper = element("div", "photo");
  const reference = Array.isArray(item.images) ? item.images[0] : null;
  const url = object(reference) ? safeLink(reference.url) : null;
  const placeholder = () => wrapper.replaceChildren(element("span", "photo-placeholder", "No photo"));
  if (!url) { placeholder(); return wrapper; }
  const image = document.createElement("img");
  image.alt = `${item.name || "Item"} · product reference`;
  image.referrerPolicy = "no-referrer";
  image.loading = "lazy";
  image.addEventListener("error", placeholder, { once: true });
  image.src = url;
  wrapper.append(image);
  return wrapper;
}

function qualifiers(item) {
  const values = [];
  if (item.current_ownership_confirmed !== true) values.push("Ownership unconfirmed");
  if (item.quantity === null) values.push("Quantity unknown");
  if (Array.isArray(item.images) && item.images.length) values.push("Reference photo");
  return values;
}

function itemButton(item, compact = false) {
  const button = element("button", "item-open");
  button.type = "button";
  button.setAttribute("aria-label", `View ${item.name}`);
  button.append(photo(item));
  if (item.brand) button.append(element("span", "item-brand", item.brand));
  button.append(element("span", "item-name", item.name));
  if (!compact) button.append(element("span", "item-meta", `${item.size || "Size unknown"} · ${item.color || "Color unknown"}`));
  const labels = qualifiers(item);
  if (labels.length) button.append(element("span", "qualifier", labels.join(" · ")));
  const reference = Array.isArray(item.images) ? item.images[0] : null;
  if (object(reference) && reference.notes) button.append(element("span", "image-note", reference.notes));
  const description = [item.brand];
  if (!compact) description.push(item.size || "Size unknown", item.color || "Color unknown");
  description.push(...labels);
  if (object(reference) && reference.notes) description.push(reference.notes);
  button.setAttribute("aria-description", description.filter(Boolean).join(" · "));
  button.addEventListener("click", () => openDetail(item));
  return button;
}

function fillOptions(select, values, allLabel) {
  const selected = select.value;
  const options = [new Option(allLabel, "")];
  for (const value of values) if (typeof value === "string") options.push(new Option(value, value));
  select.replaceChildren(...options);
  if (values.includes(selected)) select.value = selected;
}

async function loadInventory(append = false) {
  const moveFocus = append && document.activeElement === byId("load-more");
  listAbort?.abort();
  const controller = new AbortController();
  listAbort = controller;
  const revision = ++listRevision;
  const query = new URLSearchParams({ limit: "50" });
  for (const [key, id] of [["q", "search"], ["brand", "brand"], ["category", "category"]]) {
    const value = byId(id).value.trim();
    if (value) query.set(key, value);
  }
  if (append && nextPage) query.set("page_token", nextPage);
  byId("load-more").disabled = true;
  try {
    const data = await request(`v1/items?${query}`, { signal: controller.signal });
    if (revision !== listRevision) return;
    if (!object(data) || !Array.isArray(data.items) || !object(data.facets) || !Number.isInteger(data.total)) throw new APIError(200, "invalid_response");
    if (!append) byId("item-grid").replaceChildren();
    let firstAddedButton = null;
    for (const item of data.items) {
      if (!object(item) || typeof item.id !== "string" || typeof item.name !== "string") throw new APIError(200, "invalid_response");
      items.set(item.id, item);
      const card = element("article", "item-card");
      const button = itemButton(item);
      firstAddedButton ||= button;
      card.append(button);
      byId("item-grid").append(card);
    }
    nextPage = typeof data.next_page === "string" ? data.next_page : null;
    byId("load-more").hidden = nextPage === null;
    if (moveFocus && nextPage === null) firstAddedButton?.focus();
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
      notice(errorMessage(error) + previous);
    }
  } finally {
    if (revision === listRevision) byId("load-more").disabled = false;
  }
}

function openDetail(item) {
  byId("detail-brand").textContent = item.brand || "";
  byId("detail-title").textContent = item.name;
  const layout = element("div", "detail-layout");
  const imagePanel = element("div");
  imagePanel.append(photo(item));
  const reference = Array.isArray(item.images) ? item.images[0] : null;
  if (object(reference)) {
    if (reference.notes) imagePanel.append(element("p", "image-note", reference.notes));
    const source = safeLink(reference.source_url || reference.url);
    if (source) {
      const link = element("a", "reference-link", "Product reference ↗");
      link.href = source; link.target = "_blank"; link.rel = "noopener noreferrer";
      imagePanel.append(link);
    }
  }
  const values = [
    ["Category", item.category], ["Color", item.color], ["Size", item.size],
    ["Quantity", item.quantity], ["Material", item.material], ["SKU", item.sku],
    ["Ownership", item.current_ownership_confirmed ? "Confirmed" : "Unconfirmed"],
    ["Condition at purchase", item.condition_at_purchase],
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
  for (const [label, value] of values) definition.append(element("dt", "", label), element("dd", "", value === null || value === undefined || value === "" ? "Unknown" : value));
  const copy = element("div"); copy.append(definition);
  if (Array.isArray(item.notes) && item.notes.length) {
    const notes = element("div", "detail-notes");
    notes.append(element("h3", "", "Notes"));
    for (const note of item.notes) notes.append(element("p", "", note));
    copy.append(notes);
  }
  copy.append(element("p", "detail-notes muted", "To change this item, edit its source JSON and sync the catalog."));
  layout.append(imagePanel, copy);
  byId("detail-content").replaceChildren(layout);
  byId("detail").showModal();
}

function inputValues() {
  return { occasion: byId("occasion").value, ownership_scope: byId("ownership").value, weather_mode: byId("weather-mode").value };
}

function inputLabel(inputs) {
  const occasion = { everyday: "Everyday", work: "Work", dinner: "Dinner" }[inputs.occasion] || "Unknown occasion";
  return `${occasion} · ${inputs.ownership_scope === "confirmed_only" ? "Confirmed only" : "Includes unconfirmed inventory"} · ${inputs.weather_mode === "ignored" ? "Weather ignored" : "Uses forecast"}`;
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
      byId("occasion").value = inputs.occasion;
      byId("ownership").value = inputs.ownership_scope;
      byId("weather-mode").value = inputs.weather_mode;
    }
  }
  const running = generation?.status === "running";
  for (const id of ["occasion", "ownership", "weather-mode"]) byId(id).disabled = running || pendingGeneration !== null;
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
    for (const suggestion of result.suggestions.slice(0, 3)) {
      if (!object(suggestion) || !Array.isArray(suggestion.item_ids)) throw new APIError(200, "invalid_response");
      const card = element("article", `suggestion${result.freshness === "stale" ? " stale" : ""}`);
      const pieces = element("div", "pieces");
      const records = await Promise.all([...new Set(suggestion.item_ids)].slice(0, 5).map(getItem));
      if (revision !== todayRevision || view !== "today") return;
      for (const item of records) pieces.append(itemButton(item, true));
      const copy = element("div", "suggestion-copy");
      copy.append(element("h3", "", suggestion.label || "Another combination"), element("p", "muted", suggestion.advisory_reason || ""));
      if (Array.isArray(suggestion.warnings) && suggestion.warnings.length) copy.append(element("p", "qualifier", suggestion.warnings.join(" · ").replaceAll("_", " ")));
      card.append(pieces, copy); cards.append(card);
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
  for (const id of ["occasion", "ownership", "weather-mode"]) byId(id).disabled = true;
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
    if (!pendingGeneration) for (const id of ["occasion", "ownership", "weather-mode"]) byId(id).disabled = false;
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
  await loadInventory();
}

function selectView(next) {
  todayRevision += 1;
  view = next;
  byId("inventory").hidden = view !== "inventory";
  byId("today").hidden = view !== "today";
  byId("inventory-tab").setAttribute("aria-pressed", String(view === "inventory"));
  byId("today-tab").setAttribute("aria-pressed", String(view === "today"));
  clearTimeout(pollTimer);
  notice();
  if (view === "today") { pollDeadline = 0; void loadToday(); }
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
});
byId("filters").addEventListener("submit", (event) => { event.preventDefault(); void loadInventory(); });
byId("brand").addEventListener("change", () => void loadInventory());
byId("category").addEventListener("change", () => void loadInventory());
byId("load-more").addEventListener("click", () => void loadInventory(true));
byId("inventory-tab").addEventListener("click", () => selectView("inventory"));
byId("today-tab").addEventListener("click", () => selectView("today"));
byId("close-detail").addEventListener("click", () => byId("detail").close());
byId("generate").addEventListener("submit", generate);
byId("generate").addEventListener("change", () => { todayDraft = true; byId("today-input-note").hidden = !lastToday?.result || JSON.stringify(inputValues()) === JSON.stringify(lastToday.result.inputs); });
document.addEventListener("visibilitychange", () => { clearTimeout(pollTimer); if (!document.hidden && view === "today" && system) void loadToday(); });
window.addEventListener("pagehide", () => { clearTimeout(pollTimer); listAbort?.abort(); });
connect().catch((error) => { if (error instanceof APIError && error.status === 401) byId("login").hidden = false; else notice(errorMessage(error)); });
