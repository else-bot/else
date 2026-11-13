// Page-side steps shared by else-browser and the CDP helpers. Each runs with
// `this` bound to the target element (Runtime.callFunctionOn or .call).
({
  // Where a person would click the element ({x, y}), or why they could not
  // ({error}): the point must hit the element itself, or for an input that
  // is visually hidden, a visible label. Scrolls it into view first. Given a
  // point, only checks that the point still hits the element: callers check
  // again after moving the pointer, since hovering can change the page.
  clickPoint(px, py) {
    const e = this;
    // Named by tag, id and class only: attributes such as value can hold secrets.
    const tag = (n) => `<${n.localName}${n.id ? ` id="${n.id.slice(0, 40)}"` : ""}${n.getAttribute("role") ? ` role="${n.getAttribute("role").slice(0, 20)}"` : ""}${typeof n.className === "string" && n.className.trim() ? ` class="${n.className.trim().slice(0, 60)}"` : ""}>`;
    if (!e.isConnected) return { error: "NOT_CLICKABLE: the element is no longer on the page" };
    if (e.matches(":disabled") || e.getAttribute("aria-disabled") === "true") return { error: "NOT_CLICKABLE: the element is disabled" };
    const box = (n) => getComputedStyle(n).visibility !== "hidden"
      && [...n.getClientRects()].find((r) => r.width > 1 && r.height > 1);
    // A styled checkbox or radio hides its input; a person clicks a label.
    const targets = box(e) ? [e] : [...(e.labels || [])].filter(box);
    if (!targets.length) return { error: "NOT_CLICKABLE: the element is hidden" };
    // Hit-test from the element's own tree, so a closed shadow root it is in
    // does not hide which of its elements is on top.
    const scope = e.getRootNode();
    const at = (x, y) => {
      let hit = (scope.elementFromPoint ? scope : document).elementFromPoint(x, y);
      while (hit && hit.shadowRoot) {
        const inner = hit.shadowRoot.elementFromPoint(x, y);
        if (!inner || inner === hit) break;
        hit = inner;
      }
      return hit;
    };
    // The point must land in a target, or in the shadow tree the target
    // itself draws, and not on another control inside it: a Terms link in a
    // label, a switch in a card, or a component with its own button.
    const controls = "a,button,input,select,textarea,summary,label,[contenteditable],[role=button],[role=link],"
      + "[role=checkbox],[role=radio],[role=switch],[role=tab],[role=menuitem],[role=menuitemcheckbox],"
      + "[role=menuitemradio],[role=option],[role=combobox],[role=textbox],[role=searchbox],[role=listbox],"
      + "[role=slider],[role=spinbutton],[role=treeitem],[role=gridcell],[onclick]";
    // A custom control: focusable, or shown clickable where its parent is not
    // (as computer_browser's snapshot finds them); a pointer over the text or
    // icon inside a button, link or label is that control's own.
    const control = (m) => m.matches(controls) || m.tabIndex >= 0 && m.hasAttribute("tabindex")
      || getComputedStyle(m).cursor === "pointer" && m.parentElement && getComputedStyle(m.parentElement).cursor !== "pointer"
        && !m.parentElement.closest("a,button,label,summary,[role=button],[role=link]");
    const owns = (n) => targets.some((t) => {
      let other = false, seen = false;
      for (let m = n; m; m = m.parentNode || m.host) {
        if (m === t) return !other && !seen;
        if (m.nodeType === 11) {
          // Leaving a shadow tree: its controls are the target's own parts
          // only when the target is its host.
          if (seen && m.host !== t) other = true;
          seen = false;
        } else if (m.nodeType === 1 && control(m)) {
          seen = true;
        }
      }
      return false;
    });
    const covered = (hit) => ({ error: `COVERED: ${hit ? tag(hit) : "nothing"} is on top of the element, so a click would land there; close or scroll past it, or take a screenshot` });
    if (px !== undefined) {
      const hit = at(px, py);
      return owns(hit) ? { x: px, y: py } : covered(hit);
    }
    const first = box(targets[0]);
    if (first.bottom <= 0 || first.right <= 0 || first.top >= innerHeight || first.left >= innerWidth) {
      targets[0].scrollIntoView({ block: "center", inline: "center" });
    }
    // The centre of the part of a rectangle inside the window.
    const centre = (q) => {
      const l = Math.max(q.left, 0), r = Math.min(q.right, innerWidth);
      const t = Math.max(q.top, 0), b = Math.min(q.bottom, innerHeight);
      return r - l > 1 && b - t > 1 ? [(l + r) / 2, (t + b) / 2] : null;
    };
    const points = [];
    for (const target of targets) {
      points.push(centre(box(target)));
      const walker = document.createTreeWalker(target, NodeFilter.SHOW_TEXT);
      for (let t = walker.nextNode(); t; t = walker.nextNode()) {
        const range = document.createRange();
        range.selectNodeContents(t);
        for (const q of range.getClientRects()) points.push(centre(q));
      }
    }
    const inside = points.filter(Boolean);
    if (!inside.length) return { error: "NOT_CLICKABLE: the element is outside the window" };
    const point = inside.find(([x, y]) => owns(at(x, y)));
    if (!point) return covered(at(...inside[0]));
    const [x, y] = point;
    return { x, y };
  },

  // The field to type into: the element, or the control of the label it is
  // in. Callers resolve it once and run every typing step on it, so the page
  // cannot point the label at another field halfway through.
  field() {
    const e = this;
    if (!e.matches("input,textarea,select") && !e.isContentEditable && e.closest("label")?.control) return e.closest("label").control;
    return e;
  },

  // Whether the field is a password. Callers ask before typing and pass the
  // answer to the steps below, because the page's handlers can change the
  // field's type while it takes the text.
  secret() {
    return this.type === "password";
  },

  // Get the field ready for `value`. A text field or editor is focused with
  // its content selected and returns {insert: true}: the caller checks it
  // still has focus with `focusedField`, types through Input.insertText
  // (which edits like a keyboard, so the page's input handlers run and may
  // refuse it), then confirms with `holds`. A select or a picker input
  // (date, color, ...) is set here with the prototype's setter, which
  // framework value trackers do not intercept, and returns {check: value}:
  // the caller confirms that value with `holds` in a later call, after the
  // page's handlers, including their queued microtasks, have run.
  typeInto(value) {
    const e = this;
    const fire = () => {
      e.dispatchEvent(new Event("input", { bubbles: true }));
      e.dispatchEvent(new Event("change", { bubbles: true }));
    };
    if (!e.isConnected) return { error: "the field is no longer on the page" };
    if (e.matches(":disabled") || e.readOnly) return { error: "the field is disabled or read-only" };
    if (e.tagName === "SELECT") {
      const norm = (s) => s.replace(/\s+/g, " ").trim();
      const options = [...e.options].filter((o) => !o.matches(":disabled"));
      const option = options.find((o) => o.value === value) || options.find((o) => norm(o.text) === norm(value));
      if (!option) return { error: `the select has no option ${JSON.stringify(value)}; options: ${[...e.options].slice(0, 20).map((o) => JSON.stringify(norm(o.text)) + (o.matches(":disabled") ? " (disabled)" : "")).join(", ")}` };
      // Kept as plain values: the page's handlers can change the option
      // itself. Selected by index, since several options can share a value.
      const want = option.value, index = option.index;
      e.focus();
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "selectedIndex").set.call(e, index);
      fire();
      return { check: want, index };
    }
    if (e.tagName === "INPUT" && /^(checkbox|radio|file|submit|button|image|reset|hidden)$/.test(e.type)) return { error: `a ${e.type} input is not a text field; click it instead` };
    if (e.tagName === "INPUT" && /^(date|time|datetime-local|month|week|color|range)$/.test(e.type)) {
      e.focus();
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(e, value);
      fire();
      return { check: value };
    }
    if (!e.matches("input,textarea") && !e.isContentEditable) return { error: "it is not a text field" };
    e.focus();
    if (e.isContentEditable) {
      const range = document.createRange();
      range.selectNodeContents(e);
      getSelection().removeAllRanges();
      getSelection().addRange(range);
    } else {
      e.select();
    }
    return { insert: true };
  },

  // Whether focus is on the field typeInto prepared, checked in its own call
  // just before the text goes in: typed text goes wherever focus is.
  // When it is, also cancels the coming edit if the page's own beforeinput
  // handlers move focus or the selection elsewhere first.
  focusedField() {
    const e = this;
    const onTarget = () => {
      const active = e.getRootNode().activeElement;
      // In an editor, focus sits on its editing host, which may contain the
      // element; a control inside the element has its own focus and does
      // not count.
      if (active !== e && !(e.isContentEditable && !!active?.isContentEditable && active.contains(e))) return false;
      // In an editor the text goes to the selection, which must still
      // cover only this element.
      if (!e.isContentEditable) return true;
      const sel = e.getRootNode().getSelection ? e.getRootNode().getSelection() : getSelection();
      return !!sel && sel.rangeCount > 0 && e.contains(sel.anchorNode) && e.contains(sel.focusNode);
    };
    if (!onTarget()) return false;
    // Listens last (bubbling on window), after the page's handlers.
    // holds removes it, so it never touches later input.
    const guard = (event) => { if (!onTarget()) event.preventDefault(); };
    const key = Symbol.for("else.typingGuard");
    if (window[key]) removeEventListener("beforeinput", window[key]);
    window[key] = guard;
    addEventListener("beforeinput", guard, { once: true });
    setTimeout(() => removeEventListener("beforeinput", guard), 2000);
    return true;
  },

  // {done: true} when the field is still on the page and holds exactly
  // `value` (for a select, the option at `index`), else {error}. With
  // `secret` (from `secret`), errors give only the length.
  holds(value, secret, index) {
    const e = this;
    removeEventListener("beforeinput", window[Symbol.for("else.typingGuard")]);
    if (!e.isConnected) return { error: "the page replaced the field while typing, so the text did not stay in it" };
    if (index != null && (e.selectedIndex !== index || e.options[index].matches(":disabled"))) {
      return { error: `the select has ${JSON.stringify(e.options[e.selectedIndex]?.text.trim() ?? "nothing")} selected; the page changed or refused the option` };
    }
    const norm = (s) => s.replace(/[\u200b\ufeff]/g, "").replace(/\s+/g, " ").trim();
    const now = e.isContentEditable ? e.innerText : e.value;
    if (e.isContentEditable ? norm(now) === norm(value) : now === value) return { done: true };
    const held = secret ? `${now.length} characters` : JSON.stringify(now.slice(0, 200));
    return { error: `the field holds ${held} after typing; the page changed or refused the text` };
  },
})
