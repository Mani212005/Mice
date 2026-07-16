chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type !== "mice.guide.highlight") {
    return undefined;
  }

  chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
    const tab = tabs[0];
    if (!tab?.id) {
      sendResponse({ ok: false, error: "No active browser tab." });
      return;
    }
    chrome.tabs.sendMessage(tab.id, message, () => {
      if (chrome.runtime.lastError) {
        sendResponse({ ok: false, error: chrome.runtime.lastError.message });
      } else {
        sendResponse({ ok: true });
      }
    });
  });
  return true;
});
