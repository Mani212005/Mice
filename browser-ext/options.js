const token = document.getElementById("token");
const status = document.getElementById("status");
chrome.storage.local.get("bridgeToken").then(({ bridgeToken = "" }) => { token.value = bridgeToken; });
document.getElementById("save").addEventListener("click", async () => {
  await chrome.storage.local.set({ bridgeToken: token.value.trim() });
  status.textContent = "Saved.";
});
