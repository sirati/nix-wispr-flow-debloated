const { ipcRenderer } = require("electron");
for (const ch of ["notification:show", "media:startMeetingMicCapture", "test:ping", "status:dictationStatus"]) {
  ipcRenderer.on(ch, (_e, a) => ipcRenderer.send("test:got", { role: "status", ch }));
}
window.addEventListener("DOMContentLoaded", () => {
  ipcRenderer.send("test:loaded", { role: "status" });
  if ("status" === "status") ipcRenderer.send("status:reportAlphaCatchRects", { rects: [{ x: 0, y: 0, width: 300, height: 80 }], shapeRects: [], viewport: {} });
});
