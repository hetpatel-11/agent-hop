// Minimal self-hosted telemetry ingest for agent-hop.
//
// Accepts the payload emitted by src/telemetry.rs::flush() and writes each
// event as a row into a Cloudflare D1 table. Deploy with `wrangler deploy`
// and point AH_TELEMETRY_ENDPOINT (or DEFAULT_ENDPOINT in telemetry.rs) at it.
//
// Deliberately minimal and privacy-preserving:
//   - Never logs or stores the client IP. We ask Cloudflare for the country
//     only (coarse), and drop the IP entirely.
//   - No cookies, no cross-origin state, no third parties.
//
// One-time setup:
//   wrangler d1 create agent-hop-telemetry
//   wrangler d1 execute agent-hop-telemetry --command "$(cat schema.sql)"
//   # bind the DB as `DB` in wrangler.toml, then: wrangler deploy

export default {
  async fetch(request, env) {
    if (request.method !== "POST") {
      return new Response("ok", { status: 200 });
    }

    let body;
    try {
      body = await request.json();
    } catch {
      return new Response("bad json", { status: 400 });
    }

    const events = Array.isArray(body?.events) ? body.events : [];
    if (events.length === 0) {
      return new Response("no events", { status: 204 });
    }

    // Coarse geo only; the raw IP is never read or stored.
    const country = request.cf?.country ?? null;
    const receivedAt = new Date().toISOString();

    const stmt = env.DB.prepare(
      `INSERT INTO events
         (device_id, session_id, app_version, os, arch, country, event, event_time, props, received_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
    );

    const rows = events.map((e) => {
      const { event, time, ...props } = e ?? {};
      return stmt.bind(
        str(body.device_id),
        str(body.session_id),
        str(body.app_version),
        str(body.os),
        str(body.arch),
        country,
        str(event),
        str(time),
        JSON.stringify(props),
        receivedAt
      );
    });

    try {
      await env.DB.batch(rows);
    } catch {
      // Swallow: a failed insert must never signal anything useful back to
      // the client, and the client ignores the response anyway.
      return new Response("", { status: 204 });
    }

    return new Response("", { status: 204 });
  },
};

function str(v) {
  return typeof v === "string" ? v.slice(0, 512) : v == null ? null : String(v).slice(0, 512);
}
