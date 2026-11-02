// Service worker for the home-screen app: shows push notifications (sent by
// push.rs) and opens the session they're about. Nothing is cached; the page
// always comes live from Mandor.

self.addEventListener("push", (event) => {
  let msg = {};
  try {
    msg = event.data?.json() ?? {};
  } catch {}
  event.waitUntil(
    self.registration.showNotification(msg.title || "Mandor", {
      body: msg.body || "",
      icon: "icons/icon-192.png",
      // One notification per session: a newer alert replaces the older one.
      tag: msg.sessionId || undefined,
      data: { sessionId: msg.sessionId || null },
    }),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const id = event.notification.data?.sessionId;
  event.waitUntil(
    (async () => {
      const [client] = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      if (client) {
        await client.focus();
        if (id) client.postMessage({ type: "open-session", id });
        return;
      }
      // The worker was registered as sw.js?token=..., so its own query string
      // is what the page needs to get past the token check.
      const url = new URL("./", self.registration.scope);
      url.search = self.location.search;
      if (id) url.hash = "session=" + encodeURIComponent(id);
      await self.clients.openWindow(url.href);
    })(),
  );
});
