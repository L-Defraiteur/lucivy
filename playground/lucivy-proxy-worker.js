// CORS proxy for the playground's "clone a GitHub repository": the tarball
// endpoint of the GitHub API redirects to codeload.github.com, which sends
// no Access-Control-Allow-Origin, so a browser cannot fetch it directly.
//
// Deployed as a Cloudflare Worker (lucivy-proxy.luciedefraiteur.workers.dev).
// It relays exactly one thing — `GET /repos/<owner>/<repo>/tarball[/<ref>]`
// on api.github.com — and only for the playground's own origins: it is not
// a general relay to the GitHub API, and another website cannot spend the
// anonymous rate limit (60 requests an hour, shared by every visitor
// through Cloudflare's egress) that the demo depends on. A user's token
// (Authorization) is forwarded, never stored.
//
// Not enforceable here: a script outside a browser can forge Origin. The
// last line of defence is a Cloudflare rate-limiting rule on this Worker.

const ALLOWED_ORIGINS = [
  /^https:\/\/l-defraiteur\.github\.io$/,
  /^http:\/\/localhost(:\d+)?$/,
  /^http:\/\/127\.0\.0\.1(:\d+)?$/,
];
const ALLOWED_TARGET = /^https:\/\/api\.github\.com\/repos\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+\/tarball(\/[^/?#]+)?$/;

function originAllowed(origin) {
  return !!origin && ALLOWED_ORIGINS.some(re => re.test(origin));
}

// Headers the page may read: the size (a real download bar) and GitHub's
// quota (a clear message when it is exhausted, with the minute it resets).
const EXPOSED = ['Content-Length', 'X-RateLimit-Limit', 'X-RateLimit-Remaining', 'X-RateLimit-Reset'];

function cors(origin, extra = {}) {
  return {
    'Access-Control-Allow-Origin': origin,
    'Access-Control-Allow-Headers': 'Authorization',
    'Access-Control-Allow-Methods': 'GET, OPTIONS',
    'Access-Control-Expose-Headers': EXPOSED.join(', ') + ', X-Lucivy-Cache',
    'Vary': 'Origin',
    ...extra,
  };
}

function passthrough(resp) {
  const h = { 'Content-Type': resp.headers.get('Content-Type') || 'application/octet-stream' };
  for (const name of EXPOSED) {
    const v = resp.headers.get(name);
    if (v) h[name] = v;
  }
  return h;
}

// Anonymous tarballs are cached for half an hour: the demo's clone of
// lucivy costs GitHub two requests an hour instead of one per visitor, out
// of the 60 an hour the anonymous quota allows through this Worker's IPs.
// A response fetched with a token is never cached — it may be a private
// repository, and the next visitor must not receive it.
// (On a bare workers.dev address the Cache API can be a no-op; it works on
// a custom domain. Harmless either way.)
const CACHE_SECONDS = 1800;

export default {
  async fetch(request) {
    const origin = request.headers.get('Origin');
    if (!originAllowed(origin)) {
      return new Response('Forbidden: origin', { status: 403 });
    }
    if (request.method === 'OPTIONS') {
      return new Response(null, { headers: cors(origin) });
    }
    if (request.method !== 'GET') {
      return new Response('Forbidden: method', { status: 403 });
    }
    const url = new URL(request.url);
    let target;
    try { target = decodeURIComponent(url.pathname.slice(1)); }
    catch { return new Response('Forbidden: target', { status: 403 }); }
    if (!ALLOWED_TARGET.test(target)) {
      return new Response('Forbidden: target', { status: 403 });
    }
    const headers = { 'User-Agent': 'lucivy-playground' };
    const auth = request.headers.get('Authorization');
    if (auth) headers['Authorization'] = auth;

    const cacheable = !auth;
    const cache = caches.default;
    const key = new Request(target, { method: 'GET' });
    if (cacheable) {
      const hit = await cache.match(key).catch(() => null);
      if (hit) {
        return new Response(hit.body, {
          status: 200,
          headers: cors(origin, { ...passthrough(hit), 'X-Lucivy-Cache': 'hit' }),
        });
      }
    }

    const resp = await fetch(target, { headers, redirect: 'follow' });
    if (cacheable && resp.ok) {
      // Buffer once: the body goes to the visitor and to the cache, and a
      // stream can only be read once. Tarballs are tens of megabytes at most.
      const bytes = await resp.arrayBuffer();
      const stored = new Response(bytes, {
        headers: { ...passthrough(resp), 'Content-Length': String(bytes.byteLength), 'Cache-Control': `public, max-age=${CACHE_SECONDS}` },
      });
      await cache.put(key, stored.clone()).catch(() => {});
      return new Response(bytes, {
        status: 200,
        headers: cors(origin, { ...passthrough(resp), 'Content-Length': String(bytes.byteLength), 'X-Lucivy-Cache': 'miss' }),
      });
    }
    return new Response(resp.body, {
      status: resp.status,
      headers: cors(origin, passthrough(resp)),
    });
  }
};
