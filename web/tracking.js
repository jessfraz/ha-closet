"use strict";

const statusLabels = {
  clean_ready: "Clean / ready", worn_ready: "Worn / ready to rewear",
  needs_laundry: "Needs laundry", queued_dry_cleaning: "Queued for dry cleaning",
  being_worn: "Being worn", in_wash: "In the wash", at_cleaners: "At the cleaners", unknown: "Unknown",
};
const areaLabels = { closet: "Closet", rewear: "Rewear rack", hamper: "Laundry hamper", dry_cleaning_bag: "Dry-cleaning bag" };
const exceptionLabels = {
  needs_baseline: "Waiting for the first confirmed area.",
  quantity_unknown: "The number of physical pieces needs confirmation.",
  needs_reconciliation: "The source quantity changed. Review the physical pieces.",
  ambiguous_reads: "Several areas have conflicting evidence. The last confirmed state is retained.",
  insufficient_evidence: "Not enough fresh evidence. The last confirmed state is retained.",
  paused: "Automatic tracking is explicitly paused.", archived: "This catalog record is archived.",
  enrollment_recovery: "The enrollment reader needs recovery before tracking resumes.",
};
const installationLabels = { needs_tag: "Needs a tag", queued: "Queued for installation", installed: "Tag installed", exempt: "No-tag exception" };

export class TrackingController {
  constructor({ request, element, photo, getItem, openItem, sourceLabel, errorMessage, role, changed }) {
    Object.assign(this, { request, element, photo, getItem, openItem, sourceLabel, errorMessage, role, changed });
    this.snapshot = null;
    this.online = false;
    this.view = "inventory";
    this.mode = "enroll";
    this.sessionId = null;
    this.selectedId = null;
    this.replacementId = null;
    this.pending = null;
    this.busy = false;
    this.sequence = 0;
    this.queueFilter = new Set();
    this.filtering = false;
    this.filterRevision = 0;
    this.catalogFacets = null;
    this.catalog = new Map();
    this.detail = null;
    this.workspaceRevision = 0;
    this.queueLimit = 12;
    this.audio = null;
    this.fieldSequence = 0;
    for (const [id, mode] of [["enroll-mode", "enroll"], ["installation-mode", "installation"], ["lookup-mode", "lookup"]]) {
      document.getElementById(id).addEventListener("click", () => { this.mode = mode; this.renderWorkspace(); });
    }
    this.keyHandler = (event) => {
      if (this.view !== "tags" || event.altKey || event.ctrlKey || event.metaKey || /INPUT|SELECT|TEXTAREA|BUTTON/.test(event.target.tagName)) return;
      const id = { r: "capture-reset", s: "queue-skip", Enter: "capture-confirm" }[event.key];
      const button = id && document.getElementById(id);
      if (button && !button.disabled) { event.preventDefault(); button.click(); }
    };
    document.addEventListener("keydown", this.keyHandler);
  }

  node(tag, className, text) { return this.element(tag, className, text); }
  notice(text = "", settings = false, tone = "warning") {
    const node = document.getElementById(settings ? "tracking-settings-notice" : "tag-notice");
    node.textContent = text; node.hidden = !text; node.dataset.tone = tone;
  }
  button(text, action, quiet = true) {
    const button = this.node("button", quiet ? "quiet" : "", text);
    button.type = "button"; button.disabled = this.busy || this.role() !== "owner" || Boolean(this.pending);
    button.dataset.trackingMutation = "true";
    button.addEventListener("click", action);
    return button;
  }
  field(label, options = null, value = "", type = "text") {
    const wrapper = this.node("div", "field");
    const title = this.node("label", "", label);
    const input = this.node(options ? "select" : type === "textarea" ? "textarea" : "input", "");
    input.id = `tracking-field-${++this.fieldSequence}`; title.htmlFor = input.id;
    wrapper.append(title);
    if (options) for (const [key, text] of options) input.add(new Option(text, key));
    else if (type !== "textarea") input.type = type;
    input.value = value ?? "";
    input.disabled = this.busy || Boolean(this.pending);
    if (options?.length && input.selectedIndex < 0) input.selectedIndex = 0;
    wrapper.append(input);
    return { wrapper, input };
  }
  units(itemId = null) {
    return (this.snapshot?.units || []).filter((unit) => !unit.retired && !unit.archived && (itemId === null || unit.catalog_id === itemId));
  }
  area(id) { return this.snapshot?.config.areas.find((area) => area.id === id); }
  pieceLabel(item, unit) {
    const source = this.sourceLabel(item);
    return [item.brand, item.name, item.size ? `size ${item.size}` : null, item.color, source, `piece ${unit.ordinal}`].filter(Boolean).join(" · ");
  }
  confirmationContext(body) {
    const unitId = body.unit_id || (body.type === "start_enrollment" ? body.unit_ids[0] : null);
    const unit = this.snapshot.units.find((entry) => entry.id === unitId);
    const item = unit && (this.catalog.get(unit.catalog_id) || (this.detail?.item.id === unit.catalog_id ? this.detail.item : null));
    return {
      piece: unit ? item ? this.pieceLabel(item, unit) : `${unit.catalog_id} · piece ${unit.ordinal}` : null,
      area: this.area(body.baseline_area || body.area_id)?.label || null,
      purpose: body.purpose || this.snapshot.sessions.find((session) => session.id === body.session_id)?.purpose,
    };
  }
  session() { return this.snapshot?.sessions.find((session) => session.id === this.sessionId); }
  currentUnit() {
    const session = this.session();
    const id = session?.purpose !== "lookup" && session?.state === "active" ? session.queue[session.cursor] : this.selectedId;
    return this.units().find((unit) => unit.id === id);
  }
  remaining() { return this.units().filter((unit) => !unit.has_binding && unit.installation.state !== "exempt"); }
  filteredQueue() { return this.remaining().filter((unit) => !this.filtering || this.queueFilter.has(unit.catalog_id)); }
  ready(unit) { return this.snapshot?.certified_ready_unit_ids.includes(unit.id) || false; }
  pill(unit) {
    const node = this.node("span", "tracking-pill", unit.status === "unknown" ? unit.has_binding ? "Waiting for first area" : "Not tracked" : statusLabels[unit.status]);
    node.dataset.ready = String(this.ready(unit));
    node.dataset.stale = String(Boolean(unit.exception) || !this.online || (this.snapshot?.config.enabled && !this.snapshot.health.absence_coverage_healthy));
    return node;
  }
  badge(item) {
    const units = this.units(item.id);
    if (!this.snapshot?.config.enabled && !units.some((unit) => unit.evidence !== "unknown")) return null;
    if (!units.length) return null;
    const status = new Set(units.map((unit) => unit.status));
    const node = this.pill(units[0]);
    if (status.size > 1) { node.textContent = `${units.length} pieces · mixed statuses`; node.dataset.ready = "false"; }
    else if (units.length > 1) node.textContent += ` · ${units.length} pieces`;
    return node;
  }

  async refresh() {
    clearTimeout(this.timer);
    this.abort?.abort();
    const controller = new AbortController(); this.abort = controller;
    const sequence = ++this.sequence;
    try {
      const data = await this.request("v1/tracking", { signal: controller.signal });
      if (sequence !== this.sequence) return;
      if (!data || !Number.isSafeInteger(data.tracking_revision) || !Array.isArray(data.units) || !Array.isArray(data.sessions) || !data.config || !data.health || !Array.isArray(data.certified_ready_unit_ids)) throw new Error("Invalid tracking response");
      this.snapshot = data; this.online = true;
      if (!this.sessionId) this.sessionId = data.sessions.find((session) => session.state === "active")?.id || null;
      const session = this.session();
      // Ended queues stay resumable, but only a live session owns the selection.
      if (session?.state === "active" && session.purpose !== "lookup" && session.queue[session.cursor]) this.selectedId = session.queue[session.cursor];
      this.changed(); this.renderHealth(); this.renderDetail();
      if (this.view === "tags") await this.renderWorkspace();
      if (this.view === "tracking-settings") this.renderSettings();
    } catch (error) {
      if (error.name === "AbortError" || sequence !== this.sequence) return;
      this.online = false; this.changed(); this.renderHealth(); this.renderDetail();
      if (this.view === "tags") this.notice("Tracking is unavailable. Existing states are retained; pairing is disabled until the connection recovers.");
      this.invalidateConfirmation();
    } finally {
      if (sequence === this.sequence && !document.hidden) this.timer = setTimeout(() => this.refresh(), this.view === "tags" && this.session()?.state === "active" ? 1000 : 15000);
    }
  }
  setView(view) {
    this.view = view;
    clearTimeout(this.timer);
    if (!document.hidden) void this.refresh();
  }
  suspend() { clearTimeout(this.timer); this.abort?.abort(); this.sequence += 1; this.online = false; this.invalidateConfirmation(); }
  destroy() { this.suspend(); document.removeEventListener("keydown", this.keyHandler); if (this.audio) void this.audio.close().catch(() => {}); }
  invalidateConfirmation() {
    const button = document.getElementById("capture-confirm"); if (button) button.disabled = true;
  }
  renderHealth() {
    const node = document.getElementById("tracking-health");
    const enabled = Boolean(this.snapshot?.config.enabled);
    node.hidden = !enabled;
    for (const field of document.querySelectorAll("[data-tracking-filter]")) field.hidden = !enabled && !this.snapshot?.units.some((unit) => unit.evidence !== "unknown");
    const statuses = document.getElementById("tracking-status");
    for (const [key, value] of Object.entries(statusLabels)) {
      const existing = [...statuses.options].find((option) => option.value === key);
      if (existing) existing.textContent = value;
      else statuses.add(new Option(value, key));
    }
    if (this.snapshot) {
      const areas = document.getElementById("tracking-area"), selected = areas.value;
      areas.replaceChildren(new Option("All areas", ""), ...this.snapshot.config.areas.map((area) => new Option(area.label, area.id)));
      if (selected && ![...areas.options].some((option) => option.value === selected)) areas.add(new Option(selected, selected));
      areas.value = selected;
    }
    if (!enabled) return;
    const health = this.snapshot.health;
    const warning = !this.online || !health.absence_coverage_healthy;
    node.dataset.warning = String(warning);
    node.textContent = !this.online ? "Tracking connection lost. Last known states are retained." : health.warming_up ? "Reader coverage is warming up. Last known states are retained." : !health.absence_coverage_healthy ? "Reader coverage needs attention. Missing reads will not mark the wardrobe worn." : "Tracking coverage is healthy.";
  }
  async command(body, revision = this.snapshot?.tracking_revision) {
    if (this.busy || this.role() !== "owner" || !this.snapshot) return null;
    this.pending ||= { body: structuredClone(body), key: crypto.randomUUID(), revision, confirmation: this.confirmationContext(body) };
    const roots = [document.getElementById("tag-workspace"), document.getElementById("tracking-settings-content"), this.detail?.root].filter(Boolean);
    // Keep the original control state through an uncertain response and retry.
    // Cached views may survive readback, so rendering alone cannot unlock them.
    this.commandControls ||= new Map();
    for (const root of roots) {
      root.setAttribute("aria-busy", "true");
      for (const control of root.querySelectorAll("input,select,textarea,[data-tracking-mutation]")) {
        if (!this.commandControls.has(control)) this.commandControls.set(control, control.disabled);
        control.disabled = true;
      }
    }
    this.busy = true; this.invalidateConfirmation();
    let acknowledged = false;
    try {
      const pending = this.pending;
      const result = await this.request("v1/tracking/commands", {
        method: "POST", headers: { "If-Match": `"${pending.revision}"`, "Idempotency-Key": pending.key }, body: JSON.stringify(pending.body),
      });
      if (pending.body.type === "select_candidate") this.replacementId = null;
      this.pending = null;
      acknowledged = true;
      if (result.session) this.sessionId = result.session.id;
      if (pending.body.type === "select_candidate") this.captureBaseline = "";
      // Keep the acknowledged copy's identity through queue advance and retries.
      const { piece, area, purpose } = pending.confirmation;
      const initialArea = area ? ` · Initial area: ${area}` : "";
      const messages = {
        configure: "Tracking settings saved.", installation: "Installation saved.",
        correct: "Tracking status saved.", baseline: `Area baseline saved for ${piece} · ${area}.`,
        select_candidate: purpose === "replace" ? `Replacement tag saved for ${piece}${initialArea}.` : `Tag paired to ${piece}${initialArea}.`,
        start_enrollment: purpose === "replace" ? `Replacement scan started for ${piece}.` : purpose === "lookup" ? "Lookup started. Read a tag to find its garment." : "Enrollment session started.",
        abort_enrollment: "Session ended.", skip: "Piece skipped.", reset_capture: "Ready for a fresh scan.",
      };
      this.notice(messages[pending.body.type] || "Change saved.", this.view === "tracking-settings", "ok");
      return result;
    } catch (error) {
      if (Number.isInteger(error.status) && error.status >= 400 && error.status < 500 && ![408, 429].includes(error.status)) this.pending = null;
      this.notice(this.pending ? "The action was not acknowledged. Retry the same action to recover its result. Nothing will be paired automatically." : this.errorMessage(error), this.view === "tracking-settings");
      return null;
    } finally {
      this.busy = false;
      await this.refresh();
      if (!this.pending) {
        for (const [control, disabled] of this.commandControls) {
          if (control.isConnected && control.id !== "capture-confirm") control.disabled = disabled || (control.matches("[data-tracking-mutation]") && this.role() !== "owner");
        }
        this.commandControls = null;
      }
      // Confirmation always recomputes freshness, isolation and connection guards.
      this.captureKey = null; this.renderCapture();
      this.renderDetail(true);
      if (this.view === "tracking-settings") this.renderSettings(acknowledged);
      for (const root of [document.getElementById("tag-workspace"), document.getElementById("tracking-settings-content"), this.detail?.root]) root?.setAttribute("aria-busy", "false");
    }
  }
  retryPanel(root) {
    const existing = root.querySelector(".tracking-retry");
    if (!this.pending) { existing?.remove(); return; }
    if (existing?.dataset.key === this.pending.key) {
      existing.querySelector("button").disabled = this.busy || this.role() !== "owner";
      return;
    }
    existing?.remove();
    const panel = this.node("div", "notice tracking-retry", "An earlier action needs readback before making another change.");
    panel.dataset.key = this.pending.key;
    const retry = this.button("Retry same action", () => this.command(this.pending.body), false);
    retry.disabled = this.busy || this.role() !== "owner";
    panel.append(retry); root.prepend(panel);
  }

  detailSection(item) {
    const root = this.node("section", "tracking-detail");
    this.detail = { item, root };
    this.renderDetail();
    return root;
  }
  renderDetail(force = false) {
    if (!this.detail) return;
    const { root, item } = this.detail;
    if (!force && root.contains(document.activeElement)) return;
    const key = JSON.stringify([this.snapshot?.units.filter((unit) => unit.catalog_id === item.id), this.online, this.pending?.key]);
    if (!force && this.detail.key === key) return;
    this.detail.key = key;
    root.replaceChildren(this.node("h3", "", "Tracking"));
    if (!this.snapshot) { root.append(this.node("p", "muted", "Tracking is unavailable. Catalog details remain readable.")); return; }
    const units = this.snapshot.units.filter((unit) => unit.catalog_id === item.id);
    if (!units.length) { root.append(this.node("p", "muted", "No physical pieces are declared for this record.")); return; }
    for (const unit of units) {
      const block = this.node("div", "tracking-unit");
      const heading = this.node("div", "tracking-unit-heading");
      const pill = this.pill(unit);
      if (unit.retired) pill.textContent = "Retired tracking unit";
      heading.append(this.node("strong", "", `Piece ${unit.ordinal}`), pill); block.append(heading);
      const area = this.area(unit.current_area || unit.last_confirmed_area);
      const explanation = unit.exception ? exceptionLabels[unit.exception] || "Tracking needs attention." : area ? `${unit.current_area ? "Confirmed area" : "Last confirmed area"}: ${area.label}.` : unit.has_binding ? "Waiting for the first confirmed area." : "No RFID tag is paired. This record remains in your inventory.";
      block.append(this.node("p", "muted", explanation));
      if (unit.installation.state === "exempt") block.append(this.node("p", "muted", `No-tag exception${unit.installation.no_tag_reason ? `: ${unit.installation.no_tag_reason}` : ""}`));
      if (unit.evidence === "manual") block.append(this.node("p", "muted", "Manually confirmed. No RFID certainty is implied."));
      const entity = this.node("details", ""); entity.append(this.node("summary", "", "Home Assistant entity"));
      entity.append(this.node("p", "muted", "Optional and disabled by default. Find this default name in Closet's Home Assistant entities to enable its status sensor:"), this.node("code", "", `Piece ${unit.id.slice(-8)}`)); block.append(entity);
      const history = this.node("details", ""); history.append(this.node("summary", "", "Movement and corrections"));
      let loaded = false;
      history.addEventListener("toggle", async () => {
        if (!history.open || loaded) return;
        loaded = true;
        try {
          const page = await this.request(`v1/tracking/units/${encodeURIComponent(unit.id)}/history?limit=20`);
          const list = this.node("ol", "tracking-history");
          for (const event of page.events) {
            const row = this.node("li", "");
            const time = this.node("time", "", new Date(event.at).toLocaleString()); time.dateTime = event.at;
            row.append(time, document.createTextNode(`${event.automatic ? "Automatic" : "Manual"} · ${event.reason.replaceAll("_", " ")}: ${statusLabels[event.after.unit.status] || "Unknown"}${event.note ? ` · ${event.note}` : ""}`)); list.append(row);
          }
          history.append(page.events.length ? list : this.node("p", "muted", "No movement history yet."));
          if (page.next_before !== null) history.append(this.node("p", "muted", "Showing the most recent 20 events."));
        } catch { history.append(this.node("p", "muted", "History could not be loaded.")); loaded = false; }
      });
      block.append(history);
      if (this.role() === "owner") this.unitActions(block, unit);
      root.append(block);
    }
    this.retryPanel(root);
  }
  unitActions(root, unit) {
    const revision = this.snapshot.tracking_revision;
    const actions = this.node("div", "tracking-actions");
    if (unit.retired) {
      if (unit.last_event_id !== null) actions.append(this.button("Undo retirement", () => this.reasonAction(root, "undo", unit, revision)));
      root.append(actions); return;
    }
    actions.append(this.button(unit.has_binding ? "Replace tag" : "Enroll tag", () => {
      this.selectedId = unit.id; this.replacementId = unit.has_binding ? unit.id : null; this.mode = "enroll";
      document.getElementById("tags-tab").click();
    }));
    const manual = this.node("details", ""); manual.append(this.node("summary", "", "Correct this state"));
    const form = this.node("form", "tracking-form");
    const status = this.field("Status", Object.entries(statusLabels), unit.status);
    const note = this.field("Reason", null, ""); note.input.required = true; note.input.maxLength = 500;
    const save = this.button("Save correction", () => { if (form.reportValidity()) void this.command({ type: "correct", unit_id: unit.id, status: status.input.value, note: note.input.value }, revision); });
    form.addEventListener("submit", (event) => { event.preventDefault(); save.click(); });
    form.append(status.wrapper, note.wrapper, save); manual.append(form); root.append(manual);
    const area = this.field("Confirm current area", this.snapshot.config.areas.filter((area) => area.enabled).map((area) => [area.id, area.label]), unit.current_area || unit.last_confirmed_area);
    if (area.input.options.length) { const group = this.node("div", "tracking-form"); group.append(area.wrapper, this.button("Set area baseline", () => this.command({ type: "baseline", unit_id: unit.id, area_id: area.input.value }, revision))); root.append(group); }
    actions.append(this.button(unit.paused ? "Resume automation" : "Pause automation", () => this.command({ type: "pause", unit_id: unit.id, paused: !unit.paused }, revision)));
    if (unit.has_binding) actions.append(this.button("Unbind mistaken tag", () => this.reasonAction(root, "unbind", unit, revision)));
    actions.append(this.button("Retire tracking unit", () => this.reasonAction(root, "retire", unit, revision)));
    if (unit.last_event_id !== null) actions.append(this.button("Undo last change", () => this.reasonAction(root, "undo", unit, revision)));
    root.append(actions);
  }
  reasonAction(root, type, unit, revision) {
    if (root.querySelector(".reason-form")) return;
    const form = this.node("form", "tracking-form reason-form");
    const reason = this.field("Reason for this change", null, ""); reason.input.required = true; reason.input.maxLength = 500;
    const save = this.button(type === "undo" ? "Confirm undo" : type === "retire" ? "Confirm retirement" : "Confirm unbind", () => {
      if (!form.reportValidity()) return;
      void this.command({ type, ...(type === "undo" ? { event_id: unit.last_event_id } : { unit_id: unit.id }), ...(type === "unbind" ? { mistaken: true } : {}), note: reason.input.value }, revision);
    });
    form.addEventListener("submit", (event) => { event.preventDefault(); save.click(); }); form.append(reason.wrapper, save); root.append(form); reason.input.focus();
  }

  async facets() {
    if (!this.catalogFacets) {
      let cursor = null;
      const records = new Map();
      let facets = null;
      do {
        const query = new URLSearchParams({ limit: "100" });
        if (cursor) query.set("page_token", cursor);
        const page = await this.request(`v1/items?${query}`);
        for (const item of page.items) records.set(item.id, item);
        facets = page.facets;
        cursor = page.next_page;
      } while (cursor);
      this.catalog = records; this.catalogFacets = facets;
    }
    return this.catalogFacets;
  }
  async item(id) { return this.catalog.get(id) || this.getItem(id); }
  async queueFilters(root) {
    const facets = await this.facets();
    const form = this.node("form", "filters");
    const fields = {
      q: this.field("Find a piece", null, this.queueQuery?.q || ""),
      brand: this.field("Brand", [["", "All brands"], ...facets.brands.map((value) => [value, value])], this.queueQuery?.brand),
      category: this.field("Category", [["", "All categories"], ...facets.categories.map((value) => [value, value])], this.queueQuery?.category),
      source: this.field("Source", [["", "All sources"], ...facets.sources.map((value) => [value.source, value.label])], this.queueQuery?.source),
    };
    fields.q.input.type = "search"; fields.q.input.maxLength = 200;
    for (const field of Object.values(fields)) form.append(field.wrapper);
    const search = this.node("button", "quiet", "Find garments"); search.type = "submit";
    form.append(search);
    form.addEventListener("submit", async (event) => {
      event.preventDefault(); search.disabled = true; this.invalidateConfirmation();
      const query = Object.fromEntries(Object.entries(fields).map(([key, field]) => [key, field.input.value.trim()]));
      const revision = ++this.filterRevision;
      try {
        const matches = new Set(); let cursor = null;
        do {
          const parameters = new URLSearchParams({ limit: "100" });
          if (cursor) parameters.set("page_token", cursor);
          else for (const [key, value] of Object.entries(query)) if (value) parameters.set(key, value);
          const page = await this.request(`v1/items?${parameters}`);
          if (revision !== this.filterRevision) return;
          for (const item of page.items) matches.add(item.id);
          cursor = page.next_page;
        } while (cursor);
        this.queueQuery = query; this.queueFilter = matches; this.filtering = Object.values(query).some(Boolean);
        if (this.session()?.state !== "active") this.selectedId = this.filteredQueue()[0]?.id || null;
        this.workspaceKey = null; await this.renderWorkspace();
      } catch (error) { this.notice(this.errorMessage(error)); }
      finally { search.disabled = false; }
    });
    root.append(form);
  }
  async renderWorkspace() {
    const workspace = document.getElementById("tag-workspace");
    if (!this.snapshot) { workspace.replaceChildren(this.node("p", "muted", "Connecting to tracking…")); return; }
    const remaining = this.remaining();
    const exceptions = this.units().filter((unit) => unit.installation.state === "exempt").length;
    const counts = this.snapshot.counts;
    const blockers = [
      counts.quantity_unknown_items ? `${counts.quantity_unknown_items} ${counts.quantity_unknown_items === 1 ? "quantity needs" : "quantities need"} review` : null,
      counts.unallocated_units ? `${counts.unallocated_units} unallocated ${counts.unallocated_units === 1 ? "piece" : "pieces"}` : null,
      counts.capacity_limited_items ? `${counts.capacity_limited_items} ${counts.capacity_limited_items === 1 ? "record at" : "records at"} tracking capacity` : null,
    ].filter(Boolean);
    document.getElementById("tag-progress").textContent = `${counts.tagged_units} tagged · ${remaining.length} remaining · ${exceptions} no-tag exceptions${blockers.length ? ` · ${blockers.join(" · ")}` : ""}`;
    for (const [id, mode] of [["enroll-mode", "enroll"], ["installation-mode", "installation"], ["lookup-mode", "lookup"]]) document.getElementById(id).setAttribute("aria-pressed", String(this.mode === mode));
    const session = this.session();
    let unit = this.currentUnit();
    if (session?.state !== "active" && unit && this.replacementId !== unit.id && !remaining.some((piece) => piece.id === unit.id)) {
      unit = null; this.selectedId = null;
    }
    if (!unit && this.mode !== "lookup") {
      unit = this.filteredQueue()[0]; this.selectedId = unit?.id || null;
    }
    const key = JSON.stringify([this.mode, this.filterRevision, this.role(), blockers, unit?.id, unit?.has_binding, unit?.installation, session?.id, session?.cursor, session?.state, this.pending, this.snapshot.config.ports, this.snapshot.config.readers.map((reader) => [reader.id, reader.enabled]), this.snapshot.reader_readbacks.map((readback) => [readback.reader_id, readback.capabilities.inventory_pause])]);
    if (key === this.workspaceKey) { this.renderCapture(); return; }
    this.workspaceKey = key;
    const revision = ++this.workspaceRevision;
    const fragment = document.createDocumentFragment();
    this.retryPanel(fragment);
    if (blockers.length) {
      const warning = this.node("div", "notice", "Some physical pieces still need quantity or tracking-capacity review. Enrollment is not complete.");
      const review = this.node("button", "quiet", "Review tracking setup"); review.type = "button";
      review.addEventListener("click", () => document.getElementById("tracking-settings-tab").click());
      warning.append(review); fragment.append(warning);
    }
    if (this.role() !== "owner") fragment.append(this.node("p", "notice", "Owner access is needed to change tags or installation records."));
    await this.queueFilters(fragment);
    if (revision !== this.workspaceRevision) return;
    if (this.mode === "enroll" && session?.state !== "active") {
      const choices = this.replacementId === unit?.id ? [unit, ...this.filteredQueue().filter((piece) => piece.id !== unit.id)] : this.filteredQueue();
      const select = this.field("Choose a physical piece", choices.map((piece) => [piece.id, `Piece ${piece.ordinal} · ${piece.catalog_id}`]), this.selectedId);
      select.input.addEventListener("change", () => { this.invalidateConfirmation(); this.selectedId = select.input.value; this.replacementId = null; this.workspaceKey = null; void this.renderWorkspace(); });
      fragment.append(select.wrapper);
      if (unit) {
        const item = await this.item(unit.catalog_id);
        if (revision !== this.workspaceRevision) return;
        const summary = this.node("p", "muted selected-copy-details", [item.size ? `Size ${item.size}` : null, this.sourceLabel(item), item.color, `Piece ${unit.ordinal}`].filter(Boolean).join(" · "));
        summary.id = "selected-copy-details"; summary.setAttribute("role", "status");
        select.input.setAttribute("aria-describedby", summary.id); fragment.append(summary);
      }
      // Resolve names for the queue without an N+1 request on each poll.
      void Promise.all(choices.map(async (piece, index) => {
        try { const item = await this.item(piece.catalog_id); select.input.options[index].textContent = `${piece.id === this.replacementId ? "Replacement · " : ""}${this.pieceLabel(item, piece)}`; } catch { /* Keep the stable catalog identifier when offline. */ }
      }));
    }
    if (this.mode === "installation") {
      await this.installationQueue(fragment, revision);
    } else if (this.mode === "lookup") {
      fragment.append(this.node("h2", "", "Scan a tag to find its garment"));
      fragment.append(this.node("p", "muted", "Use the enrollment position. Lookup does not pair tags or change garment status."));
      fragment.append(this.stationControls("lookup"));
      fragment.append(this.node("div", "capture-panel", ""));
    } else if (!unit) {
      fragment.append(this.node("h2", "", remaining.length ? "No matching untagged pieces" : blockers.length ? "More pieces need review" : "The queue is complete"));
      fragment.append(this.node("p", "muted", remaining.length ? "Try another brand, category, source or search." : blockers.length ? "Confirm unknown quantities or resolve unallocated pieces in Tracking settings before finishing enrollment." : "Every active piece is paired or has an explicit no-tag exception."));
    } else {
      const item = await this.item(unit.catalog_id);
      if (revision !== this.workspaceRevision) return;
      const layout = this.node("div", "enroll-layout");
      const image = this.node("div", "enroll-photo"); image.append(this.photo(item, 0, "detail"));
      const copy = this.node("div", "enroll-copy");
      copy.append(this.node("p", "eyebrow", item.brand), this.node("h2", "", item.name));
      copy.append(this.node("p", "muted", this.pieceLabel(item, unit)));
      copy.append(this.node("p", "muted", "Place only this piece at the enrollment antenna, then start a fresh scan. The reader supplies the tag; the phone does not scan UHF tags."));
      copy.append(this.stationControls(unit.has_binding ? "replace" : "bind"));
      copy.append(this.node("div", "capture-panel", ""));
      const actions = this.node("div", "enroll-actions");
      const skip = this.button("Skip this piece", async () => {
        const active = this.session();
        if (active?.state === "active") await this.command({ type: "skip", session_id: active.id, capture_version: active.capture_version, unit_id: unit.id });

      });
      skip.id = "queue-skip"; skip.disabled ||= session?.state !== "active"; actions.append(skip);
      if (session?.state === "active") actions.append(this.button("End session", () => this.command({ type: "abort_enrollment", session_id: session.id })));
      copy.append(actions, this.node("p", "muted", "Keyboard: R scans again · S skips · Enter confirms when available."));
      const installation = this.node("details", ""); installation.append(this.node("summary", "", "Installation details or no-tag exception"));
      this.installationForm(installation, unit); copy.append(installation);
      layout.append(image, copy); fragment.append(layout);
    }
    if (revision !== this.workspaceRevision) return;
    this.captureKey = null; workspace.replaceChildren(fragment); this.renderCapture();
  }
  stationControls(purpose) {
    const root = this.node("div", "station-controls");
    const ports = this.snapshot.config.ports.filter((port) => port.enabled && this.snapshot.config.readers.some((reader) => reader.id === port.reader_id && reader.enabled) && (port.role === "enrollment" || this.snapshot.reader_readbacks.some((readback) => readback.reader_id === port.reader_id && readback.capabilities.inventory_pause)));
    const port = this.field("Reading position", ports.map((port) => {
      const reader = this.snapshot.config.readers.find((reader) => reader.id === port.reader_id);
      return [port.id, `${reader?.label || port.reader_id} · antenna ${port.antenna}${port.role === "enrollment" ? " · enrollment" : " · borrowed area antenna"}`];
    }), this.session()?.port_id);
    root.append(port.wrapper);
    const start = this.button(purpose === "lookup" ? "Start lookup scan" : purpose === "replace" ? "Scan replacement tag" : "Start enrollment session", async () => {
      const selected = ports.find((candidate) => candidate.id === port.input.value);
      if (!selected) return;
      const queue = this.filteredQueue(); const chosen = this.currentUnit();
      const unitIds = purpose === "lookup" ? [] : purpose === "replace" ? [chosen.id] : [chosen.id, ...queue.filter((unit) => unit.id !== chosen.id).slice(0, 4999).map((unit) => unit.id)];
      const result = await this.command({ type: "start_enrollment", unit_ids: unitIds, port_id: selected.id, borrow: selected.role !== "enrollment", purpose, ttl_seconds: 900 });
      if (result?.session) { this.sessionId = result.session.id; this.workspaceKey = null; await this.renderWorkspace(); }
    }, false);
    start.disabled ||= !ports.length || this.session()?.state === "active";
    const actions = this.node("div", "enroll-actions"); actions.append(start);
    const saved = [...this.snapshot.sessions].reverse().find((session) => session.purpose === purpose && session.state !== "active" && session.state !== "complete" && session.cursor < session.queue.length && ["not_borrowed", "restored"].includes(session.borrow_state));
    if (purpose !== "lookup" && saved && this.session()?.state !== "active") actions.append(this.button("Resume saved queue", () => this.command({ type: "resume_enrollment", session_id: saved.id, ttl_seconds: 900 }), false));
    root.append(actions);
    if (!ports.length) {
      root.append(this.node("p", "muted", "A dedicated enrollment position is needed. Monitored positions are available only when the connected reader supports pausing their inventory."));
      const settings = this.node("button", "quiet", "Open tracking settings"); settings.type = "button";
      settings.addEventListener("click", () => document.getElementById("tracking-settings-tab").click()); root.append(settings);
    }
    const sound = this.node("label", "sound-option");
    const checkbox = this.node("input", ""); checkbox.type = "checkbox"; checkbox.checked = Boolean(this.audio);
    checkbox.addEventListener("change", async () => {
      if (checkbox.checked) { this.audio = new AudioContext(); await this.audio.resume(); }
      else if (this.audio) { await this.audio.close(); this.audio = null; }
    });
    sound.append(checkbox, document.createTextNode("Optional confirmation sound")); root.append(sound);
    return root;
  }
  renderCapture() {
    const panel = document.querySelector("#tag-workspace .capture-panel"); if (!panel) return;
    const session = this.session();
    const previousFocus = panel.contains(document.activeElement) ? document.activeElement.id : null;
    if (!session || session.state !== "active") { panel.replaceChildren(this.node("h3", "", "Reader scan")); panel.append(this.node("p", "muted", session?.state === "recovery_required" ? "Reader recovery is required before another capture." : "Start a session to capture a fresh tag.")); return; }
    const now = Date.now();
    const portHealth = this.snapshot.health.ports.find((port) => port.port_id === session.port_id);
    const disconnected = ["disconnected", "fault"].includes(portHealth?.state);
    const isolated = ["not_borrowed", "paused"].includes(session.borrow_state);
    const fresh = session.candidates.filter((candidate) => now - Date.parse(candidate.last_seen_at) <= this.snapshot.config.thresholds.candidate_fresh_ms);
    const key = JSON.stringify([session.id, session.capture_version, session.capture_overflow, this.currentUnit()?.id, disconnected, isolated, this.online, this.busy, this.pending?.key, Date.parse(session.expires_at) <= now, session.candidates.map((candidate) => [candidate.epc, candidate.bound_unit_id, candidate.selectable, fresh.includes(candidate)])]);
    if (this.captureKey === key) return;
    this.captureKey = key;
    panel.replaceChildren(this.node("h3", "", "Reader scan"));
    if ((this.mode === "lookup") !== (session.purpose === "lookup")) {
      panel.append(this.node("p", "notice", "End the current session before switching scan modes."), this.button("End current session", () => this.command({ type: "abort_enrollment", session_id: session.id }))); return;
    }
    const candidates = this.node("ul", "capture-candidates");
    for (const [index, candidate] of session.candidates.entries()) {
      const row = this.node("li", "", candidate.bound_unit_id ? `Detected tag ${index + 1} · already paired` : `Detected tag ${index + 1} · unpaired`);
      row.append(this.node("span", "muted", session.purpose === "lookup" ? fresh.includes(candidate) ? "Fresh observation" : "Stale observation" : candidate.selectable ? "Fresh isolated candidate" : fresh.includes(candidate) ? "Not eligible for this pairing" : "Stale observation"));
      if (candidate.bound_unit_id) {
        const bound = this.snapshot.units.find((unit) => unit.id === candidate.bound_unit_id);
        if (bound) {
          const holder = this.node("p", "muted", "Loading matched garment…"); row.append(holder);
          void this.item(bound.catalog_id).then((item) => {
            const open = this.node("button", "quiet lookup-open", this.pieceLabel(item, bound));
            open.type = "button"; open.setAttribute("aria-label", `View matched ${item.name}`);
            open.addEventListener("click", () => this.openItem(item)); holder.replaceChildren(open);
          }).catch(() => { holder.textContent = "Matched garment could not be loaded."; });
        }
      }
      candidates.append(row);
    }
    panel.append(candidates);
    const message = !this.online || disconnected ? session.purpose === "lookup" ? "Reader connection unavailable. Lookup is paused." : "Reader connection unavailable. Confirmation is blocked." : !isolated ? "Waiting for the reader to isolate this antenna. Normal area coverage is intentionally paused." : Date.parse(session.expires_at) <= now ? "This session expired. Start a new session." : session.capture_overflow ? "Too many tags were detected in this capture. Clear the reading position and scan again." : fresh.length > 1 ? "More than one tag is present. Move other garments away and scan again." : fresh.length === 0 ? "Waiting for a fresh tag. Keep just this piece in the reading position." : session.purpose === "lookup" ? fresh[0].bound_unit_id ? "Garment found. Open its details below." : "No garment is paired with this tag." : fresh[0].bound_unit_id ? "This tag is already paired. Check the garment before changing any binding." : fresh[0].selectable ? "One fresh tag is ready for confirmation." : "Keep this piece still while the reader confirms a stable isolated tag.";
    panel.append(this.node("p", "capture-state", message));
    if (!this.online || disconnected) {
      panel.append(this.node("p", "muted reader-recovery", "Check the reader’s power and network connection. After it reconnects, choose Scan again and wait for a fresh isolated tag."));
      const diagnostics = this.node("button", "quiet", "Open reader diagnostics"); diagnostics.type = "button";
      diagnostics.addEventListener("click", () => { this.openDiagnosticsRequested = true; document.getElementById("tracking-settings-tab").click(); }); panel.append(diagnostics);
    }
    const actions = this.node("div", "enroll-actions");
    const reset = this.button("Scan again", () => this.command({ type: "reset_capture", session_id: session.id })); reset.id = "capture-reset"; actions.append(reset);
    if (session.purpose !== "lookup") {
      const unit = this.currentUnit();
      if (this.baselineUnitId !== unit?.id) { this.captureBaseline = ""; this.baselineUnitId = unit?.id; }
      const baseline = this.field("Initial area (optional)", [["", "Wait for normal area evidence"], ...this.snapshot.config.areas.filter((area) => area.enabled).map((area) => [area.id, area.label])]);
      baseline.input.value = this.captureBaseline || "";
      baseline.input.addEventListener("change", () => { this.captureBaseline = baseline.input.value; });
      panel.append(baseline.wrapper);
      const captureVersion = session.capture_version, candidate = fresh.length === 1 ? fresh[0] : null;
      const confirm = this.button(session.purpose === "replace" ? "Confirm replacement" : "Confirm pairing & next", async () => {
        const current = this.session();
        const currentCandidate = current?.candidates.find((entry) => entry.epc === candidate?.epc);
        if (!this.online || disconnected || !isolated || !currentCandidate?.selectable || Date.now() - Date.parse(currentCandidate.last_seen_at) > this.snapshot.config.thresholds.candidate_fresh_ms || !unit || current?.id !== session.id || current.capture_version !== captureVersion || this.currentUnit()?.id !== unit.id) { this.notice("The selected piece or scan changed. Scan again before confirming."); return; }
        const result = await this.command({ type: "select_candidate", session_id: session.id, capture_version: captureVersion, unit_id: unit.id, epc: candidate.epc, baseline_area: baseline.input.value || null });
        if (result && this.audio) { const tone = this.audio.createOscillator(), volume = this.audio.createGain(); volume.gain.value = .035; tone.frequency.value = 660; tone.connect(volume); volume.connect(this.audio.destination); tone.start(); tone.stop(this.audio.currentTime + .08); }
      }, false);
      confirm.id = "capture-confirm";
      confirm.disabled ||= !this.online || disconnected || !isolated || !unit || !candidate?.selectable || fresh.length !== 1 || Date.parse(session.expires_at) <= now;
      actions.append(confirm);
    } else actions.append(this.button("End lookup", () => this.command({ type: "abort_enrollment", session_id: session.id })));
    panel.append(actions);
    if (previousFocus) panel.querySelector(`#${previousFocus}`)?.focus({ preventScroll: true });
  }
  installationForm(root, unit) {
    const form = this.node("form", "tracking-form");
    const state = this.field("Installation", Object.entries(installationLabels), unit.installation.state);
    const model = this.field("Tag model (optional)", null, unit.installation.tag_model);
    const placement = this.field("Attachment location (optional)", null, unit.installation.placement);
    const notes = this.field("Short notes / no-tag reason (optional)", null, unit.installation.notes, "textarea");
    const reason = this.field("No-tag reason (optional)", null, unit.installation.no_tag_reason);
    notes.wrapper.firstChild.textContent = "Short installation notes (optional)";
    for (const field of [model, placement, notes, reason]) field.input.maxLength = 500;
    const revision = this.snapshot.tracking_revision;
    const save = this.button("Save installation", () => this.command({ type: "installation", unit_id: unit.id, installation: { state: state.input.value, tag_model: model.input.value.trim() || null, placement: placement.input.value.trim() || null, notes: notes.input.value.trim() || null, no_tag_reason: reason.input.value.trim() || null } }, revision));
    form.addEventListener("submit", (event) => { event.preventDefault(); save.click(); });
    form.append(state.wrapper, model.wrapper, placement.wrapper, notes.wrapper, reason.wrapper, save); root.append(form);
  }
  async installationQueue(root, revision) {
    root.append(this.node("p", "muted", "Test a few tags for comfort and drape before choosing an attachment location. An existing care label is only an attachment location, not an RFID identifier."));
    const units = this.units().filter((unit) => !this.filtering || this.queueFilter.has(unit.catalog_id));
    const grid = this.node("div", "tag-queue");
    for (const unit of units.slice(0, this.queueLimit)) {
      const item = await this.item(unit.catalog_id);
      if (revision !== this.workspaceRevision) return;
      const card = this.node("article", "tag-queue-card");
      card.append(this.photo(item), this.node("h3", "", item.name), this.node("p", "muted", `${item.brand || ""} · piece ${unit.ordinal}`));
      card.append(this.node("p", "muted", installationLabels[unit.installation.state]));
      const actions = this.node("div", "tracking-actions");
      const trackingRevision = this.snapshot.tracking_revision;
      if (unit.installation.state === "needs_tag") actions.append(this.button("Queue installation", () => this.command({ type: "installation", unit_id: unit.id, installation: { ...unit.installation, state: "queued" } }, trackingRevision)));
      if (unit.installation.state === "queued") actions.append(this.button("Mark installed", () => this.command({ type: "installation", unit_id: unit.id, installation: { ...unit.installation, state: "installed" } }, trackingRevision)));
      card.append(actions);
      const details = this.node("details", ""); details.append(this.node("summary", "", "Installation details"));
      this.installationForm(details, unit); card.append(details); grid.append(card);
    }
    root.append(grid);
    if (units.length > this.queueLimit) root.append(this.button("Show more installation pieces", async () => { this.queueLimit += 12; this.workspaceKey = null; await this.renderWorkspace(); }));
  }
  checkbox(label, checked) {
    const field = this.field(label, null, "", "checkbox");
    field.wrapper.classList.add("tracking-checkbox");
    field.wrapper.firstChild.prepend(field.input);
    field.input.checked = checked; return field;
  }
  number(label, value, optional = false) {
    const field = this.field(label, null, value ?? "", "number");
    field.input.step = "1"; field.input.required = !optional;
    return field;
  }
  renderSettings(force = false) {
    const root = document.getElementById("tracking-settings-content");
    if (!this.snapshot) return;
    this.retryPanel(root);
    if (!force && (root.contains(document.activeElement) || ((this.busy || this.pending) && root.querySelector("form")))) {
      this.retryPanel(root); this.renderDiagnostics(); return;
    }
    const revision = this.snapshot.tracking_revision;
    const key = JSON.stringify([this.snapshot.configuration_version, this.snapshot.inventory, this.online, this.pending?.key]);
    if (!force && this.settingsKey === key) { this.renderDiagnostics(); return; }
    this.settingsKey = key;
    root.replaceChildren(); this.retryPanel(root);
    const config = structuredClone(this.snapshot.config);
    const form = this.node("form", "tracking-settings-form");
    const groups = this.node("div", "tracking-settings-groups");
    const areas = this.node("section", "tracking-settings-section");
    areas.append(this.node("h3", "", "Areas"), this.node("p", "muted", "Add the places you keep clothes, then connect their antennas."));
    const areaCards = this.node("div", "tracking-config-list"); areas.append(areaCards);
    const readers = this.node("details", "tracking-settings-section tracking-readers");
    readers.append(this.node("summary", "", "Reader connections"), this.node("p", "muted", "A reader can serve antennas in several areas. Its connection is shared."));
    const readerCards = this.node("div", "tracking-config-list"); readers.append(readerCards);
    const ports = this.node("section", "tracking-settings-section");
    ports.append(this.node("h3", "", "Tag pairing"), this.node("p", "muted", "Use a separate antenna for pairing tags, or borrow an area antenna from the Tags page."));
    const enrollmentCards = this.node("div", "tracking-config-list"); ports.append(enrollmentCards);
    const areaFields = [];
    const readerFields = [];
    const portFields = [];
    const nullable = (field) => field.input.value === "" ? null : Number(field.input.value);
    const advancedId = (kind, field) => {
      const details = this.node("details", "tracking-identifiers");
      details.append(this.node("summary", "", `Advanced ${kind} settings`), field.wrapper);
      return details;
    };
    // A connection has one editor, even when its antennas span several areas.
    // Keep it with the first referring position, or with unused connections.
    const placeReaders = () => {
      for (const reader of readerFields) {
        const position = portFields.find((entry) => entry.reader.input.value === reader.id.input.id);
        const home = position?.readerSettings || readerCards;
        if (reader.block.parentElement !== home) home.append(reader.block);
      }
      for (const position of portFields) position.readerSettings.hidden = !position.readerSettings.querySelector("fieldset");
      readers.hidden = readerCards.childElementCount === 0;
    };
    const editReader = (reader) => {
      const position = portFields.find((entry) => entry.reader.input.value === reader.id.input.id);
      (position?.readerSettings || readers).open = true;
      reader.label.input.focus();
    };
    const placePosition = (position) => {
      const area = areaFields.find((entry) => entry.id.input.id === position.area.input.value);
      (area?.positions || enrollmentCards).append(position.block);
      position.area.wrapper.hidden = Boolean(area);
    };
    const removePosition = (position) => {
      portFields.splice(portFields.indexOf(position), 1); placeReaders();
      position.block.remove(); refreshPositions();
    };
    const refreshPositions = (changed = null, choose = false) => {
      for (const position of portFields) {
        const reader = readerFields.find((entry) => entry.id.input.id === position.reader.input.value);
        position.editReader.textContent = reader ? "Edit reader connection" : "Connect a reader";
        const original = reader?.originalConnection;
        const sameConnection = original?.type === reader?.transport.input.value && (original?.type === "impinj_llrp" ? original.host === reader.host.input.value.trim() && original.port === Number(reader.connectionPort.input.value) : original?.type === "impinj_iot" && original.base_url === reader.origin.input.value.trim());
        const readback = sameConnection && this.snapshot.reader_readbacks.find((entry) => entry.reader_id === reader.originalId);
        const supported = readback ? [...new Set(readback.capabilities.antenna_ports)].sort((a, b) => a - b) : null;
        const used = new Set(portFields.filter((entry) => entry !== position && entry.reader.input.value === position.reader.input.value && entry.antenna.input.value !== "").map((entry) => Number(entry.antenna.input.value)));
        const available = supported?.filter((port) => !used.has(port));
        if (choose && position === changed) position.antenna.input.value = available?.[0] ?? "";
        const value = position.antenna.input.value, port = Number(value);
        let error = "";
        if (value && used.has(port)) error = "This antenna port already has a position. Choose an unused port.";
        else if (value && supported && !supported.includes(port)) error = "This antenna port is not reported by the selected reader.";
        position.antenna.input.setCustomValidity(error);
        position.hint.textContent = error || !reader ? error || "Choose a reader, then an antenna port." : supported === null ? "No reader readback is available. Enter an antenna port verified on this reader." : !available.length && !value ? "All reported antenna ports already have positions. Choose another reader or remove an unused draft position." : `Reported ports: ${supported.join(", ")}.${available.length ? ` Unused: ${available.join(", ")}.` : ""}`;
      }
    };
    // Select values identify draft rows. A duplicate editable domain ID must
    // never retarget a position that selected a different area or reader.
    const updateReferences = () => {
      for (const position of portFields) {
        for (const [select, records, empty] of [[position.reader.input, readerFields, "Choose a reader"], [position.area.input, areaFields, "No area (enrollment only)"]]) {
          const value = select.value;
          select.replaceChildren(new Option(empty, ""), ...records.map((record) => new Option(record.label.input.value.trim() || "Unnamed", record.id.input.id)));
          if (records === readerFields) select.add(new Option("Connect another reader…", "new-reader"));
          select.value = value;
          if (select.selectedIndex < 0) select.value = "";
        }
      }
      placeReaders(); refreshPositions();
    };
    const referencedId = (field, records) => records.find((record) => record.id.input.id === field.input.value)?.id.input.value.trim() || null;
    const addArea = (area) => {
      const block = this.node("fieldset", "tracking-config-card"); block.append(this.node("legend", "", "Area"));
      const id = this.field("Area ID", null, area.id); id.input.pattern = "[A-Za-z0-9_-]{1,64}"; id.input.required = true;
      const label = this.field("Area label", null, area.label); label.input.required = true; label.input.maxLength = 120;
      const kind = this.field("Area purpose", Object.entries(areaLabels), area.kind);
      const active = this.checkbox("Area enabled", area.enabled);
      const overrides = this.node("details", ""); overrides.append(this.node("summary", "", "Optional area evidence overrides"));
      const thresholds = {};
      for (const [key, title] of [["positive_dwell_ms", "Presence dwell (ms)"], ["positive_min_reads", "Minimum reads"], ["movement_hysteresis_ms", "Movement hysteresis (ms)"], ["absence_ms", "Confirmed absence (ms)"]]) {
        const field = this.number(title, area.thresholds?.[key], true); field.input.min = key === "positive_min_reads" ? "1" : "0";
        thresholds[key] = field; overrides.append(field.wrapper);
      }
      const positions = this.node("div", "tracking-config-list");
      const antennas = this.node("div", "tracking-area-antennas");
      antennas.append(this.node("h4", "", "Antennas"), positions, this.button("Add antenna", () => addPort({ id: `position-${crypto.randomUUID()}`, reader_id: "", antenna: null, area_id: null, role: "monitor", enabled: true, required_for_absence: true, commissioned: false }, true, fields)));
      block.append(label.wrapper, kind.wrapper, active.wrapper, overrides, advancedId("area", id), antennas);
      const fields = { block, id, label, kind, active, thresholds, positions }; areaFields.push(fields);
      label.input.addEventListener("input", updateReferences);
      block.append(this.button("Remove area", () => {
        for (const position of [...portFields]) if (position.area.input.value === id.input.id) removePosition(position);
        block.remove(); areaFields.splice(areaFields.indexOf(fields), 1); updateReferences();
      })); areaCards.append(block); updateReferences();
    };
    for (const area of config.areas) addArea(area);
    areas.append(this.button("Add area", () => addArea({ id: `area-${crypto.randomUUID()}`, label: "", kind: "closet", enabled: true, thresholds: {} })));
    const addReader = (reader) => {
      const block = this.node("fieldset", "tracking-config-card"); block.append(this.node("legend", "", "Reader"));
      const id = this.field("Reader ID", null, reader.id); id.input.pattern = "[A-Za-z0-9_-]{1,64}"; id.input.required = true;
      const label = this.field("Reader label", null, reader.label); label.input.required = true; label.input.maxLength = 120;
      const active = this.checkbox("Reader enabled", reader.enabled);
      const transport = this.field("Reader transport", [["impinj_llrp", "Impinj LLRP"], ["impinj_iot", "Impinj IoT API"], ["simulator", "Simulator (not physical evidence)"]], reader.connection.type);
      const host = this.field("Reader host", null, reader.connection.host); host.input.maxLength = 253;
      const connectionPort = this.number("Reader port", reader.connection.port ?? 5084); connectionPort.input.min = "1"; connectionPort.input.max = "65535";
      const program = this.number("Closet-owned ROSpec ID", reader.connection.owned_rospec_id ?? 1); program.input.min = "1"; program.input.max = "4294967295";
      const origin = this.field("Reader API origin", null, reader.connection.base_url, "url"); origin.input.pattern = "https://.*";
      const auth = this.field("Reader authentication", [["none", "None"], ["basic_env", "Environment-backed basic authentication"]], reader.connection.credentials?.type || "none");
      const username = this.field("Username environment variable", null, reader.connection.credentials?.username);
      const password = this.field("Password environment variable", null, reader.connection.credentials?.password);
      for (const field of [username, password]) { field.input.pattern = "[A-Z_][A-Z0-9_]*"; field.input.maxLength = 128; }
      const credentialHint = this.node("p", "muted", "Set credentials in the app environment. Enter only the environment variable names here.");
      const update = () => { host.wrapper.hidden = connectionPort.wrapper.hidden = program.wrapper.hidden = transport.input.value !== "impinj_llrp"; host.input.required = connectionPort.input.required = program.input.required = !host.wrapper.hidden; origin.wrapper.hidden = transport.input.value !== "impinj_iot"; auth.wrapper.hidden = origin.wrapper.hidden; username.wrapper.hidden = password.wrapper.hidden = origin.wrapper.hidden || auth.input.value !== "basic_env"; origin.input.required = !origin.wrapper.hidden; username.input.required = password.input.required = !username.wrapper.hidden; credentialHint.hidden = username.wrapper.hidden; };
      transport.input.addEventListener("change", update); auth.input.addEventListener("change", update); update();
      const advanced = advancedId("reader", id); advanced.append(connectionPort.wrapper, program.wrapper);
      block.append(label.wrapper, active.wrapper, transport.wrapper, host.wrapper, origin.wrapper, auth.wrapper, username.wrapper, password.wrapper, credentialHint, advanced);
      const fields = { block, id, label, active, transport, host, connectionPort, program, origin, auth, username, password, originalId: reader.id, originalConnection: structuredClone(reader.connection) }; readerFields.push(fields);
      label.input.addEventListener("input", updateReferences);
      for (const field of [transport, host, connectionPort, origin]) field.input.addEventListener("input", () => refreshPositions());
      block.append(this.button("Remove reader", () => { block.remove(); readerFields.splice(readerFields.indexOf(fields), 1); updateReferences(); })); readerCards.append(block); updateReferences();
      return fields;
    };
    for (const reader of config.readers) addReader(reader);
    readers.append(this.button("Add reader", () => addReader({ id: `reader-${crypto.randomUUID()}`, label: "", enabled: true, connection: { type: "impinj_llrp", credentials: { type: "none" } } })));
    const addPort = (port, isNew = false, areaContext = null) => {
      const block = this.node("fieldset", "tracking-config-card"); block.append(this.node("legend", "", "Antenna position"));
      const id = this.field("Position ID", null, port.id); id.input.pattern = "[A-Za-z0-9_-]{1,64}"; id.input.required = true;
      const selectedReader = readerFields.find((entry) => entry.id.input.value === port.reader_id) || (isNew && readerFields.length === 1 ? readerFields[0] : null);
      const reader = this.field("Reader for this position", [["", "Choose a reader"], ...readerFields.map((entry) => [entry.id.input.id, entry.label.input.value || "Unnamed reader"]), ["new-reader", "Connect another reader…"]], selectedReader?.id.input.id || ""); reader.input.required = true;
      const antenna = this.number("Antenna port", port.antenna); antenna.input.min = "1";
      const hint = this.node("p", "muted antenna-hint"); hint.id = `${antenna.input.id}-hint`;
      antenna.input.setAttribute("aria-describedby", hint.id); hint.setAttribute("role", "status");
      const area = this.field("Area for this position", [["", "No area (enrollment only)"], ...areaFields.map((entry) => [entry.id.input.id, entry.label.input.value || "Unnamed area"])], areaContext?.id.input.id || areaFields.find((entry) => entry.id.input.value === port.area_id)?.id.input.id || "");
      const role = this.field("Position role", [["monitor", "Area monitoring"], ["enrollment", "Isolated enrollment position"]], port.role);
      const active = this.checkbox("Position enabled", port.enabled);
      const required = this.checkbox("Required for confirmed absence", port.required_for_absence);
      const commissioned = this.checkbox("Coverage tested", port.commissioned);
      const power = this.number("Requested transmit power (hundredths of dBm, optional)", port.transmit_power_cdbm, true);
      const dwell = this.number("Requested antenna dwell (ms, optional)", port.dwell_ms, true); dwell.input.min = "1";
      const updateRole = () => {
        const enrollment = role.input.value === "enrollment";
        area.input.required = !enrollment;
        if (enrollment) required.input.checked = false;
        commissioned.wrapper.hidden = enrollment;
        required.input.disabled = enrollment || this.busy || Boolean(this.pending) || this.role() !== "owner";
      };
      role.input.addEventListener("change", updateRole); updateRole();
      const connection = this.button("Connect a reader", () => {
        const selected = readerFields.find((entry) => entry.id.input.id === reader.input.value);
        const record = selected || addReader({ id: `reader-${crypto.randomUUID()}`, label: "", enabled: true, connection: { type: "impinj_llrp", credentials: { type: "none" } } });
        reader.input.value = record.id.input.id; placeReaders(); refreshPositions(fields, !selected); editReader(record);
      });
      const readerSettings = this.node("details", "tracking-reader-settings");
      readerSettings.append(this.node("summary", "", "Reader connection"));
      const advanced = advancedId("position", id);
      advanced.append(role.wrapper, active.wrapper, required.wrapper, power.wrapper, dwell.wrapper);
      block.append(reader.wrapper, connection, readerSettings, antenna.wrapper, hint, area.wrapper, commissioned.wrapper, advanced);
      const fields = { block, id, reader, antenna, hint, area, role, active, required, commissioned, power, dwell, editReader: connection, readerSettings }; portFields.push(fields);
      reader.input.addEventListener("change", () => {
        if (reader.input.value === "new-reader") connection.click();
        placeReaders(); refreshPositions(fields, true);
      });
      antenna.input.addEventListener("input", () => refreshPositions());
      area.input.addEventListener("change", () => placePosition(fields));
      block.append(this.button("Remove position", () => removePosition(fields))); placePosition(fields);
      placeReaders(); refreshPositions(fields, isNew);
    };
    for (const port of config.ports) addPort(port);
    ports.append(this.button("Add pairing antenna", () => addPort({ id: `position-${crypto.randomUUID()}`, reader_id: "", antenna: null, area_id: null, role: "enrollment", enabled: true, required_for_absence: false, commissioned: false }, true)));
    placeReaders(); groups.append(areas, ports, readers); form.append(groups);
    const thresholds = this.node("details", ""); thresholds.append(this.node("summary", "", "Default evidence thresholds"));
    const thresholdFields = {};
    const titles = { positive_dwell_ms: "Presence dwell (ms)", positive_min_reads: "Minimum reads", movement_hysteresis_ms: "Movement hysteresis (ms)", absence_ms: "Confirmed absence (ms)", warmup_ms: "Restart/configuration warm-up (ms)", coverage_max_gap_ms: "Maximum coverage gap (ms)", candidate_fresh_ms: "Enrollment candidate freshness (ms)", reader_event_max_age_ms: "Maximum reader event age (ms)" };
    for (const [key, title] of Object.entries(titles)) { const field = this.number(title, config.thresholds[key]); field.input.min = "1"; thresholdFields[key] = field; thresholds.append(field.wrapper); }
    form.append(thresholds);
    const save = this.button("Save tracking settings", () => {
      if (!form.reportValidity()) return;
      const next = {
        schema_version: config.schema_version,
        areas: areaFields.map((f) => ({ id: f.id.input.value.trim(), label: f.label.input.value.trim(), kind: f.kind.input.value, enabled: f.active.input.checked, thresholds: Object.fromEntries(Object.entries(f.thresholds).map(([key, field]) => [key, nullable(field)])) })),
        readers: readerFields.map((f) => ({ id: f.id.input.value.trim(), label: f.label.input.value.trim(), enabled: f.active.input.checked, connection: f.transport.input.value === "simulator" ? { type: "simulator" } : f.transport.input.value === "impinj_llrp" ? { type: "impinj_llrp", host: f.host.input.value.trim(), port: Number(f.connectionPort.input.value), owned_rospec_id: Number(f.program.input.value) } : { type: "impinj_iot", base_url: f.origin.input.value.trim(), credentials: f.auth.input.value === "none" ? { type: "none" } : { type: "basic_env", username: f.username.input.value.trim(), password: f.password.input.value.trim() } } })),
        ports: portFields.map((f) => ({ id: f.id.input.value.trim(), reader_id: referencedId(f.reader, readerFields) || "", antenna: Number(f.antenna.input.value), area_id: referencedId(f.area, areaFields), role: f.role.input.value, enabled: f.active.input.checked, required_for_absence: f.required.input.checked, commissioned: f.commissioned.input.checked, transmit_power_cdbm: nullable(f.power), dwell_ms: nullable(f.dwell) })),
        thresholds: Object.fromEntries(Object.entries(thresholdFields).map(([key, field]) => [key, Number(field.input.value)])),
      };
      void this.command({ type: "configure", config: next }, revision);
    }, false);
    form.addEventListener("submit", (event) => { event.preventDefault(); save.click(); }); form.append(save);
    if (this.role() !== "owner") { for (const input of form.querySelectorAll("input,select,button")) input.disabled = true; }
    root.append(form);
    const checklist = this.node("details", "tracking-settings-support"); checklist.append(this.node("summary", "", "Test reader coverage"), this.node("p", "muted", "Test each antenna with actual tags and garments before marking it commissioned. A connection alone does not prove an empty area was scanned."));
    const steps = this.node("ol", "");
    for (const text of ["Confirm the exact region, reader, antenna, cables and supported power with the hardware documentation.", "Test representative garment tags and orientations on each antenna, including crowded racks and wet laundry.", "Measure intended coverage and unwanted adjacent-area reads. Tune placement, dwell and power without assuming RSSI is distance.", "Verify empty-area continuous inventory proof, disconnection, recovery and warm-up before marking a position commissioned.", "Test multi-tag enrollment, a bound ambient tag, replacement and isolation before a batch."]) steps.append(this.node("li", "", text));
    checklist.append(steps); root.append(checklist);
    const reconcile = this.snapshot.inventory.filter((record) => record.needs_reconciliation && !record.archived);
    for (const record of reconcile) {
      const block = this.node("div", "notice"); block.append(this.node("p", "", `Physical count needs review: ${record.catalog_id}. Source quantity: ${record.catalog_quantity ?? "not recorded"}; active tracked pieces: ${record.active_units}.`));
      if (record.capacity_limited) {
        block.append(this.node("p", "muted", `New allocations are limited to 20,000 active tracking units. ${record.unallocated_units} physical pieces have not been allocated. The authoritative catalog and its quantities are unchanged. Review obsolete tracking units before trying another allocation.`));
        if (record.catalog_quantity !== null || record.declared_quantity !== null) block.append(this.button("Retry tracking allocation", () => this.command({ type: "reconcile_units", catalog_id: record.catalog_id }, revision)));
      } else if (record.catalog_quantity === null) {
        const count = this.number("Confirmed physical count", record.declared_quantity ?? record.active_units); count.input.min = "1"; count.input.max = "20000";
        block.append(count.wrapper, this.button("Confirm physical count", () => { if (count.input.reportValidity()) void this.command({ type: "declare_unit_count", catalog_id: record.catalog_id, count: Number(count.input.value) }, revision); }));
      } else if (record.active_units < record.catalog_quantity) {
        block.append(this.button("Restore missing physical units", () => this.command({ type: "reconcile_units", catalog_id: record.catalog_id }, revision)));
      } else {
        block.append(this.node("p", "muted", "Review the individual pieces and retire any extra tracking units. The source quantity cannot be overridden here."));
      }
      block.append(this.button("Review physical pieces", async () => { try { this.openItem(await this.item(record.catalog_id)); } catch (error) { this.notice(this.errorMessage(error), true); } }));
      root.append(block);
    }
    const backup = this.node("details", "tracking-settings-support"); backup.append(this.node("summary", "", "Tracking backup and recovery"), this.node("p", "muted", "Exports contain private tag associations and movement history. Keep them private. Catalog metadata is not changed by a tracking import."));
    backup.append(this.button("Download tracking export", async () => {
      try {
        const data = await this.request("v1/tracking/export"); const url = URL.createObjectURL(new Blob([JSON.stringify(data)], { type: "application/json" }));
        const link = this.node("a", "", ""); link.href = url; link.download = "closet-tracking.json"; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000);
      } catch (error) { this.notice(this.errorMessage(error), true); }
    }));
    const file = this.field("Restore tracking export", null, "", "file"); file.input.accept = ".json,application/json";
    const importButton = this.button("Restore selected export", async () => {
      const selected = file.input.files[0]; if (!selected) return;
      if (selected.size > 8 * 1024 * 1024) { this.notice("This export is too large for the Home Assistant bridge.", true); return; }
      try {
        const data = JSON.parse(await selected.text());
        const body = { type: "import", data };
        if (new TextEncoder().encode(JSON.stringify(body)).length > 8 * 1024 * 1024) { this.notice("This export is too large for the Home Assistant bridge.", true); return; }
        await this.command(body, revision);
      } catch { this.notice("The selected export is not valid JSON.", true); }
    }); backup.append(file.wrapper, importButton); root.append(backup);
    root.append(this.node("div", "tracking-diagnostics")); this.renderDiagnostics();
  }
  renderDiagnostics() {
    const root = document.querySelector(".tracking-diagnostics"); if (!root || !this.snapshot) return;
    const requested = Boolean(this.openDiagnosticsRequested);
    const open = requested || root.querySelector("details")?.open || false;
    this.openDiagnosticsRequested = false;
    const details = this.node("details", ""); details.open = open; details.append(this.node("summary", "", "Reader health and evidence diagnostics"));
    details.append(this.node("p", "muted", "Raw tag identifiers appear only in this diagnostic panel. Simulation is not physical commissioning evidence."));
    const list = this.node("ul", "");
    for (const port of this.snapshot.health.ports) {
      const config = this.snapshot.config.ports.find((entry) => entry.id === port.port_id);
      const reader = config && this.snapshot.config.readers.find((entry) => entry.id === config.reader_id);
      list.append(this.node("li", "", `${reader?.label || port.port_id}${config ? ` · antenna ${config.antenna}` : ""}: ${port.state.replaceAll("_", " ")}${port.last_coverage_at ? ` · coverage last received ${new Date(port.last_coverage_at).toLocaleTimeString()}` : " · no verified coverage"}`));
    }
    details.append(list);
    for (const readback of this.snapshot.reader_readbacks) {
      const reader = this.snapshot.config.readers.find((entry) => entry.id === readback.reader_id);
      const capabilities = readback.capabilities;
      details.append(this.node("h4", "", reader?.label || readback.reader_id), this.node("p", "muted", `Supported antennas: ${capabilities.antenna_ports.join(", ")}. Continuous inventory proof: ${capabilities.continuous_inventory_proof ? "supported" : "not verified"}. Inventory pause: ${capabilities.inventory_pause ? "supported" : "not verified"}.`));
      if (capabilities.min_power_cdbm !== null && capabilities.max_power_cdbm !== null) details.append(this.node("p", "muted", `Reported power range: ${capabilities.min_power_cdbm / 100}–${capabilities.max_power_cdbm / 100} dBm.`));
      for (const port of readback.ports) details.append(this.node("p", "muted", `Antenna ${port.antenna}: ${port.enabled ? "enabled" : "disabled"}${port.transmit_power_cdbm !== null ? ` · ${port.transmit_power_cdbm / 100} dBm` : ""}${port.dwell_ms !== null ? ` · ${port.dwell_ms} ms dwell` : ""}`));
    }
    const recent = this.node("ol", "tracking-history");
    for (const read of this.snapshot.diagnostics.recent_reads.slice(-30).reverse()) recent.append(this.node("li", "", `${new Date(read.received_at).toLocaleTimeString()} · ${read.data_source === "simulation" ? "SIMULATION" : "Live"} · ${read.purpose} · ${read.port_id} · EPC ${read.epc} · ${read.read_count} reads${read.peak_rssi_cdbm !== null ? ` · RSSI ${read.peak_rssi_cdbm / 100} dBm` : ""}`));
    details.append(this.node("h4", "", "Recent reads"), recent);
    for (const decision of this.snapshot.diagnostics.decisions.slice(-20).reverse()) details.append(this.node("p", "muted", `${new Date(decision.at).toLocaleTimeString()} · ${decision.reason.replaceAll("_", " ")}${decision.area_id ? ` · ${this.area(decision.area_id)?.label || decision.area_id}` : ""}`));
    for (const fault of this.snapshot.diagnostics.faults.slice(-20).reverse()) details.append(this.node("p", "notice", `${new Date(fault.at).toLocaleTimeString()} · ${fault.code.replaceAll("_", " ")}`));
    root.replaceChildren(details);
    if (requested) details.querySelector("summary").focus();
  }

}
