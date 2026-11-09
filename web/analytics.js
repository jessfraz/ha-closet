"use strict";

const byId = (id) => document.getElementById(id);
const integer = new Intl.NumberFormat();
const count = (value, singular, plural = `${singular}s`) => `${integer.format(value)} ${value === 1 ? singular : plural}`;

function day(value) {
  return new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", timeZone: "UTC" }).format(new Date(`${value}T12:00:00Z`));
}

function date(value, timezone) {
  return new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", year: "numeric", timeZone: timezone }).format(new Date(value));
}

function duration(milliseconds) {
  const days = milliseconds / 86_400_000;
  if (days >= 1) return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(days)} days`;
  const hours = milliseconds / 3_600_000;
  return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(hours)} hours`;
}

function svg(tag, attributes, text) {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, String(value));
  if (text !== undefined) node.textContent = text;
  return node;
}

export class AnalyticsController {
  constructor({ request, element, card, errorMessage, role, changeRange, openTracking, active }) {
    Object.assign(this, { request, element, card, errorMessage, role, changeRange, openTracking, active });
    this.days = 90;
    this.sequence = 0;
    this.abort = null;
    byId("analytics-filters").addEventListener("submit", (event) => {
      event.preventDefault();
      const days = Number(byId("analytics-period").value);
      if (days === this.days) void this.load();
      else this.changeRange(days);
    });
  }

  node(tag, className, text) { return this.element(tag, className, text); }

  setRange(days) {
    this.days = days;
    byId("analytics-period").value = String(days);
  }

  suspend() {
    this.sequence += 1;
    this.abort?.abort();
    this.chartObserver?.disconnect();
    this.chartObserver = null;
    byId("analytics-content").removeAttribute("aria-busy");
  }

  async load({ pick = false } = {}) {
    this.suspend();
    if (!this.active()) return;
    const sequence = this.sequence;
    const content = byId("analytics-content");
    content.replaceChildren();
    byId("analytics-range-label").textContent = "";
    const state = byId("analytics-state");
    if (this.role() !== "owner") {
      state.textContent = "Analytics needs owner or Home Assistant administrator access.";
      return;
    }
    const controller = new AbortController();
    this.abort = controller;
    state.textContent = pick ? "Finding a ready piece…" : "Loading analytics…";
    content.setAttribute("aria-busy", "true");
    try {
      const seed = crypto.getRandomValues(new Uint32Array(1))[0];
      const query = new URLSearchParams({ days: String(this.days), pick_seed: String(seed) });
      const data = await this.request(`v1/tracking/analytics?${query}`, { signal: controller.signal });
      if (sequence !== this.sequence || !this.active()) return;
      if (data.days !== this.days || !Array.isArray(data.daily) || !Array.isArray(data.most_worn) || !Array.isArray(data.overlooked)) throw new Error("invalid_analytics");
      const fragment = this.render(data);
      if (sequence !== this.sequence || !this.active()) return;
      byId("analytics-range-label").textContent = `${date(data.from, data.timezone)} – ${date(data.as_of, data.timezone)} · ${data.timezone.replaceAll("_", " ")}`;
      content.replaceChildren(fragment);
      state.textContent = data.tracking_enabled ? "" : data.tracked_units > 0 || data.inferred_wears > 0
        ? "Tracking is currently off. Saved history is still shown." : "Tracking is currently off.";
      if (pick) byId("analytics-pick-again")?.focus({ preventScroll: true });
    } catch (error) {
      if (error.name !== "AbortError" && sequence === this.sequence && this.active()) state.textContent = this.errorMessage(error);
    } finally {
      if (sequence === this.sequence) content.removeAttribute("aria-busy");
    }
  }

  render(data) {
    const fragment = document.createDocumentFragment();
    if (data.tracked_units === 0 && data.inferred_wears === 0) {
      const empty = this.node("div", "analytics-empty");
      empty.append(this.node("p", "eyebrow", "A LITTLE HISTORY FIRST"), this.node("h2", "", "No wear history yet"));
      empty.append(this.node("p", "muted", "Tracking a few pieces will build a picture of their rotation. Untracked pieces are not included in Least worn."));
      const setup = this.node("button", "quiet", "Open tracking settings");
      setup.type = "button"; setup.addEventListener("click", this.openTracking);
      empty.append(setup); fragment.append(empty);
      return fragment;
    }
    const summary = this.node("dl", "analytics-summary");
    for (const [label, value, detail] of [
      ["Wear episodes", data.inferred_wears, "Inferred from confirmed movement"],
      ["Items worn", data.distinct_items_worn, "Distinct catalog items in this range"],
      ["Tracked pieces", data.tracked_units, `Of ${count(data.total_units, "physical piece")}`],
    ]) {
      const metric = this.node("div", "analytics-metric");
      const valueNode = this.node("dd", "", integer.format(value));
      valueNode.append(this.node("small", "", detail));
      metric.append(this.node("dt", "", label), valueNode);
      summary.append(metric);
    }
    fragment.append(summary, this.trend(data));
    const lists = this.node("div", "analytics-lists");
    const most = this.section("Most worn", "Recorded wear episodes in this range.");
    if (data.most_worn.length) most.append(this.ranked(data.most_worn, (row) => `${count(row.inferred_wears, "wear")}${row.last_worn_at ? ` · last ${date(row.last_worn_at, data.timezone)}` : ""}`));
    else most.append(this.node("p", "muted", "No inferred wear episodes in this range yet."));
    const overlooked = this.section("Least worn", `Ready pieces, ordered by fewest recorded wears. Each has at least ${duration(data.minimum_observation_ms)} of healthy observation in this range.`);
    if (data.overlooked.length) {
      overlooked.append(this.ranked(data.overlooked, (row) => this.overlookedNote(row, data.timezone)));
      const pick = this.node("div", "analytics-pick");
      pick.append(this.node("h3", "", "Give one another turn"));
      if (data.pick) {
        const result = this.ranked([data.pick], (row) => this.overlookedNote(row, data.timezone));
        result.classList.add("analytics-pick-result");
        result.setAttribute("aria-label", "Ready overlooked pick");
        pick.append(result);
      } else pick.append(this.node("p", "muted", "No ready pick is available right now."));
      const button = this.node("button", "quiet", data.pick ? "Pick another" : "Check for a ready piece");
      button.id = "analytics-pick-again"; button.type = "button";
      button.addEventListener("click", () => void this.load({ pick: true }));
      pick.append(button, this.node("p", "muted", "Availability is checked again for each pick."));
      overlooked.append(pick);
    } else {
      overlooked.append(this.node("p", "muted", data.healthy_observation_ms < data.minimum_observation_ms
        ? "Still building observed history. Ready pieces appear here after enough healthy reader coverage."
        : "No ready pieces meet the observation threshold in this range."));
    }
    lists.append(most, overlooked); fragment.append(lists);
    const method = this.node("details", "analytics-method");
    method.append(this.node("summary", "", "How this is counted"), this.node("p", "", "Wear episodes are inferred from recorded status changes, not individual RFID reads. They describe movement, not proof of what someone wore. Manual corrections and gaps in reader coverage affect what can be inferred."), this.node("p", "", `Healthy reader observation in this range: ${duration(data.healthy_observation_ms)}. Each row under Least worn is one physical piece; untracked pieces are excluded. “No wear recorded” refers only to the selected range.`));
    fragment.append(method);
    return fragment;
  }

  section(title, description) {
    const section = this.node("section", "analytics-section");
    const heading = this.node("h2", "", title);
    heading.id = title === "Most worn" ? "analytics-most-heading" : "analytics-overlooked-heading";
    section.setAttribute("aria-labelledby", heading.id);
    section.append(heading, this.node("p", "muted", description));
    return section;
  }

  overlookedNote(row, timezone) {
    const copy = row.item.quantity > 1 || row.ordinal > 1 ? `Piece ${row.ordinal} · ` : "";
    const worn = row.last_worn_at ? `${count(row.inferred_wears, "wear")} in this range · last worn ${date(row.last_worn_at, timezone)}` : "No wear recorded in this range";
    return `${copy}${worn}`;
  }

  ranked(rows, describe) {
    const list = this.node("ol", "analytics-ranked");
    rows.forEach((row, index) => {
      const entry = this.node("li", "");
      const rank = this.node("span", "analytics-rank", String(index + 1).padStart(2, "0"));
      rank.setAttribute("aria-hidden", "true");
      const card = this.card(row.item);
      card.querySelector(":scope > .tracking-pill")?.remove();
      if (row.unit_id) {
        card.dataset.analyticsUnit = row.unit_id;
        if (row.ready_to_wear) {
          const ready = this.node("span", "tracking-pill", "Ready to wear");
          ready.dataset.ready = "true";
          card.insertBefore(ready, card.querySelector(".item-badges"));
        }
      }
      const detail = this.node("span", "analytics-item-note", describe(row));
      card.insertBefore(detail, card.querySelector(".item-badges"));
      entry.append(rank, card); list.append(entry);
    });
    return list;
  }

  trend(data) {
    const figure = this.node("figure", "analytics-trend");
    const caption = this.node("figcaption", "");
    caption.append(this.node("h2", "", "Wear over time"), this.node("p", "muted", count(data.inferred_wears, "episode")));
    figure.append(caption);
    const chart = svg("svg", { viewBox: "0 0 720 210", role: "img", "aria-labelledby": "analytics-chart-title analytics-chart-description", class: "analytics-chart" });
    const draw = (measuredWidth) => {
      chart.setAttribute("viewBox", `0 0 ${measuredWidth} 210`);
      chart.replaceChildren(svg("title", { id: "analytics-chart-title" }, `Daily inferred wear episodes over ${data.days} days`), svg("desc", { id: "analytics-chart-description" }, "Bars show recorded wear episodes. Hollow dots mark days with no healthy observation. The exact values are available in the data table below."));
      const maximum = Math.max(1, ...data.daily.map((entry) => entry.inferred_wears));
      const left = 28, top = 14, width = Math.max(1, measuredWidth - 40), height = 160;
      for (const value of [...new Set([0, Math.ceil(maximum / 2), maximum])]) {
        const y = top + height * (1 - value / maximum);
        chart.append(svg("line", { x1: left, x2: left + width, y1: y, y2: y, class: "chart-guide" }), svg("text", { x: left - 8, y: y + 4, "text-anchor": "end" }, String(value)));
      }
      const step = width / Math.max(1, data.daily.length);
      data.daily.forEach((entry, index) => {
        const barHeight = height * entry.inferred_wears / maximum;
        const x = left + index * step;
        const bar = svg("rect", { x: x + step * .17, y: top + height - barHeight, width: Math.max(.5, step * .66), height: barHeight, rx: Math.min(2, step * .1), class: "chart-bar" });
        bar.append(svg("title", {}, `${day(entry.date)}: ${count(entry.inferred_wears, "wear")}; ${duration(entry.observed_ms)} observed`));
        chart.append(bar);
        if (entry.observed_ms === 0) chart.append(svg("circle", { cx: x + step / 2, cy: top + height + 8, r: Math.min(2, step * .2), fill: "none", stroke: "currentColor" }));
        if (index === 0 || index === Math.floor(data.daily.length / 2) || index === data.daily.length - 1) chart.append(svg("text", { x: Math.min(left + width - 5, Math.max(left + 5, x + step / 2)), y: 202, "text-anchor": index === 0 ? "start" : index === data.daily.length - 1 ? "end" : "middle" }, day(entry.date)));
      });
    };
    draw(720);
    let previousWidth = 0;
    this.chartObserver = new ResizeObserver(([entry]) => {
      const width = entry.contentRect.width;
      if (width > 0 && Math.abs(width - previousWidth) > .1) { previousWidth = width; draw(width); }
    });
    // Observe HTML width, not SVG coordinates. A fixed CSS height prevents the
    // redraw from resizing the observed figure and creating a feedback loop.
    this.chartObserver.observe(figure);
    figure.append(chart, this.node("p", "muted", "○ No healthy observation. Gaps in reader coverage do not count as wear."));
    const details = this.node("details", "analytics-data");
    details.append(this.node("summary", "", "View daily data"));
    const table = this.node("table", ""); table.setAttribute("aria-label", "Daily wear and reader observation");
    const head = this.node("thead", ""), titles = this.node("tr", "");
    for (const label of ["Day", "Wear episodes", "Observation"]) { const th = this.node("th", "", label); th.scope = "col"; titles.append(th); }
    head.append(titles); const body = this.node("tbody", "");
    for (const entry of data.daily) {
      const row = this.node("tr", "");
      row.append(this.node("td", "", day(entry.date)), this.node("td", "", integer.format(entry.inferred_wears)), this.node("td", "", duration(entry.observed_ms))); body.append(row);
    }
    table.append(head, body); details.append(table); figure.append(details);
    return figure;
  }
}
