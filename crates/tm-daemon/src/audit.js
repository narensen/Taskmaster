// The free UI checks, run in the page under test by audit.rs over the DevTools protocol.
// Each check returns [check, text] pairs. The text is written for the agent that repairs
// the page: what is wrong, where, and what to do instead.
(() => {
  const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
  const seen = (el) => {
    if (!el || !el.isConnected) return false;
    const style = getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") return false;
    const box = el.getBoundingClientRect();
    if (box.width <= 0 || box.height <= 0) return false;
    for (let up = el; up; up = up.parentElement) if (Number(getComputedStyle(up).opacity) === 0) return false;
    return true;
  };
  const words = (el) => (el.innerText || el.value || el.getAttribute("aria-label") || "").trim().replace(/\s+/g, " ").slice(0, 32);
  const name = (el) => {
    let out = "<" + el.tagName.toLowerCase();
    if (el.id) out += "#" + el.id;
    else if (el.classList.length) out += "." + el.classList[0];
    if (el.tagName === "INPUT") out += " type=" + (el.getAttribute("type") || "text");
    out += ">";
    const said = words(el);
    return said ? `${out} "${said}"` : out;
  };
  // Words a person can see in an element: text whose box is not a screen-reader-only 1px.
  const showsWords = (el) => {
    const walk = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    for (let node = walk.nextNode(); node; node = walk.nextNode()) {
      if (!/\p{L}{2}/u.test(node.textContent || "")) continue;
      const box = node.parentElement.getBoundingClientRect();
      if (box.width > 2 && box.height > 2) return true;
    }
    return false;
  };
  const labelled = (el) =>
    (el.labels && el.labels.length > 0) ||
    !!el.getAttribute("aria-label")?.trim() ||
    !!el.getAttribute("aria-labelledby") ||
    !!el.getAttribute("title")?.trim();
  const TYPED = ["", "text", "search", "email", "url", "tel"];
  const ITEMS = "ul > li, ol > li, [role=list] > [role=listitem], tbody > tr";
  const items = () => [...document.querySelectorAll(ITEMS)].filter((el) => !el.closest("nav, header, footer") && seen(el)).length;
  const EMPTY =
    /\b(no (tasks?|todos?|to-dos?|items?|entries|notes?|results?|cards?|records?|expenses?|events?|books?|recipes?|contacts?|messages?|habits?|goals?|reminders?|links?|bookmarks?|transactions?)( yet| here| to show| found)?\b|nothing (to do|here|yet|to show)|list is empty|is empty|add your first|get started by|all caught up|all clear)/i;

  // The text field a person adds things with: in a form, or beside an Add button.
  const adder = () => {
    const fields = [...document.querySelectorAll("input")].filter(
      (el) => seen(el) && !el.disabled && !el.readOnly && TYPED.includes((el.getAttribute("type") || "").toLowerCase()),
    );
    const inForm = fields.find((el) => el.form);
    if (inForm) return { input: inForm, form: inForm.form };
    for (const input of fields) {
      for (let box = input.parentElement, up = 0; box && up < 3; box = box.parentElement, up++) {
        const button = [...box.querySelectorAll("button, [role=button], input[type=submit]")].find(
          (b) => seen(b) && /add|create|save|new|submit|\+/i.test(words(b)),
        );
        if (button) return { input, button };
      }
    }
    return null;
  };
  const type = (input, text) => {
    const proto = input instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(input, text);
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const send = async (add, text) => {
    type(add.input, text);
    if (add.form) {
      if (add.form.requestSubmit) add.form.requestSubmit();
      else add.form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    } else add.button.click();
    await sleep(350);
  };

  window.__tmAudit = {
    // Blank input, HTML in user text, and the empty state once the list has an item.
    async probe() {
      const add = adder();
      if (!add) return { ran: false, found: [] };
      const found = [];
      const before = items();
      await send(add, "   ");
      const blank = items();
      if (blank > before) found.push(["blank_input", `submitting a blank ${name(add.input)} added an item: ignore input that is empty after trim()`]);
      window.__tmProbe = 0;
      await send(add, '<b data-tm-probe="1">Probe</b><img src="x" onerror="window.__tmProbe=1">');
      await sleep(150);
      if (document.querySelector("[data-tm-probe]") || window.__tmProbe === 1) {
        found.push(["html_escape", `text typed into ${name(add.input)} was put on the page as HTML (its <b> and <img onerror> ran): show user text with textContent, never innerHTML`]);
      }
      let after = items();
      if (after <= blank) {
        await send(add, "Probe item");
        after = items();
      }
      if (after > blank) {
        const shown = [...document.querySelectorAll("body *")].filter((el) => {
          const text = el.innerText || "";
          return text.length < 160 && el.children.length <= 3 && !el.closest(ITEMS) && EMPTY.test(text) && seen(el);
        });
        const inner = shown.filter((el) => !shown.some((other) => other !== el && el.contains(other)));
        if (inner.length) {
          found.push([
            "empty_state",
            `the empty-state message ${name(inner[0])} is still shown while the list has ${after} item(s): hide it as soon as there is an item (an element with [hidden] also needs display: none when a class sets display)`,
          ]);
        }
      }
      return { ran: true, added: after > before, found };
    },

    // Wait briefly for in-DOM <img> elements to finish loading so real network images are not
    // misreported as broken while still in flight.
    async waitImages(timeoutMs = 4000) {
      const imgs = [...document.querySelectorAll("img")].filter((img) => img.getAttribute("src")?.trim());
      if (!imgs.length) return;
      const pending = imgs.filter((img) => !img.complete);
      if (!pending.length) return;
      await Promise.race([
        Promise.all(
          pending.map(
            (img) =>
              new Promise((done) => {
                img.addEventListener("load", done, { once: true });
                img.addEventListener("error", done, { once: true });
              }),
          ),
        ),
        sleep(timeoutMs),
      ]);
    },

    // A heading, labels, broken images, and form controls drawn at the design's size.
    scan() {
      const found = [];
      if (![...document.querySelectorAll("h1")].some(seen)) found.push(["h1", "the page has no visible <h1>: give it exactly one, naming the page"]);
      const brokenImgs = [];
      for (const img of document.querySelectorAll("img")) {
        const style = getComputedStyle(img);
        if (style.display === "none" || style.visibility === "hidden") continue;
        const rawSrc = (img.getAttribute("src") || "").trim();
        if (!rawSrc || !img.complete || img.naturalWidth === 0 || img.naturalHeight === 0) {
          const alt = (img.getAttribute("alt") || "").trim();
          const label = alt ? `alt="${alt.slice(0, 40)}"` : name(img);
          const shownSrc = rawSrc ? rawSrc.slice(0, 120) : "(empty src)";
          brokenImgs.push(`${label} src="${shownSrc}"`);
        }
      }
      if (brokenImgs.length) {
        const sample = brokenImgs.slice(0, 4).join(", ");
        const extra = brokenImgs.length > 4 ? ` (+${brokenImgs.length - 4} more)` : "";
        found.push([
          "images",
          `BROKEN_IMAGE: ${brokenImgs.length} <img> element(s) failed to load (naturalWidth=0): ${sample}${extra}. Replace with verified HTTP-200 real image URLs or local image files in public/.`,
        ]);
      }
      for (const el of document.querySelectorAll("input, select, textarea")) {
        const kind = (el.getAttribute("type") || "").toLowerCase();
        if (["hidden", "submit", "button", "reset", "image"].includes(kind) || !seen(el) || labelled(el)) continue;
        found.push(["label", `${name(el)} has no label: give it a <label for> or an aria-label`]);
        break;
      }
      for (const el of document.querySelectorAll("button, [role=button]")) {
        if (!seen(el) || /[\p{L}\p{N}]/u.test((el.innerText || "").trim()) || labelled(el)) continue;
        found.push(["label", `${name(el)} shows no words and has no aria-label: say what it does`]);
        break;
      }
      for (const el of document.querySelectorAll("input[type=checkbox], input[type=radio]")) {
        if (!seen(el)) continue;
        const style = getComputedStyle(el);
        if (style.appearance === "none") continue;
        const box = el.getBoundingClientRect();
        if (box.width > 30 || box.height > 30 || Math.abs(box.width - box.height) > 4) {
          found.push(["controls", `${name(el)} is a native ${el.type} stretched to ${Math.round(box.width)}x${Math.round(box.height)}px: size it on its own (18-20px), not with the rule for text inputs`]);
          break;
        }
        if (box.width < 14 && box.height < 14 && style.accentColor === "auto") {
          found.push(["controls", `${name(el)} is an unstyled native ${el.type} at the browser's ${Math.round(box.width)}px: size it to 18-20px and set accent-color`]);
          break;
        }
      }
      const body = parseFloat(getComputedStyle(document.body).fontSize);
      for (const el of document.querySelectorAll("input, button, select, textarea")) {
        const kind = (el.getAttribute("type") || "").toLowerCase();
        if (["checkbox", "radio", "hidden", "range", "color", "file"].includes(kind) || !seen(el)) continue;
        // An icon button's font size shows nowhere.
        if (el.tagName === "BUTTON" && !showsWords(el)) continue;
        const size = parseFloat(getComputedStyle(el).fontSize);
        if (Math.abs(size - 13.333) < 0.05 && Math.abs(body - 13.333) > 0.5) {
          found.push(["controls", `${name(el)} uses the browser's default 13.3px font: give form controls font: inherit`]);
          break;
        }
      }
      return found;
    },

    // Freeze transitions so a focus style is read as it ends up, not as it starts.
    still(on) {
      let tag = document.getElementById("tm-audit-still");
      if (on && !tag) {
        tag = document.createElement("style");
        tag.id = "tm-audit-still";
        tag.textContent = "*, *::before, *::after { transition: none !important; animation: none !important; }";
        document.head.appendChild(tag);
      } else if (!on && tag) tag.remove();
    },

    // What the element the keyboard just reached looks like focused and not.
    focused() {
      const el = document.activeElement;
      if (!el || el === document.body || el === document.documentElement) return null;
      const look = (s) =>
        [
          s.outlineStyle !== "none" && parseFloat(s.outlineWidth) > 0 ? `${s.outlineStyle} ${s.outlineWidth} ${s.outlineColor}` : "none",
          s.boxShadow,
          s.borderTopColor,
          s.borderBottomColor,
          s.backgroundColor,
          s.color,
          s.textDecorationLine,
        ].join("|");
      const on = look(getComputedStyle(el));
      el.blur();
      return { name: name(el), visible: on !== look(getComputedStyle(el)) };
    },

    // Sideways scroll at the width the page is being shown at.
    narrow() {
      const width = window.innerWidth;
      const over = document.documentElement.scrollWidth - width;
      if (over <= 1) return [];
      const wide = [...document.querySelectorAll("body *")].filter((el) => seen(el) && el.getBoundingClientRect().right > width + 1);
      const inner = wide.filter((el) => !wide.some((other) => other !== el && el.contains(other)));
      const worst = inner.sort((a, b) => b.getBoundingClientRect().right - a.getBoundingClientRect().right)[0];
      const where = worst ? `; ${name(worst)} reaches ${Math.round(worst.getBoundingClientRect().right)}px` : "";
      return [["narrow", `at ${width}px wide the page scrolls sideways by ${Math.round(over)}px${where}: let it shrink (max-width: 100%, flex-wrap, min-width: 0, overflow-wrap: anywhere)`]];
    },

    // Corrupt everything the page saved, and keep the real values to put back.
    corrupt() {
      const saved = {};
      for (let i = 0; i < localStorage.length; i++) {
        const key = localStorage.key(i);
        saved[key] = localStorage.getItem(key);
      }
      for (const key of Object.keys(saved)) localStorage.setItem(key, "{not json");
      return saved;
    },
    restore(saved) {
      localStorage.clear();
      for (const [key, value] of Object.entries(saved)) localStorage.setItem(key, value);
    },
  };
  return true;
})();
