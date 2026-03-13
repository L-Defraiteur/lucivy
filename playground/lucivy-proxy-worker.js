export default {
  async fetch(request) {
    if (request.method === 'OPTIONS') {
      return new Response(null, {
        headers: {
          'Access-Control-Allow-Origin': '*',
          'Access-Control-Allow-Headers': 'Authorization',
        },
      });
    }

    const url = new URL(request.url);
    const target = decodeURIComponent(url.pathname.slice(1));
    if (!target.startsWith('https://api.github.com/')) {
      return new Response('Forbidden', { status: 403 });
    }

    const headers = { 'User-Agent': 'lucivy-playground' };
    const auth = request.headers.get('Authorization');
    if (auth) headers['Authorization'] = auth;

    const resp = await fetch(target, { headers, redirect: 'follow' });

    return new Response(resp.body, {
      status: resp.status,
      headers: {
        'Access-Control-Allow-Origin': '*',
        'Content-Type': resp.headers.get('Content-Type') || 'application/octet-stream',
      },
    });
  }
};
