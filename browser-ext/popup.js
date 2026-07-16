const status = document.getElementById("status");
document.getElementById("guide").addEventListener("click", async () => {
  const instruction = document.getElementById("instruction").value.trim();
  if (!instruction) return;
  const { bridgeToken = "" } = await chrome.storage.local.get("bridgeToken");
  if (!bridgeToken) { status.textContent = "Set the bridge token in extension options."; return; }
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const snapshot = await chrome.tabs.sendMessage(tab.id, {
    type: "mice.guide.snapshot",
    instruction
  });
  const response = await fetch("http://127.0.0.1:9417/guide", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Mice-Token": bridgeToken },
    body: JSON.stringify({ instruction, ...snapshot })
  });
  const result = await response.json();
  if (!response.ok) { status.textContent = result.error || "Guide request failed."; return; }
  const highlight = await chrome.tabs.sendMessage(tab.id, {
    type: "mice.guide.highlight",
    selector: result.selector,
    instructionText: result.instructionText
  });
  status.textContent = highlight?.ok ? result.instructionText : (highlight?.error || "Could not highlight that element.");
});
