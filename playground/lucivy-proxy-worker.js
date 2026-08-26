// CORS proxy for the playground's "clone a GitHub repository": the tarball
// endpoint of the GitHub API redirects to codeload.github.com, which sends
// no Access-Control-Allow-Origin, so a browser cannot fetch it directly.
// The demo itself does not go through here: its source is the tarball the
// Pages deploy builds next to the page (same origin, no quota).
//
// Deployed as a Cloudflare Worker (lucivy-proxy.luciedefraiteur.workers.dev).
// It relays exactly one thing — `GET /repos/<owner>/<repo>/tarball[/<ref>]`
// on api.github.com — and only for the playground's own origins: it is not
// a general relay to the GitHub API, and another website cannot spend the
// anonymous rate limit (60 requests an hour, shared by every visitor
// through Cloudflare's egress). A user's token (Authorization) is
// forwarded, never stored, and lifts that limit for them.
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
// The body is streamed through, never buffered: a Worker has 128 MB.
const EXPOSED = ['Content-Length', 'X-RateLimit-Limit', 'X-RateLimit-Remaining', 'X-RateLimit-Reset'];

function cors(origin, extra = {}) {
  return {
    'Access-Control-Allow-Origin': origin,
    'Access-Control-Allow-Headers': 'Authorization',
    'Access-Control-Allow-Methods': 'GET, OPTIONS',
    'Access-Control-Expose-Headers': EXPOSED.join(', '),
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
    const resp = await fetch(target, { headers, redirect: 'follow' });
    return new Response(resp.body, {
      status: resp.status,
      headers: cors(origin, passthrough(resp)),
    });
  }
};
