const MICE_HIGHLIGHT_ID = "mice-browser-guide-highlight";
const MAX_SNAPSHOT_ELEMENTS = 100;
const MAX_SNAPSHOT_LABEL_CHARS = 140;
const MAX_RANKING_ELEMENTS = 500;
// Elements whose text is longer than this are containers (whole cards), not
// atomic controls; the pointer sweep skips them to avoid giant labels.
const MAX_POINTER_TEXT_CHARS = 200;
// Standard interactive tags plus ARIA widget roles and focusable/clickable
// hints. Covers most well-built pages without a full DOM sweep.
const EXPLICIT_INTERACTIVE_SELECTOR =
  "a,button,input,select,textarea," +
  "[role='button'],[role='link'],[role='menuitem'],[role='option'],[role='tab']," +
  "[role='checkbox'],[role='radio'],[role='switch'],[onclick],[tabindex]";
// Bounded fallback sweep for app UIs (e.g. Canva) whose clickable tiles are
// <div>/<span> with no role: cap how many nodes we inspect and collect.
const MAX_POINTER_SCAN = 3000;
const MAX_POINTER_CANDIDATES = 160;

function visible(element) {
  const style = window.getComputedStyle(element);
  const rect = element.getBoundingClientRect();
  return style.visibility !== "hidden" && style.display !== "none" && rect.width > 0 && rect.height > 0;
}

function isUniqueSelector(selector) {
  try {
    return document.querySelectorAll(selector).length === 1;
  } catch {
    return false;
  }
}

function structuralSelector(element) {
  const parts = [];
  let current = element;
  while (current instanceof Element) {
    const tagName = current.localName;
    const siblings = current.parentElement
      ? [...current.parentElement.children].filter((sibling) => sibling.localName === tagName)
      : [];
    const position = siblings.indexOf(current) + 1;
    parts.unshift(siblings.length > 1 ? `${tagName}:nth-of-type(${position})` : tagName);
    if (current === document.documentElement) break;
    current = current.parentElement;
  }
  return parts.join(" > ");
}

function selectorFor(element) {
  if (element.id) {
    const selector = `#${CSS.escape(element.id)}`;
    if (isUniqueSelector(selector)) return selector;
  }
  const testId = element.getAttribute("data-testid");
  if (testId) {
    const selector = `[data-testid="${CSS.escape(testId)}"]`;
    if (isUniqueSelector(selector)) return selector;
  }
  const role = element.getAttribute("role");
  const label = element.getAttribute("aria-label");
  if (role && label) {
    const selector = `[role="${CSS.escape(role)}"][aria-label="${CSS.escape(label)}"]`;
    if (isUniqueSelector(selector)) return selector;
  }
  return structuralSelector(element);
}

function cappedText(value, maximumCharacters) {
  // Collapse whitespace so multi-line card/container text becomes a compact
  // single-line label instead of a huge blob that bloats the observation.
  const collapsed = value.replace(/\s+/g, " ").trim();
  return Array.from(collapsed).slice(0, maximumCharacters).join("");
}

function elementLabel(element) {
  const associatedLabel = element.labels?.[0]?.innerText || "";
  return cappedText(
    element.getAttribute("aria-label")
      || element.getAttribute("placeholder")
      || associatedLabel
      || element.innerText
      || element.title
      || element.value
      || "",
    MAX_SNAPSHOT_LABEL_CHARS
  );
}

function instructionTerms(instruction) {
  return (instruction.toLowerCase().match(/[\p{L}\p{N}]+/gu) || [])
    .filter((term) => term.length > 1);
}

function localCandidateScore(element, label, terms) {
  const role = (element.getAttribute("role") || element.localName).toLowerCase();
  let score = 0;
  for (const term of terms) {
    if (label.toLowerCase().includes(term)) score += 10;
    if (role.includes(term)) score += 2;
  }
  const requestsTextInput = terms.some((term) => ["input", "message", "prompt", "type", "write"].includes(term));
  if (requestsTextInput && (element.matches("textarea, input") || role.includes("textbox"))) score += 8;
  return score;
}

function collectInteractiveElements() {
  const found = new Set(document.querySelectorAll(EXPLICIT_INTERACTIVE_SELECTOR));
  // Many app UIs (Canva tiles, dashboards) are clickable <div>/<span> with no
  // role. Add a bounded sweep for elements whose computed cursor is a pointer,
  // skipping tiny nodes, page-sized wrappers, and containers of a control we
  // already have. Caps keep this affordable on large pages.
  const all = document.body ? document.body.getElementsByTagName("*") : [];
  const viewportArea = Math.max(1, window.innerWidth * window.innerHeight);
  let added = 0;
  for (let index = 0; index < all.length && index < MAX_POINTER_SCAN && added < MAX_POINTER_CANDIDATES; index += 1) {
    const element = all[index];
    if (found.has(element)) continue;
    const rect = element.getBoundingClientRect();
    if (rect.width < 12 || rect.height < 12) continue;
    if (rect.width * rect.height > viewportArea * 0.8) continue;
    if ((element.innerText || "").trim().length > MAX_POINTER_TEXT_CHARS) continue;
    if (window.getComputedStyle(element).cursor !== "pointer") continue;
    if (element.querySelector(EXPLICIT_INTERACTIVE_SELECTOR)) continue;
    found.add(element);
    added += 1;
  }
  return [...found];
}

function interactiveElements(instruction = "") {
  try {
    return interactiveElementsUnsafe(instruction);
  } catch {
    // Never let a DOM edge case throw: a throw at import time (pageSignature
    // below) would abort the whole script before its message listener registers,
    // which surfaces as "Receiving end does not exist" on every request.
    return [];
  }
}

function interactiveElementsUnsafe(instruction = "") {
  const terms = instructionTerms(instruction);
  return collectInteractiveElements()
    .filter(visible)
    .map((element, index) => ({
      element,
      index,
      label: elementLabel(element)
    }))
    .sort((left, right) => {
      const scoreDifference = localCandidateScore(right.element, right.label, terms)
        - localCandidateScore(left.element, left.label, terms);
      return scoreDifference || left.index - right.index;
    })
    .slice(0, MAX_RANKING_ELEMENTS)
    .slice(0, MAX_SNAPSHOT_ELEMENTS)
    .map(({ element, label }) => ({
        selector: selectorFor(element),
        role: element.getAttribute("role") || element.tagName.toLowerCase(),
        label
    }))
    .filter((element) => element.selector.length <= 1024);
}

function publishDomSnapshot() {
  window.dispatchEvent(new CustomEvent("mice.guide.dom", {
    detail: { url: location.href, elements: interactiveElements() }
  }));
}

function pageFingerprint() {
  return interactiveElements().slice(0, 24)
    .map((element) => `${element.selector}\u0000${element.label}`)
    .join("\n");
}
let pageSignature = `${location.href}\n${document.title}\n${pageFingerprint()}`;
let pageChangeTimer;
function publishPageChange() {
  const next = `${location.href}\n${document.title}\n${pageFingerprint()}`;
  if (next === pageSignature) return;
  pageSignature = next;
  chrome.runtime.sendMessage({ type: "mice.page.changed", url: location.href, title: document.title }).catch(() => {});
}
function schedulePageChange() {
  clearTimeout(pageChangeTimer);
  pageChangeTimer = setTimeout(publishPageChange, 250);
}
for (const method of ["pushState", "replaceState"]) {
  const original = history[method];
  history[method] = function(...args) { const result = original.apply(this, args); schedulePageChange(); return result; };
}
addEventListener("popstate", schedulePageChange);
new MutationObserver(schedulePageChange).observe(document.documentElement, { childList: true, subtree: true });

function highlight(selector, instructionText = "") {
  const target = document.querySelector(selector);
  if (!target) return { ok: false, error: `No element matches ${selector}` };
  target.scrollIntoView({ block: "center", inline: "center", behavior: "auto" });
  const previous = document.getElementById(MICE_HIGHLIGHT_ID);
  previous?.remove();
  const rect = target.getBoundingClientRect();
  const overlay = document.createElement("div");
  overlay.id = MICE_HIGHLIGHT_ID;
  overlay.textContent = instructionText;
  Object.assign(overlay.style, {
    position: "fixed", left: `${rect.left - 4}px`, top: `${rect.top - 4}px`,
    width: `${rect.width + 8}px`, height: `${rect.height + 8}px`, zIndex: "2147483647",
    border: "3px solid #00d4ff", borderRadius: "6px", pointerEvents: "none",
    color: "#001820", background: "rgba(0, 212, 255, 0.15)", font: "600 12px system-ui"
  });
  document.documentElement.append(overlay);
  return { ok: true };
}

function act(action, selector, value = "") {
  if (action === "scroll") {
    window.scrollBy({ top: Math.max(320, Math.floor(window.innerHeight * 0.75)), behavior: "smooth" });
    return { ok: true, pageChanged: false };
  }
  const target = document.querySelector(selector);
  if (!target || !visible(target)) return { ok: false, error: "Target is no longer visible." };
  const text = [target.getAttribute("aria-label"), target.getAttribute("name"), target.getAttribute("id"), target.getAttribute("autocomplete"), target.getAttribute("type"), target.textContent, target.value]
    .filter(Boolean).join(" ").toLowerCase();
  const hasSensitiveForm = Boolean(target.closest("form")?.querySelector("input[type='password'], input[autocomplete^='cc-'], input[autocomplete='one-time-code']"));
  const sensitiveFill = target instanceof HTMLInputElement && (
    target.type === "password" || target.autocomplete.startsWith("cc-") || target.autocomplete === "one-time-code"
    || /password|passcode|otp|one[- ]time|verification code|cvv|cvc|card number|routing number|account number/.test(text)
  );
  const sensitiveClick = /pay|purchase|place order|confirm payment|file return|submit return|transfer|sign in|log in|login/.test(text)
    || ((target instanceof HTMLButtonElement || target instanceof HTMLInputElement) && (target.type === "submit" || target.type === "image") && hasSensitiveForm);
  if (action === "fill" && sensitiveFill) return { ok: false, error: "MICE will not fill credentials, one-time codes, or payment fields." };
  if (action === "click" && sensitiveClick) return { ok: false, error: "MICE will not click authentication, payment, transfer, or final-submission controls." };
  target.scrollIntoView({ block: "center", inline: "center", behavior: "auto" });
  if (action === "click") {
    const beforeUrl = location.href;
    target.click();
    // Report a page change only when the URL actually changed. Same-page
    // interactions (menus, dropdowns, toggles like Canva's "Create a design"
    // panel) keep the URL, so the agent must re-observe the new state
    // immediately instead of waiting for a navigation event that never comes.
    return { ok: true, pageChanged: location.href !== beforeUrl };
  }
  if (action === "fill" && (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement)) {
    const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(target), "value")?.set;
    if (!setter) return { ok: false, error: "Target does not accept text." };
    setter.call(target, value);
    target.dispatchEvent(new Event("input", { bubbles: true }));
    target.dispatchEvent(new Event("change", { bubbles: true }));
    return { ok: true, pageChanged: false };
  }
  return { ok: false, error: "Unsupported browser action." };
}

// Single-page apps keep rendering after load, so an immediate snapshot can miss
// the real UI (e.g. Canva shows only skip-links for the first second). Wait until
// the DOM has been quiet briefly, or until a hard cap, before snapshotting — and
// bail out of the wait early once enough real controls exist.
function explicitControlCount() {
  try {
    return document.querySelectorAll(EXPLICIT_INTERACTIVE_SELECTOR).length;
  } catch {
    return 0;
  }
}

function waitForSettle(quietMs = 400, maxWaitMs = 2000, enoughControls = 12) {
  return new Promise((resolve) => {
    let quietTimer;
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      observer.disconnect();
      clearTimeout(quietTimer);
      clearTimeout(hardCap);
      resolve();
    };
    const armQuiet = () => {
      clearTimeout(quietTimer);
      quietTimer = setTimeout(finish, quietMs);
    };
    // Cheap control count (no computed-style sweep) is a good proxy for "the
    // app has painted its UI"; bail out of the wait as soon as it is met.
    const observer = new MutationObserver(() => {
      if (explicitControlCount() >= enoughControls) return finish();
      armQuiet();
    });
    observer.observe(document.documentElement, { childList: true, subtree: true });
    if (explicitControlCount() >= enoughControls) return finish();
    armQuiet();
    const hardCap = setTimeout(finish, maxWaitMs);
  });
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "mice.guide.snapshot") {
    (async () => {
      await waitForSettle();
      sendResponse({ url: location.href, elements: interactiveElements(message.instruction || "") });
    })();
    return true;
  } else if (message?.type === "mice.guide.highlight") {
    sendResponse(highlight(message.selector, message.instructionText));
  } else if (message?.type === "mice.browser.act") {
    sendResponse(act(message.action, message.selector, message.value));
  } else {
    return undefined;
  }
  return true;
});

// A full navigation replaces this document before its MutationObserver can
// report a delta. Announce the freshly ready page so an in-flight autopilot
// action always receives a new snapshot after cross-origin navigation.
chrome.runtime.sendMessage({ type: "mice.page.ready", url: location.href, title: document.title }).catch(() => {});
publishDomSnapshot();
