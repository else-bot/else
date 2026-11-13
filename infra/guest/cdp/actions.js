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
})
