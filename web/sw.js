// Minimal offline shell so the app is installable and launches without a
// network round-trip. Live session traffic never touches the cache.
//
// Network-first, not stale-while-revalidate: this app's whole point is
// showing live connectivity state, so serving a knowingly-stale app.js just
// to save one round-trip is the wrong trade here. The cache only kicks in
// when there's genuinely no network. Bump CACHE whenever the shell's
// behavior changes in a way that matters even for a moment of staleness
// (like this comment doing exactly that).
const CACHE = 'rc-shell-v9';
const SHELL = [
  '.', 'index.html', 'style.css', 'app.js', 'manifest.webmanifest',
  'icon-192.png', 'icon-512.png', 'apple-touch-icon.png',
  'pkg/rc_web.js', 'pkg/rc_web_bg.wasm',
];

self.addEventListener('install', e => {
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', e => {
  e.waitUntil(
    caches.keys().then(ks => Promise.all(ks.filter(k => k !== CACHE).map(k => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== location.origin) return;
  e.respondWith(
    fetch(e.request)
      .then(res => {
        if (res.ok) caches.open(CACHE).then(c => c.put(e.request, res.clone()));
        return res;
      })
      .catch(() => caches.match(e.request))
  );
});
