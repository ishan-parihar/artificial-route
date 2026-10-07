// peer-stamp-launcher.cjs — restores the peer-stamp runtime contract the
// compiled dashboard expects before handing control to server.js.
//
// OmniRoute's authz layer never trusts the client-controlled Host header
// (GHSA-7pq4-8pvv-rx7r): a request counts as "local" only when the custom
// Node server stamps the real TCP peer into `x-omniroute-peer-ip` with a
// per-process token. OmniRoute's own launchers (scripts/dev/peer-stamp.mjs,
// wired into run-next.mjs / standalone-server-ws.mjs) do this; the bare Next
// standalone `server.js` does not. Without the stamp every request —
// including a 127.0.0.1 browser — fails closed to "remote", so onboarding's
// first-password write 401s and the wizard demands the one-time bootstrap
// token from the log ("This connection isn't recognized as local").
//
// This launcher reproduces the stamping exactly: mint the per-process token,
// stamp every request on every http.Server this process creates, then start
// Next via server.js unchanged.

"use strict";

const http = require("node:http");
const { randomUUID } = require("node:crypto");

// Must match src/server/authz/headers.ts in the compiled app.
const PEER_IP_HEADER = "x-omniroute-peer-ip";
const VIA_PROXY_HEADER = "x-omniroute-via-proxy";
const CLIENT_IP_HEADER = "x-omniroute-client-ip";

// Same form as peer-stamp.mjs: env so the middleware running in this same
// process reads the identical value.
function ensurePeerStampToken() {
  process.env.OMNIROUTE_PEER_STAMP_TOKEN ||= randomUUID();
  return process.env.OMNIROUTE_PEER_STAMP_TOKEN;
}

// Same trusted-proxy-peer set as peer-stamp.mjs's PRIVATE_LAN_PATTERNS plus
// loopback. The dashboard binds loopback only (`aroute dashboard` sets
// HOSTNAME=127.0.0.1), so a peer is either the local operator or a local
// reverse proxy — Cloudflare edges and operator-configured proxies can never
// appear on this socket, which is why those branches are not reproduced.
function isPrivateLanIp(ip) {
  return (
    /^10\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(ip) ||
    /^100\.(6[4-9]|[78]\d|9\d|1[01]\d|12[0-7])\.\d{1,3}\.\d{1,3}$/.test(ip) ||
    /^192\.168\.\d{1,3}\.\d{1,3}$/.test(ip) ||
    /^172\.(1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}$/.test(ip) ||
    /^f[cd][0-9a-f]{2}:/i.test(ip) ||
    /^fe80:/i.test(ip)
  );
}

function isLoopbackIp(ip) {
  return ip === "127.0.0.1" || ip === "::1" || ip === "::ffff:127.0.0.1";
}

function isTrustedProxyPeer(ip) {
  return isLoopbackIp(ip) || isPrivateLanIp(ip);
}

// Forwarding headers mean the loopback/private socket is a proxy hop, not the
// end user — the locality verdict must downgrade to "remote" so the authz
// gates are not bypassed through an external reverse proxy.
function hasProxyHopHeader(headers) {
  for (const rawName of Object.keys(headers)) {
    const name = rawName.toLowerCase();
    if (
      name.startsWith("x-forwarded-") ||
      name === "x-real-ip" ||
      name === "forwarded" ||
      name === "via"
    ) {
      return true;
    }
  }
  return false;
}

// A plain IP from a header value, tolerating `::ffff:` and `:port` forms.
function normalizeIp(value) {
  if (typeof value !== "string") return null;
  let candidate = value.trim().replace(/^::ffff:/i, "");
  const withPort = /^(\d{1,3}(?:\.\d{1,3}){3}):\d+$/.exec(candidate);
  if (withPort) candidate = withPort[1];
  return candidate || null;
}

// Walk the x-forwarded-for chain from the hop nearest this proxy outward,
// skipping further trusted proxies, falling back to x-real-ip, then the peer.
function resolveClientIp(headers, peer) {
  const chain = String(headers["x-forwarded-for"] || "").split(",");
  for (let index = chain.length - 1; index >= 0; index -= 1) {
    const hop = normalizeIp(chain[index]);
    if (!hop) break;
    if (!isTrustedProxyPeer(hop)) return hop;
  }
  const realIp = normalizeIp(String(headers["x-real-ip"] || "").split(",")[0]);
  return realIp || peer;
}

// Strip client-supplied stamps, then stamp the real TCP peer and the
// via-proxy marker. Never throws — a stamping failure must degrade to
// "locality unknown", which fails closed in the middleware exactly like no
// stamp at all.
function stampPeerIp(req) {
  try {
    if (!req || !req.headers) return;
    delete req.headers[PEER_IP_HEADER];
    delete req.headers[VIA_PROXY_HEADER];
    delete req.headers[CLIENT_IP_HEADER];
    const ip = req.socket && req.socket.remoteAddress;
    if (!ip) return;
    const token = ensurePeerStampToken();
    req.headers[PEER_IP_HEADER] = token + "|" + ip;
    const viaProxy = hasProxyHopHeader(req.headers) && isTrustedProxyPeer(ip);
    req.headers[VIA_PROXY_HEADER] = token + "|" + (viaProxy ? "1" : "0");
    const clientIp = viaProxy ? resolveClientIp(req.headers, ip) : ip;
    req.headers[CLIENT_IP_HEADER] = token + "|" + clientIp;
  } catch {
    /* never block a request on peer stamping */
  }
}

// Stamp on the 'request' emit so every http.Server this process creates is
// covered — Next's server and the embedded services alike — regardless of
// how (or whether) a request listener was attached at construction.
const originalEmit = http.Server.prototype.emit;
http.Server.prototype.emit = function peerStampingEmit(event, req) {
  if (event === "request") stampPeerIp(req);
  return originalEmit.apply(this, arguments);
};

ensurePeerStampToken();
require("./server.js");

// Warm the /v1/models catalog cache once, shortly after boot. The catalog
// builder walks every provider registry + SQLite and takes ~3 s on a cold
// call; memoized thereafter. Without this the first page load that asks for
// the model list (provider topology, combo editors, …) eats the whole 3 s.
// Fire-and-forget: a failed warm must never take the server down.
(function warmModelsCatalog() {
  const port = process.env.PORT || "3000";
  const host = process.env.HOSTNAME || "127.0.0.1";
  let attempts = 0;
  const tick = () => {
    attempts += 1;
    const req = http.get(
      { host: host === "0.0.0.0" ? "127.0.0.1" : host, port, path: "/v1/models", timeout: 30000 },
      (res) => res.resume()
    );
    req.on("timeout", () => req.destroy());
    req.on("error", () => {
      // Server not listening yet — retry for up to ~30 s, then give up.
      if (attempts < 15) setTimeout(tick, 2000);
    });
  };
  setTimeout(tick, 2000);
})();
