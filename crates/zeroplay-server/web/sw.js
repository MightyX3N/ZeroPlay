// ZeroPlay's service worker. It only makes the page installable as an app;
// nothing is cached, because everything ZeroPlay shows is live.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", () => {});
