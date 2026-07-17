let port;
let coreMessageQueue = Promise.resolve();
let reconnectTimer;
// The one tab MICE drives for the active goal. Pinning it keeps snapshots,
// actions, screenshots, and page-change events on a single consistent tab, even
// when the user has other tabs open or a background tab finishes loading.
let goalTabId = null;
const MAX_NATIVE_MESSAGE_DATA_URL_CHARS = 900_000;

function blobDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error);
    reader.onloadend = () => resolve(reader.result);
    reader.readAsDataURL(blob);
  });
}

async function compactScreenshot(dataUrl) {
  if (dataUrl.length <= MAX_NATIVE_MESSAGE_DATA_URL_CHARS) return dataUrl;
  const bitmap = await createImageBitmap(await (await fetch(dataUrl)).blob());
  const scale = Math.min(1, 1024 / Math.max(bitmap.width, bitmap.height));
  const canvas = new OffscreenCanvas(
    Math.max(1, Math.round(bitmap.width * scale)),
    Math.max(1, Math.round(bitmap.height * scale))
  );
  canvas.getContext("2d").drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  bitmap.close();
  let compact = await blobDataUrl(await canvas.convertToBlob({ type: "image/jpeg", quality: 0.4 }));
  if (compact.length > MAX_NATIVE_MESSAGE_DATA_URL_CHARS) {
    compact = await blobDataUrl(await canvas.convertToBlob({ type: "image/jpeg", quality: 0.22 }));
  }
  return compact.length <= MAX_NATIVE_MESSAGE_DATA_URL_CHARS ? compact : null;
}

// Resolve once the tab has finished loading (or a timeout), so a re-observation
// after navigation snapshots a rendered page rather than a blank/loading one.
function waitForTabComplete(tabId, timeoutMs = 15000) {
  return new Promise((resolve) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      chrome.tabs.onUpdated.removeListener(listener);
      clearTimeout(timer);
      resolve();
    };
    const listener = (id, info) => {
      if (id === tabId && info.status === "complete") finish();
    };
    chrome.tabs.onUpdated.addListener(listener);
    chrome.tabs.get(tabId).then((tab) => { if (tab?.status === "complete") finish(); }).catch(finish);
    const timer = setTimeout(finish, timeoutMs);
  });
}

// Send a message to a tab's content script, retrying briefly while it is still
// loading. Right after a navigation the page can report "complete" before the
// content script has registered its listener ("Receiving end does not exist").
async function sendToTab(tabId, message, attempts = 4, delayMs = 250) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await chrome.tabs.sendMessage(tabId, message);
    } catch (error) {
      if (attempt === attempts - 1) throw error;
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
}

// Resolve the tab MICE should drive: the pinned goal tab if it still exists,
// otherwise the current active tab (and adopt it as the pin).
async function workingTab() {
  if (goalTabId != null) {
    try {
      return await chrome.tabs.get(goalTabId);
    } catch {
      goalTabId = null;
    }
  }
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab || null;
}

function setBadge(text, color) {
  chrome.action.setBadgeText({ text });
  chrome.action.setBadgeBackgroundColor({ color });
}

function connect() {
  port = chrome.runtime.connectNative("com.mice.bridge");
  port.onMessage.addListener((message) => {
    coreMessageQueue = coreMessageQueue
      .then(() => handleCoreMessage(message))
      .catch((error) => console.warn("MICE core message failed", error));
  });
  port.onDisconnect.addListener(() => {
    // Reading lastError acknowledges expected disconnects (for example while
    // MICE is not running) so Chrome does not emit an "Unchecked" error on
    // every reconnect attempt.
    void chrome.runtime.lastError;
    setBadge("!", "#8b1e1e");
    clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(connect, 1500);
  });
  port.postMessage({ type: "bridge.hello" });
  setBadge("", "#00a8a8");
}

async function handleCoreMessage(message) {
  if (message?.type === "browser.highlight") {
    const tab = await workingTab();
    if (tab?.id != null) chrome.tabs.sendMessage(tab.id, {
      type: "mice.guide.highlight", selector: message.selector, instructionText: message.instructionText
    }).catch(() => {});
    return;
  }
  if (message?.type === "browser.screenshot") {
    try {
      const tab = await workingTab();
      const captured = await chrome.tabs.captureVisibleTab(tab?.windowId, { format: "jpeg", quality: 40 });
      const dataUrl = await compactScreenshot(captured);
      port?.postMessage({ type: "browser.screenshot", sessionId: message.sessionId, dataUrl });
    } catch (error) {
      port?.postMessage({ type: "browser.screenshot", sessionId: message.sessionId, error: String(error) });
    }
    return;
  }
  if (message?.type === "browser.act") {
    if (message.action === "open_url") {
      if (!/^https?:\/\//i.test(message.url || "")) {
        port?.postMessage({ type: "browser.actResult", sessionId: message.sessionId, ok: false, error: "Only http(s) URLs are allowed." });
        return;
      }
      // Navigate the pinned tab in place so MICE keeps observing one tab rather
      // than spawning a new one it then loses track of. Create only if none.
      const tab = await workingTab();
      if (tab?.id != null) {
        await chrome.tabs.update(tab.id, { url: message.url, active: true });
        goalTabId = tab.id;
        // Wait for the page to load so the follow-up re-observation sees a
        // rendered page instead of a blank/loading one.
        await waitForTabComplete(tab.id);
        port?.postMessage({ type: "browser.actResult", sessionId: message.sessionId, ok: true, pageChanged: true });
      } else {
        const created = await chrome.tabs.create({ url: message.url, active: true });
        goalTabId = created?.id ?? null;
        if (created?.id != null) await waitForTabComplete(created.id);
        port?.postMessage({ type: "browser.actResult", sessionId: message.sessionId, ok: Boolean(created?.id), pageChanged: true });
      }
      return;
    }
    const tab = await workingTab();
    if (tab?.id == null) {
      port?.postMessage({ type: "browser.actResult", sessionId: message.sessionId, ok: false, error: "No target tab." });
      return;
    }
    try {
      const result = await sendToTab(tab.id, {
        type: "mice.browser.act", action: message.action, selector: message.selector, value: message.value
      });
      port?.postMessage({ type: "browser.actResult", sessionId: message.sessionId, ...result });
    } catch (error) {
      // Content script not ready (mid-navigation): report a failure so the loop
      // re-observes instead of waiting for a result that will never arrive.
      port?.postMessage({ type: "browser.actResult", sessionId: message.sessionId, ok: false, error: String(error) });
    }
    return;
  }
  if (message?.type !== "goal.step") return;
  const directive = message.directive;
  if (!directive) {
    // The goal ended; release the pinned tab for the next run.
    goalTabId = null;
    return;
  }
  const tab = await workingTab();
  if (goalTabId == null && tab?.id != null) goalTabId = tab.id;
  // Content scripts cannot run on chrome://, the New Tab page, the Web Store,
  // PDFs, or view-source pages, so the snapshot will fail there. Rather than
  // stalling the loop, always report an observation: an empty candidate list
  // plus the tab URL lets the model choose open_url to reach a real page.
  let snapshot = { url: tab?.url || "about:blank", elements: [] };
  if (tab?.id != null) {
    try {
      const result = await sendToTab(tab.id, {
        type: "mice.guide.snapshot", instruction: directive.instruction
      });
      if (result && Array.isArray(result.elements)) snapshot = result;
    } catch {
      // Non-injectable tab (chrome://, PDF, …): keep the empty observation.
    }
  }
  port?.postMessage({ type: "goal.snapshot", ...directive, ...snapshot });
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message?.type === "mice.page.changed" || message?.type === "mice.page.ready") {
    // Only forward page changes from the tab MICE is driving.
    if (goalTabId == null || sender?.tab?.id === goalTabId) {
      port?.postMessage({ type: "browser.pageChanged", url: message.url, title: message.title });
    }
    return undefined;
  }
  if (message?.type !== "mice.guide.highlight") return undefined;
  workingTab().then((tab) => {
    if (tab?.id == null) return sendResponse({ ok: false, error: "No active browser tab." });
    chrome.tabs.sendMessage(tab.id, message, () => {
      sendResponse(chrome.runtime.lastError ? { ok: false, error: chrome.runtime.lastError.message } : { ok: true });
    });
  });
  return true;
});

chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (changeInfo.status !== "complete") return;
  if (goalTabId != null ? tabId === goalTabId : tab.active) {
    port?.postMessage({ type: "browser.pageChanged", url: tab.url, title: tab.title });
  }
});
chrome.tabs.onRemoved.addListener((tabId) => {
  if (tabId === goalTabId) goalTabId = null;
});
connect();
