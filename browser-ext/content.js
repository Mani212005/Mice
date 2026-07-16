const MICE_HIGHLIGHT_ID = "mice-browser-guide-highlight";
const MAX_SNAPSHOT_ELEMENTS = 100;
const MAX_SNAPSHOT_LABEL_CHARS = 240;
const MAX_RANKING_ELEMENTS = 500;

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
  return Array.from(value.trim()).slice(0, maximumCharacters).join("");
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

function interactiveElements(instruction = "") {
  const terms = instructionTerms(instruction);
  return [...document.querySelectorAll("a,button,input,select,textarea,[role='button'],[role='link']")]
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

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "mice.guide.snapshot") {
    sendResponse({ url: location.href, elements: interactiveElements(message.instruction || "") });
  } else if (message?.type === "mice.guide.highlight") {
    sendResponse(highlight(message.selector, message.instructionText));
  } else {
    return undefined;
  }
  return true;
});

publishDomSnapshot();
